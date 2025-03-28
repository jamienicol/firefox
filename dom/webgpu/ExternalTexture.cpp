/* -*- Mode: C++; tab-width: 4; indent-tabs-mode: nil; c-basic-offset: 2 -*- */
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "ExternalTexture.h"

#include "MediaInfo.h"
#include "mozilla/CycleCollectedJSContext.h"
#include "mozilla/dom/HTMLVideoElement.h"
#include "mozilla/dom/VideoFrame.h"
#include "mozilla/dom/WebGPUBinding.h"
#include "mozilla/gfx/2D.h"
#include "mozilla/gfx/MatrixFwd.h"
#include "mozilla/gfx/Point.h"
#include "mozilla/glue/Debug.h"
#include "mozilla/ipc/ByteBuf.h"
#include "mozilla/layers/ImageBridgeChild.h"
#include "mozilla/layers/LayersSurfaces.h"
#include "mozilla/Maybe.h"
#include "mozilla/RefPtr.h"
#include "mozilla/webgpu/Buffer.h"
#include "mozilla/webgpu/ffi/wgpu.h"
#include "mozilla/webgpu/Texture.h"
#include "mozilla/webgpu/WebGPUChild.h"
#include "nsError.h"
#include "nsThreadUtils.h"
#include "Queue.h"

