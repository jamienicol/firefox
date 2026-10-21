/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef mozilla_ProcInfoChild_h
#define mozilla_ProcInfoChild_h

#include "mozilla/PProcInfoChild.h"

namespace mozilla {

// Child process side of PProcInfo. Lives on a background task queue so that
// it can respond even if the main thread is busy.
class ProcInfoChild final : public PProcInfoChild {
 public:
  NS_INLINE_DECL_THREADSAFE_REFCOUNTING(ProcInfoChild, override)

  // Must be called on the main thread. Binds a new actor to aEndpoint on a
  // background task queue.
  static already_AddRefed<ProcInfoChild> Create(
      ipc::Endpoint<PProcInfoChild>&& aEndpoint);

  void Shutdown();

  ipc::IPCResult RecvGetProcInfo(GetProcInfoResolver&& aResolver);

 private:
  explicit ProcInfoChild(nsISerialEventTarget* aTaskQueue)
      : mTaskQueue(aTaskQueue) {}
  ~ProcInfoChild() = default;

  const nsCOMPtr<nsISerialEventTarget> mTaskQueue;
};

}  // namespace mozilla

#endif
