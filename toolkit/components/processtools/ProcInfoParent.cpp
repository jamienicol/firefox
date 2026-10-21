/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "mozilla/ProcInfoParent.h"

#include "mozilla/StaticPtr.h"
#include "mozilla/ipc/Endpoint.h"
#include "nsITimer.h"
#include "nsTHashMap.h"
#include "nsThreadUtils.h"

namespace mozilla {

static constexpr uint32_t kGetProcInfoTimeoutMs = 1000;

static StaticAutoPtr<nsTHashMap<GeckoChildID, ProcInfoParent*>> sActors;

/* static */
already_AddRefed<ProcInfoParent> ProcInfoParent::Create(
    ipc::Endpoint<PProcInfoParent>&& aEndpoint) {
  MOZ_ASSERT(NS_IsMainThread());
  const GeckoChildID childID = aEndpoint.OtherChildID();
  RefPtr<ProcInfoParent> actor = new ProcInfoParent(childID);
  if (!aEndpoint.Bind(actor)) {
    return nullptr;
  }
  if (!sActors) {
    sActors = new nsTHashMap<GeckoChildID, ProcInfoParent*>();
  }
  sActors->InsertOrUpdate(childID, actor);
  return actor.forget();
}

/* static */
ProcInfoParent* ProcInfoParent::Get(GeckoChildID aChildID) {
  MOZ_ASSERT(NS_IsMainThread());
  return sActors ? sActors->Get(aChildID) : nullptr;
}

void ProcInfoParent::ActorDestroy(ActorDestroyReason aWhy) {
  MOZ_ASSERT(NS_IsMainThread());
  if (!sActors) {
    return;
  }
  sActors->Remove(mChildID);
  if (sActors->IsEmpty()) {
    sActors = nullptr;
  }
}

void ProcInfoParent::Shutdown() {
  MOZ_ASSERT(NS_IsMainThread());
  if (CanSend()) {
    Close();
  }
}

RefPtr<ProcInfoParent::RemoteProcInfoPromise> ProcInfoParent::GetProcInfo() {
  MOZ_ASSERT(NS_IsMainThread());
  if (!CanSend()) {
    return RemoteProcInfoPromise::CreateAndReject(NS_ERROR_NOT_AVAILABLE,
                                                  __func__);
  }

  // The child may be unable to respond, e.g. if it has been frozen by the OS,
  // so reject rather than wait indefinitely. Whichever of the timer and the
  // response comes first settles the promise, and MozPromise ignores the other.
  auto promise = MakeRefPtr<RemoteProcInfoPromise::Private>(__func__);
  auto timer = NS_NewTimerWithCallback(
      [promise](nsITimer*) { promise->Reject(NS_ERROR_FAILURE, __func__); },
      kGetProcInfoTimeoutMs, nsITimer::TYPE_ONE_SHOT,
      "ProcInfoParent::GetProcInfo"_ns, GetMainThreadSerialEventTarget());
  if (timer.isErr()) {
    return RemoteProcInfoPromise::CreateAndReject(timer.unwrapErr(), __func__);
  }

  SendGetProcInfo()->Then(
      GetCurrentSerialEventTarget(), __func__,
      [promise, timer = timer.unwrap()](
          GetProcInfoPromise::ResolveOrRejectValue&& aResult) {
        timer->Cancel();
        if (aResult.IsReject() || aResult.ResolveValue().isNothing()) {
          promise->Reject(NS_ERROR_FAILURE, __func__);
          return;
        }
        promise->Resolve(std::move(aResult.ResolveValue().ref()), __func__);
      });
  return promise;
}

}  // namespace mozilla