namespace mozilla::webgpu {

ExternalTexturePlanes::ExternalTexturePlanes(Device* const aDevice,
                                             RefPtr<layers::Image> aImage,
                                             RawId aMultiplanarTextureId,
                                             std::array<RawId, 3>&& aTextureIds,
                                             std::array<RawId, 3>&& aViewIds)
    : ChildOf(aDevice),
      mMultiplanarTextureId(aMultiplanarTextureId),
      mTextureIds(std::move(aTextureIds)),
      mViewIds(std::move(aViewIds)),
      mImage(aImage) {}

RefPtr<ExternalTexturePlanes> ExternalTexturePlanes::Create(
    Device* aDevice, RefPtr<layers::Image> aImage) {
  // FIXME: just use BuildSurfaceDescriptorGPUVideoOrBuffer()??
  Maybe<layers::SurfaceDescriptor> desc;
  if (aImage->GetFormat() == mozilla::ImageFormat::GPU_VIDEO) {
    // Only call GetDesc for GPU_VIDEO images. For other types (eg
    // PlanarYCbCrImage) this calls through to ShmemTextureData::Serialize
    // which uses an existing shmem, presumably allocated from a different IPC
    // actor, leading to a crash when attempting to deserialize.
    desc = aImage->GetDesc();
  }

  if (!desc) {
    layers::SurfaceDescriptorBuffer sdBuffer;
    printf_stderr("calling BuildSurfaceDescriptorBuffer()\n");
    nsresult result = aImage->BuildSurfaceDescriptorBuffer(
        sdBuffer, layers::Image::BuildSdbFlags::Default,
        [&](uint32_t aBufferSize) {
          ipc::Shmem buffer;
          if (!aDevice->GetBridge()->AllocShmem(aBufferSize, &buffer)) {
            return layers::MemoryOrShmem();
          }
          return layers::MemoryOrShmem(std::move(buffer));
        });
    if (NS_SUCCEEDED(result)) {
      desc.emplace(sdBuffer);
    }

    // FIXME: Andrew is making BuildSurfaceDescriptorBuffer work with
    // SourceSurfaceImage, therefore we won't need this fallback
    // if (!desc) {
    //   printf_stderr("jamiedbg falling back to calling
    //   GetAsSourceSurface()\n"); RefPtr<gfx::SourceSurface> surf =
    //   aImage->GetAsSourceSurface(); if (!surf) {
    //     return nullptr;
    //   }
    //   size_t length = layers::ImageDataSerializer::ComputeRGBBufferSize(
    //       surf->GetSize(), surf->GetFormat());
    //   ipc::Shmem shmem;
    //   if (!aDevice->GetBridge()->AllocShmem(length, &shmem)) {
    //     return nullptr;
    //   }
    //   RefPtr<gfx::DrawTarget> dt = gfx::Factory::CreateDrawTargetForData(
    //       gfx::BackendType::SKIA, shmem.get<uint8_t>(), surf->GetSize(),
    //       layers::ImageDataSerializer::ComputeRGBStride(surf->GetFormat(),
    //                                                     surf->GetSize().width),
    //       surf->GetFormat());
    //   if (!dt) {
    //     return nullptr;
    //   }
    //   dt->CopySurface(surf, gfx::IntRect(gfx::IntPoint(0, 0),
    //   surf->GetSize()),
    //                   gfx::IntPoint(0, 0));
    //   dt->Flush();
    //   desc.emplace(layers::SurfaceDescriptorBuffer(
    //       layers::RGBDescriptor(surf->GetSize(), surf->GetFormat()),
    //       layers::MemoryOrShmem(std::move(shmem))));
    // }
  }

  if (!desc) {
    return nullptr;
  }

  RefPtr<WebGPUChild> actor = aDevice->GetBridge();
  auto id = ffi::wgpu_client_make_multiplanar_texture_id(actor->GetClient());
  std::array<RawId, 3> textureIds{
      ffi::wgpu_client_make_texture_id(actor->GetClient()),
      ffi::wgpu_client_make_texture_id(actor->GetClient()),
      ffi::wgpu_client_make_texture_id(actor->GetClient()),
  };
  std::array<RawId, 3> viewIds{
      ffi::wgpu_client_make_texture_view_id(actor->GetClient()),
      ffi::wgpu_client_make_texture_view_id(actor->GetClient()),
      ffi::wgpu_client_make_texture_view_id(actor->GetClient()),
  };
  // printf_stderr(
  //     "jamiedbg ExternalTexturePlanes::Create() multiplanarId: %" PRIu64
  //     ", image: %p %d\n",
  //     id, aImage.get(), aImage->GetSerial());

  RefPtr<ExternalTexturePlanes> planes = new ExternalTexturePlanes(
      aDevice, aImage, id, std::move(textureIds), std::move(viewIds));

  aDevice->GetBridge()->FlushQueuedMessages();
  actor->SendDeviceCreateMultiplanarTexture(
      aDevice->mId, aDevice->GetQueue()->mId, planes->mMultiplanarTextureId,
      planes->mTextureIds, planes->mViewIds, *desc);

  return planes;
}

ExternalTexturePlanes::~ExternalTexturePlanes() {
  auto bridge = mParent->GetBridge();
  if (!bridge && bridge->CanSend()) {
    return;
  }
  // printf_stderr(
  //     "jamiedbg ExternalTexturePlanes::~ExternalTexturePlanes() id: %" PRIu64
  //     ", image: %p %d\n",
  //     mMultiplanarTextureId, mImage.get(), mImage->GetSerial());
  ffi::wgpu_client_destroy_multiplanar_texture(bridge->GetClient(),
                                               mMultiplanarTextureId);
  ffi::wgpu_client_drop_multiplanar_texture(bridge->GetClient(),
                                            mMultiplanarTextureId);
  ffi::wgpu_client_free_multiplanar_texture_id(bridge->GetClient(),
                                               mMultiplanarTextureId);
}

class ExternalTextureExpiryMicroTask final : public MicroTaskRunnable {
 public:
  ExternalTextureExpiryMicroTask(RefPtr<ExternalTexture> aExternalTexture)
      : mExternalTexture(aExternalTexture) {}
  //  FIXME do I need MOZ_CAN_RUN_SCRIPT?
  MOZ_CAN_RUN_SCRIPT
  virtual void Run(AutoSlowOperation& aAso) override {
    // printf_stderr("jamiedbg ExternalTextureExpiryMicroTask::Run()\n");
    mExternalTexture->GetDevice()->GetBridge()->FlushQueuedMessages();
    mExternalTexture->Expire();
  }

