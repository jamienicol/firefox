/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RemoteLayerTreeOwner.h"

#include "base/basictypes.h"
#include "mozilla/PresShell.h"
#include "mozilla/dom/BrowserParent.h"
#include "mozilla/dom/ContentParent.h"
#include "mozilla/dom/EffectsInfo.h"
#include "mozilla/gfx/GPUProcessManager.h"
#include "mozilla/layers/CompositorBridgeChild.h"
#include "mozilla/layers/CompositorBridgeParent.h"
#include "mozilla/layers/CompositorTypes.h"
#include "mozilla/layers/WebRenderLayerManager.h"
#include "mozilla/layers/WebRenderScrollData.h"
#include "mozilla/webrender/WebRenderAPI.h"
#include "nsError.h"
#include "nsFrameLoader.h"
#include "nsStyleStructInlines.h"
#include "nsSubDocumentFrame.h"
#include "nsThreadUtils.h"

using namespace mozilla::dom;
using namespace mozilla::gfx;
using namespace mozilla::layers;

namespace mozilla {
namespace layout {

static already_AddRefed<nsIWidget> GetWidget(BrowserParent* aBrowserParent) {
  RefPtr<nsIWidget> widget;
  if (Element* element = aBrowserParent->GetOwnerElement()) {
    widget = nsContentUtils::WidgetForContent(element);
    if (widget) {
      return widget.forget();
    }
    widget = nsContentUtils::WidgetForDocument(element->OwnerDoc());
    if (widget) {
      return widget.forget();
    }
  }
  return nullptr;
}

static already_AddRefed<WindowRenderer> GetWindowRenderer(
    BrowserParent* aBrowserParent) {
  RefPtr<WindowRenderer> renderer;
  if (RefPtr<nsIWidget> widget = GetWidget(aBrowserParent)) {
    renderer = widget->GetWindowRenderer();
    if (renderer) {
      return renderer.forget();
    }
  }
  return nullptr;
}

RemoteLayerTreeOwner::RemoteLayerTreeOwner()
    : mLayersId{0},
      mBrowserParent(nullptr),
      mInitializing(false),
      mInitialized(false),
      mLayersConnected(false) {}

RemoteLayerTreeOwner::~RemoteLayerTreeOwner() = default;

RefPtr<RemoteLayerTreeOwner::InitializePromise>
RemoteLayerTreeOwner::Initialize(BrowserParent* aBrowserParent) {
  if (!aBrowserParent) {
    return InitializePromise::CreateAndReject(NS_ERROR_INVALID_ARG, __func__);
  }

  if (mInitialized) {
    return InitializePromise::CreateAndResolve(true, __func__);
  }

  if (mInitializing) {
    return mInitializePromise.Ensure(__func__);
  }

  mBrowserParent = aBrowserParent;
  mTabProcessId = mBrowserParent->Manager()->OtherPid();
  mInitializing = true;

  RefPtr<InitializePromise> promise = mInitializePromise.Ensure(__func__);
  mInitializePromise.UseSynchronousTaskDispatch(__func__);

  RefPtr<nsIWidget> widget = GetWidget(mBrowserParent);
  if (!widget) {
    CompleteInitialize(nullptr);
    return promise.forget();
  }

  widget->GetWindowRendererAsync()
      ->Then(
          GetCurrentSerialEventTarget(), __func__,
          [self = this](const RefPtr<WindowRenderer>& aRenderer) {
            self->mInitializeWindowRendererRequest.Complete();
            self->CompleteInitialize(aRenderer);
          },
          [self = this](Ok) {
            self->mInitializeWindowRendererRequest.Complete();
            self->CompleteInitialize(nullptr);
          })
      ->Track(mInitializeWindowRendererRequest);

  return promise.forget();
}

void RemoteLayerTreeOwner::CompleteInitialize(WindowRenderer* aRenderer) {
  MOZ_ASSERT(mInitializing);
  MOZ_ASSERT(!mInitialized);

  PCompositorBridgeChild* compositor =
      aRenderer ? aRenderer->GetCompositorBridgeChild() : nullptr;

  // Our remote frame will push layers updates to the compositor,
  // and we'll keep an indirect reference to that tree.
  GPUProcessManager* gpm = GPUProcessManager::Get();
  mLayersConnected = gpm->AllocateAndConnectLayerTreeId(
      compositor, mTabProcessId, &mLayersId, &mCompositorOptions);

  mInitializing = false;
  mInitialized = true;
  mInitializePromise.Resolve(true, __func__);
}

void RemoteLayerTreeOwner::EnsureInitialized() {
  if (!mInitializing) {
    return;
  }

  mInitializeWindowRendererRequest.DisconnectIfExists();
  RefPtr<WindowRenderer> renderer =
      mBrowserParent ? GetWindowRenderer(mBrowserParent) : nullptr;
  CompleteInitialize(renderer);
}

void RemoteLayerTreeOwner::Destroy() {
  mInitializeWindowRendererRequest.DisconnectIfExists();
  if (mInitializing) {
    mInitializing = false;
  }
  mInitializePromise.RejectIfExists(NS_ERROR_ABORT, __func__);

  if (mLayersId.IsValid()) {
    GPUProcessManager::Get()->UnmapLayerTreeId(mLayersId, mTabProcessId);
  }

  mBrowserParent = nullptr;
  mWindowRenderer = nullptr;
}

void RemoteLayerTreeOwner::EnsureLayersConnected(
    Maybe<CompositorOptions>& aCompositorOptions) {
  EnsureInitialized();
  RefPtr<WindowRenderer> renderer = GetWindowRenderer(mBrowserParent);
  if (!renderer || !renderer->GetCompositorBridgeChild()) {
    aCompositorOptions = Nothing();
    return;
  }

  mLayersConnected =
      renderer->GetCompositorBridgeChild()->SendNotifyChildRecreated(
          mLayersId, &mCompositorOptions);
  aCompositorOptions = Some(mCompositorOptions);
}

bool RemoteLayerTreeOwner::AttachWindowRenderer() {
  EnsureInitialized();
  RefPtr<WindowRenderer> renderer;
  if (mBrowserParent) {
    renderer = GetWindowRenderer(mBrowserParent);
  }

  // Perhaps the document containing this frame currently has no presentation?
  if (renderer && renderer->GetCompositorBridgeChild() &&
      renderer != mWindowRenderer) {
    mLayersConnected =
        renderer->GetCompositorBridgeChild()->SendAdoptChild(mLayersId);
  }

  mWindowRenderer = std::move(renderer);
  return !!mWindowRenderer;
}

void RemoteLayerTreeOwner::OwnerContentChanged() {
  (void)AttachWindowRenderer();
}

void RemoteLayerTreeOwner::GetTextureFactoryIdentifier(
    TextureFactoryIdentifier* aTextureFactoryIdentifier) const {
  const_cast<RemoteLayerTreeOwner*>(this)->EnsureInitialized();
  RefPtr<WindowRenderer> renderer =
      mBrowserParent ? GetWindowRenderer(mBrowserParent) : nullptr;
  // Perhaps the document containing this frame currently has no presentation?
  if (renderer && renderer->AsWebRender()) {
    *aTextureFactoryIdentifier =
        renderer->AsWebRender()->GetTextureFactoryIdentifier();
  } else {
    *aTextureFactoryIdentifier = TextureFactoryIdentifier();
  }
}

}  // namespace layout
}  // namespace mozilla
