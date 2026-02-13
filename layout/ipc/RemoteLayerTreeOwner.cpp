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
#include "nsFrameLoader.h"
#include "nsStyleStructInlines.h"
#include "nsSubDocumentFrame.h"

using namespace mozilla::dom;
using namespace mozilla::gfx;
using namespace mozilla::layers;

namespace mozilla {
namespace layout {

static already_AddRefed<WindowRenderer> GetWindowRenderer(
    BrowserParent* aBrowserParent) {
  RefPtr<WindowRenderer> renderer;
  if (Element* element = aBrowserParent->GetOwnerElement()) {
    renderer = nsContentUtils::WindowRendererForContent(element);
    if (renderer) {
      return renderer.forget();
    }
    renderer = nsContentUtils::WindowRendererForDocument(element->OwnerDoc());
    if (renderer) {
      return renderer.forget();
    }
  }
  return nullptr;
}

RemoteLayerTreeOwner::RemoteLayerTreeOwner()
    : mLayersId{0},
      mBrowserParent(nullptr),
      mInitialized(false),
      mLayersConnected(false) {}

RemoteLayerTreeOwner::~RemoteLayerTreeOwner() = default;

RefPtr<RemoteLayerTreeOwner::InitPromise> RemoteLayerTreeOwner::Initialize(
    BrowserParent* aBrowserParent) {
  if (mInitialized || !aBrowserParent) {
    return InitPromise::CreateAndReject(nsresult::NS_ERROR_FAILURE, __func__);
  }

  mInitialized = true;
  mBrowserParent = aBrowserParent;
  RefPtr<WindowRenderer> renderer = GetWindowRenderer(mBrowserParent);
  PCompositorBridgeChild* compositor =
      renderer ? renderer->GetCompositorBridgeChild() : nullptr;
  mTabProcessId = mBrowserParent->Manager()->OtherPid();

  auto promise = MakeRefPtr<InitPromise::Private>(__func__);
  promise->UseDirectTaskDispatch(__func__);

  // Our remote frame will push layers updates to the compositor,
  // and we'll keep an indirect reference to that tree.
  GPUProcessManager* gpm = GPUProcessManager::Get();
  gpm->AllocateAndConnectLayerTreeId(compositor, mTabProcessId, &mLayersId)
      ->Then(
          GetCurrentSerialEventTarget(), __func__,
          // Hold a reference to the BrowserParent to ensure `this` remains
          // alive.
          [this, promise, bp = RefPtr{aBrowserParent}](
              layers::CompositorOptions&& aCompositorOptions) {
            mLayersConnected = true;
            mCompositorOptions = std::move(aCompositorOptions);
            promise->Resolve(Ok{}, __func__);
          },
          [this, promise, bp = RefPtr{aBrowserParent}](nsresult aError) {
            mLayersConnected = false;
            promise->Resolve(Ok{}, __func__);
          });

  return promise;
}

void RemoteLayerTreeOwner::Destroy() {
  if (mLayersId.IsValid()) {
    GPUProcessManager::Get()->UnmapLayerTreeId(mLayersId, mTabProcessId);
  }

  mBrowserParent = nullptr;
  mWindowRenderer = nullptr;
}

void RemoteLayerTreeOwner::EnsureLayersConnected(
    Maybe<CompositorOptions>& aCompositorOptions) {
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

}  // namespace layout
}  // namespace mozilla