 private:
  RefPtr<ExternalTexture> mExternalTexture;
};

GPU_IMPL_CYCLE_COLLECTION(ExternalTexture, mParent)

/* static */ already_AddRefed<ExternalTexture> ExternalTexture::Import(
    Device* const aParent, const dom::GPUExternalTextureDescriptor& aDesc,
    const RefPtr<ExternalTexturePlanes>& aPlanes) {
  if (!aPlanes) {
    // FIXME: handle error
    return nullptr;
  }

  RefPtr<layers::Image> image;
  gfx::IntSize displaySize;
  gfx::IntRect cropRect;
  VideoRotation rotation;
  switch (aDesc.mSource.GetType()) {
    case dom::OwningHTMLVideoElementOrVideoFrame::Type::eHTMLVideoElement: {
      const auto& videoElement = aDesc.mSource.GetAsHTMLVideoElement();
      image = videoElement->GetCurrentImage();
      displaySize = {videoElement->VideoWidth(), videoElement->VideoHeight()};
      cropRect = gfx::IntRect({}, displaySize);
      rotation = videoElement->RotationDegrees();
    } break;
    case dom::OwningHTMLVideoElementOrVideoFrame::Type::eVideoFrame: {
      const auto& videoFrame = aDesc.mSource.GetAsVideoFrame();
      image = videoFrame->GetImage();
      displaySize = videoFrame->NativeDisplaySize();
      cropRect = videoFrame->NativeVisibleRect();
      rotation = VideoRotation::kDegree_0;
    } break;
  }

  if (rotation == VideoRotation::kDegree_90 ||
      rotation == VideoRotation::kDegree_270) {
    std::swap(cropRect.x, cropRect.y);
    std::swap(cropRect.width, cropRect.height);
  }

  // FIXME: replace with constants for each rotation to avoid trig calls
  gfx::Matrix sampleTransform =
      gfx::Matrix::Rotation(-static_cast<float>(rotation) * M_PI / 180.0);
  gfx::Matrix loadTransform;

  sampleTransform.PreTranslate(-0.5, -0.5);
  sampleTransform.PostTranslate(0.5, 0.5);

  gfx::IntSize unrotatedDisplaySize = displaySize;
  if (rotation == VideoRotation::kDegree_90 ||
      rotation == VideoRotation::kDegree_270) {
    std::swap(unrotatedDisplaySize.width, unrotatedDisplaySize.height);
  }
  sampleTransform.PostScale(
      static_cast<float>(cropRect.Width()) /
          static_cast<float>(unrotatedDisplaySize.width),
      static_cast<float>(cropRect.Height()) /
          static_cast<float>(unrotatedDisplaySize.height));
  sampleTransform.PostTranslate(
      static_cast<float>(cropRect.x) /
          static_cast<float>(unrotatedDisplaySize.width),
      static_cast<float>(cropRect.y) /
          static_cast<float>(unrotatedDisplaySize.height));

  loadTransform = sampleTransform;
  loadTransform.PreScale(
      1.0 / static_cast<float>(std::max(displaySize.width - 1, 1)),
      1.0 / static_cast<float>(std::max(displaySize.height - 1, 1)));
  loadTransform.PostScale(static_cast<float>(image->GetSize().width - 1),
                          static_cast<float>(image->GetSize().height - 1));

  // printf_stderr("jamiedbg rotation: %d\n", rotation);
  // printf_stderr("jamiedbg imageSize: %s\n",
  //               mozilla::ToString(image->GetSize()).c_str());
  // printf_stderr("jamiedbg displaySize: %s\n",
  //               mozilla::ToString(displaySize).c_str());
  // printf_stderr("jamiedbg cropRect: %s\n",
  // mozilla::ToString(cropRect).c_str()); printf_stderr("jamiedbg rotation:
  // %d\n", rotation); printf_stderr("jamiedbg sampleTransform: %s\n",
  //               mozilla::ToString(sampleTransform).c_str());
  // printf_stderr("jamiedbg loadTransform: %s\n",
  //               mozilla::ToString(loadTransform).c_str());

  ffi::WGPUPredefinedColorSpace colorSpace;
  switch (aDesc.mColorSpace) {
    case dom::PredefinedColorSpace::Srgb:
      colorSpace = ffi::WGPUPredefinedColorSpace::WGPUPredefinedColorSpace_Srgb;
      break;
    case dom::PredefinedColorSpace::Display_p3:
      colorSpace =
          ffi::WGPUPredefinedColorSpace::WGPUPredefinedColorSpace_DisplayP3;
      break;
  }
  RefPtr<ExternalTexture> externalTexture;
  // FIXME: reuse/unexpire existing ExternalTexture
  if (!externalTexture) {
    ffi::WGPUExternalTextureDescriptor desc{
        .label = nullptr,
        .width = static_cast<uint32_t>(displaySize.width),
        .height = static_cast<uint32_t>(displaySize.height),
        .sample_transform = {sampleTransform._11, sampleTransform._12,
                             sampleTransform._21, sampleTransform._22,
                             sampleTransform._31, sampleTransform._32},
        .load_transform = {loadTransform._11, loadTransform._12,
                           loadTransform._21, loadTransform._22,
                           loadTransform._31, loadTransform._32},
        .color_space = colorSpace,
    };
    ipc::ByteBuf bb;
    RawId id = ffi::wgpu_client_create_external_texture(
        aParent->GetBridge()->GetClient(), aParent->mId, &desc,
        aPlanes->mMultiplanarTextureId);
    externalTexture = new ExternalTexture(aParent, id, aPlanes);
  }

  // printf_stderr(
  //     "jamiedbg ExternalTexture::Import() id: %" PRIu64 ", planesId: %"
  //     PRIu64 "\n", externalTexture->mId,
  //     externalTexture->mPlanes->mMultiplanarTextureId);

  if (aDesc.mSource.GetType() ==
      dom::OwningHTMLVideoElementOrVideoFrame::Type::eHTMLVideoElement) {
    CycleCollectedJSContext* ccjs = CycleCollectedJSContext::Get();
    if (ccjs) {
      RefPtr<ExternalTextureExpiryMicroTask> expiryTask =
          new ExternalTextureExpiryMicroTask(externalTexture);
      ccjs->DispatchToMicroTask(expiryTask.forget());
    } else {
      // printf_stderr("jamiedbg Failed to get CycleCollectedJSContext\n");
    }
  }

  return externalTexture.forget();
}

ExternalTexture::ExternalTexture(Device* const aParent, RawId aId,
                                 const RefPtr<ExternalTexturePlanes>& aPlanes)
    : ChildOf(aParent), mId(aId), mPlanes(aPlanes) {}

void ExternalTexture::Cleanup() {
  if (!mValid) {
    return;
  }
  mValid = false;

  auto bridge = mParent->GetBridge();
  if (!bridge) {
    return;
  }

  if (mPlanes) {
    // printf_stderr("jamiedbg ExternalTexture::Cleanup() id: %" PRIu64
    //               ", planes_id: %" PRIu64 "\n",
    //               mId, mPlanes->mMultiplanarTextureId);
  }
  ffi::wgpu_client_drop_external_texture(bridge->GetClient(), mId);
  wgpu_client_free_external_texture_id(bridge->GetClient(), mId);
  mPlanes = nullptr;
}

ExternalTexture::~ExternalTexture() { Cleanup(); }

void ExternalTexture::Expire() {
  // printf_stderr("jamiedbg ExternalTexture::Expire() id: %" PRIu64 "\n", mId);
  mIsExpired = true;
  MaybeDestroy();
}

void ExternalTexture::OnSubmit(uint64_t aSubmissionIndex) {
  // printf_stderr("jamiedbg ExternalTexture::OnSubmit() id: %" PRIu64 ", index:
  // %" PRIu64
  //               "\n",
  //               mId, aSubmissionIndex);
  mLastSubmissionIndex = aSubmissionIndex;
}
void ExternalTexture::OnSubmissionWorkDone(uint64_t aSubmissionIndex) {
  // printf_stderr("jamiedbg ExternalTexture::OnSubmissionWorkDone() id: %"
  // PRIu64
  //               ", index: %" PRIu64 "\n",
  //               mId, aSubmissionIndex);
  mLastSubmissionWorkDoneIndex = aSubmissionIndex;
  MaybeDestroy();
}

void ExternalTexture::MaybeDestroy() {
  if (mLastSubmissionWorkDoneIndex >= mLastSubmissionIndex && mIsExpired) {
    // printf_stderr(
    //     "jamiedbg expired and last submission work done. destroying external
    //     " "texture %" PRIu64 "\n", mId);
    // wgpu_client_destroy_external_texture destroys the params buffer, and
    // dropping our planes reference destroys the textures when no other
    // external textures are still using them.
    //
    // FIXME: Ideally we wouldn't do this immediately in expire, but when we
    // know that the image is no longer valid, meaning we cannot reuse the
    // external texture. But for now this will do. It just means applications
    // may do slightly more work than is ideal.
    if (auto bridge = mParent->GetBridge()) {
      wgpu_client_destroy_external_texture(bridge->GetClient(), mId);
    }
    mPlanes = nullptr;
  }
}

JSObject* ExternalTexture::WrapObject(JSContext* cx,
                                      JS ::Handle<JSObject*> givenProto) {
  return dom::GPUExternalTexture_Binding::Wrap(cx, this, givenProto);
}

}  // namespace mozilla::webgpu
