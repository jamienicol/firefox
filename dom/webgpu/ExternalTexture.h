/* -*- Mode: C++; tab-width: 4; indent-tabs-mode: nil; c-basic-offset: 2 -*- */
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef ExternalTexture_H_
#define ExternalTexture_H_

#include "mozilla/AlreadyAddRefed.h"
#include "mozilla/WeakPtr.h"
#include "mozilla/webgpu/ObjectModel.h"
#include "mozilla/webgpu/WebGPUTypes.h"
#include "nsTArray.h"

namespace mozilla {
namespace dom {
struct GPUExternalTextureDescriptor;
}
namespace ipc {
class Shmem;
}
namespace layers {
class Image;
}
namespace webgpu {
class Device;
}

namespace webgpu {

// Texture and TextureViews representing the planes of an ExternalTexture
// imported from an HTMLVideoElement or VideoFrame. There may be up to 3
// planes: either a single RGBA plane, a Y and an interleaved UV plane, or
// separate Y, U, and V planes.
// Each plane may have its own Texture and TextureView, or there may be a
// single Texture with each plane having its own View.
// This is managed separately from ExternalTexture, as multiple
// ExternalTextures may be imported from the same source but with different
// parameters.
class ExternalTexturePlanes : public ChildOf<Device>, public SupportsWeakPtr {
  NS_INLINE_DECL_THREADSAFE_REFCOUNTING(ExternalTexturePlanes)

 public:
  static RefPtr<ExternalTexturePlanes> Create(Device* aDevice,
                                              RefPtr<layers::Image> aImage);

  const RawId mMultiplanarTextureId;
  const std::array<RawId, 3> mTextureIds;
  const std::array<RawId, 3> mViewIds;

 private:
  ExternalTexturePlanes(Device* const aDevice, RefPtr<layers::Image> aImage,
                        RawId aMultiplanarTextureId,
                        std::array<RawId, 3>&& aTextureIds,
                        std::array<RawId, 3>&& aViewIds);
  ~ExternalTexturePlanes();

  const RefPtr<layers::Image> mImage;
};

// NOTE: Incomplete, and needs to be reconciled with the existing
// `ExternalTexture`, which is used by and for internals that handle compositor
// textures.
//
// Follow-up to complete implementation is at
// <https://bugzilla.mozilla.org/show_bug.cgi?id=1827116>.
class ExternalTexture : public ObjectBase, public ChildOf<Device> {
 public:
  GPU_DECL_CYCLE_COLLECTION(ExternalTexture)
  GPU_DECL_JS_WRAP(ExternalTexture)

  Device* GetDevice() { return mParent; }

  static already_AddRefed<ExternalTexture> Import(
      Device* const aParent, const dom::GPUExternalTextureDescriptor& aDesc,
      const RefPtr<ExternalTexturePlanes>& aPlanes);

  bool IsExpired() const { return mIsExpired; }
  void Expire();

  void MaybeDestroy();
  void OnSubmit(uint64_t aSubmissionIndex);
  void OnSubmissionWorkDone(uint64_t aSubmissionIndex);

  const RawId mId;

 private:
  explicit ExternalTexture(Device* const aParent, RawId aId,
                           const RefPtr<ExternalTexturePlanes>& aPlanes);
  virtual ~ExternalTexture();
  void Cleanup();

  bool mIsExpired = false;
  RefPtr<ExternalTexturePlanes> mPlanes;
  uint64_t mLastSubmissionIndex = 0;
  uint64_t mLastSubmissionWorkDoneIndex = 0;
};

}  // namespace webgpu
}  // namespace mozilla

#endif  // GPU_ExternalTexture_H_
