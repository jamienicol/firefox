/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef _include_gfx_ipc_GPUThreadPerformanceHints_h_
#define _include_gfx_ipc_GPUThreadPerformanceHints_h_

#include <sched.h>

#include "base/platform_thread.h"
#include "mozilla/Maybe.h"
#include "mozilla/TimeStamp.h"
#include "mozilla/UniquePtr.h"
#include "nsTArray.h"

namespace mozilla {

namespace hal {
class PerformanceHintSession;
}

namespace gfx {

/**
 * Helps the GPU process's Renderer, Compositor and CanvasRender threads get
 * scheduled on the performant CPU cores on Android. Enabled by the
 * layers.gpu-process.thread-performance-hints pref.
 *
 * Prefers a PerformanceHintSession for the threads, which is told how long
 * each rendered frame took. If that is unavailable, falls back to setting the
 * threads' affinity to the big cores, like
 * ContentChild::ConfigureThreadPerformanceHints does for the stylo threads.
 *
 * Owned by the RenderThread and only used on the Renderer thread.
 */
class GPUThreadPerformanceHints final {
 public:
  /// Can only be called from the main thread, after the Compositor, Render and
  /// CanvasRender threads have been started. Does nothing if the pref is off.
  static void Start();

  explicit GPUThreadPerformanceHints(nsTArray<PlatformThreadHandle>&& aThreads);
  ~GPUThreadPerformanceHints();

  /// Called on the Renderer thread for each rendered frame.
  void DidRenderFrame(const TimeStamp& aStart, const TimeStamp& aEnd);

 private:
  void ApplyAffinity(const TimeStamp& aNow);

  UniquePtr<hal::PerformanceHintSession> mSession;
  nsTArray<pid_t> mThreadIds;
  Maybe<cpu_set_t> mCpuSet;
  TimeStamp mLastAffinityApplied;
};

}  // namespace gfx
}  // namespace mozilla

#endif  // _include_gfx_ipc_GPUThreadPerformanceHints_h_
