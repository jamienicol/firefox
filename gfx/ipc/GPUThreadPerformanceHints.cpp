/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "GPUThreadPerformanceHints.h"

#include <pthread.h>

#include "gfxPlatform.h"
#include "mozilla/Hal.h"
#include "mozilla/StaticPrefs_layers.h"
#include "mozilla/gfx/CanvasRenderThread.h"
#include "mozilla/layers/CompositorThread.h"
#include "mozilla/webrender/RenderThread.h"
#include "nsThreadUtils.h"

namespace mozilla::gfx {

// Android resets a thread's affinity to all cores whenever its process moves
// between the background and foreground. The GPU process isn't told about
// this, so the affinity is re-applied this often while frames are rendered.
static constexpr double kAffinityReapplyIntervalSeconds = 1.0;

static void AppendCurrentThread(nsTArray<PlatformThreadHandle>& aThreads) {
  // The canvas work may run on the Compositor or Render thread.
  pthread_t self = pthread_self();
  for (pthread_t thread : aThreads) {
    if (pthread_equal(thread, self)) {
      return;
    }
  }
  aThreads.AppendElement(self);
}

static Maybe<cpu_set_t> GetPerformantCpuSet() {
  const auto& cpuInfo = hal::GetHeterogeneousCpuInfo();
  // If CPUs are homogeneous there is no point setting affinity.
  if (!cpuInfo || cpuInfo->mBigCpus.Count() == cpuInfo->mTotalNumCpus) {
    return Nothing();
  }

  BitSet<hal::HeterogeneousCpuInfo::MAX_CPUS> cpus = cpuInfo->mBigCpus;
  if (cpus.Count() < 2) {
    // As for the stylo threads (see ContentChild), additionally use the
    // medium cores if there are fewer than 2 big cores.
    cpus |= cpuInfo->mMediumCpus;
  }

  static_assert(hal::HeterogeneousCpuInfo::MAX_CPUS <= CPU_SETSIZE,
                "HeterogeneousCpuInfo::MAX_CPUS is too large for CPU_SETSIZE");

  cpu_set_t cpuset;
  CPU_ZERO(&cpuset);
  for (size_t i = 0; i < cpuInfo->mTotalNumCpus; i++) {
    if (cpus.test(i)) {
      CPU_SET(i, &cpuset);
    }
  }
  return Some(cpuset);
}

// static
void GPUThreadPerformanceHints::Start() {
  MOZ_ASSERT(NS_IsMainThread());

  if (!StaticPrefs::layers_gpu_process_thread_performance_hints_AtStartup()) {
    return;
  }

  nsCOMPtr<nsIThread> canvasThread =
      CanvasRenderThread::GetCanvasRenderThread();
  nsCOMPtr<nsIThread> renderThread = wr::RenderThread::GetRenderThread();
  if (!layers::CompositorThread() || !canvasThread || !renderThread) {
    return;
  }

  // Collect the native handle of each thread by hopping through them in turn,
  // ending on the Renderer thread, which owns the hints.
  layers::CompositorThread()->Dispatch(NS_NewRunnableFunction(
      "GPUThreadPerformanceHints::Start",
      [canvasThread = std::move(canvasThread),
       renderThread = std::move(renderThread)]() {
        nsTArray<PlatformThreadHandle> threads;
        AppendCurrentThread(threads);
        canvasThread->Dispatch(NS_NewRunnableFunction(
            "GPUThreadPerformanceHints::Start",
            [threads = std::move(threads), renderThread]() mutable {
              AppendCurrentThread(threads);
              renderThread->Dispatch(NS_NewRunnableFunction(
                  "GPUThreadPerformanceHints::Start",
                  [threads = std::move(threads)]() mutable {
                    AppendCurrentThread(threads);
                    if (wr::RenderThread* owner = wr::RenderThread::Get()) {
                      owner->SetPerformanceHints(
                          MakeUnique<GPUThreadPerformanceHints>(
                              std::move(threads)));
                    }
                  }));
            }));
      }));
}

GPUThreadPerformanceHints::GPUThreadPerformanceHints(
    nsTArray<PlatformThreadHandle>&& aThreads) {
  MOZ_ASSERT(wr::RenderThread::IsInRenderThread());

  // Estimate that we want the frame to be rendered in at most half of the
  // refresh period, like ContentChild::GetPerformanceHintTarget().
  const TimeDuration target =
      TimeDuration::FromMilliseconds(1000.0 /
                                     gfxPlatform::GetSoftwareVsyncRate()) /
      int64_t(2);
  mSession = hal::CreatePerformanceHintSession(aThreads, target);
  if (mSession) {
    return;
  }

  mCpuSet = GetPerformantCpuSet();
  if (!mCpuSet) {
    return;
  }

  // Remember the thread IDs rather than the handles, so that a thread which
  // exits before us is not a problem.
  for (pthread_t thread : aThreads) {
    mThreadIds.AppendElement(pthread_gettid_np(thread));
  }
  ApplyAffinity(TimeStamp::Now());
}

GPUThreadPerformanceHints::~GPUThreadPerformanceHints() = default;

void GPUThreadPerformanceHints::DidRenderFrame(const TimeStamp& aStart,
                                               const TimeStamp& aEnd) {
  MOZ_ASSERT(wr::RenderThread::IsInRenderThread());

  if (mSession) {
    mSession->ReportActualWorkDuration(aEnd - aStart);
  } else if (mCpuSet && (aEnd - mLastAffinityApplied).ToSeconds() >=
                            kAffinityReapplyIntervalSeconds) {
    ApplyAffinity(aEnd);
  }
}

void GPUThreadPerformanceHints::ApplyAffinity(const TimeStamp& aNow) {
  for (pid_t tid : mThreadIds) {
    // This can fail, e.g. if we are racing with the process being moved
    // to the foreground. It will be retried on a later frame.
    (void)sched_setaffinity(tid, sizeof(cpu_set_t), mCpuSet.ptr());
  }
  mLastAffinityApplied = aNow;
}

}  // namespace mozilla::gfx
