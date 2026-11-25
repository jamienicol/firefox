/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// Implements IChildProcess for child processes hosted by an Android native
// service (android:nativeService="true"), which run without a JVM. The parent
// binds to these exactly as it does to Java child services, so this must match
// the wire format of the Java AIDL proxy for IChildProcess.aidl.

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>
#include <android/log.h>
#include <android/native_service.h>
#include <dlfcn.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <atomic>
#include <optional>
#include <string>
#include <vector>

#include "APKOpen.h"
#include "NativeChildService.h"
#include "mozilla/Assertions.h"

#define NATIVE_SERVICE_API \
  __attribute__((availability(android, introduced = 37)))

#define LOG(...) \
  __android_log_print(ANDROID_LOG_INFO, "GeckoNativeChild", __VA_ARGS__)
#define LOG_ERROR(...) \
  __android_log_print(ANDROID_LOG_ERROR, "GeckoNativeChild", __VA_ARGS__)

namespace {

constexpr char kDescriptor[] = "org.mozilla.gecko.process.IChildProcess";

// These must match the order of the methods in IChildProcess.aidl.
enum : transaction_code_t {
  TRANSACTION_getPid = FIRST_CALL_TRANSACTION,
  TRANSACTION_start,
  TRANSACTION_crash,
  TRANSACTION_getCompositorSurfaceManager,
  TRANSACTION_getSurfaceAllocator,
  TRANSACTION_startNative,
};

// These must match the constants in IChildProcess.aidl.
constexpr int32_t STARTED_OK = 0;
constexpr int32_t STARTED_FAIL = 1;
constexpr int32_t STARTED_BUSY = 2;

constexpr size_t kGeckoThreadStackSize = 8 * 1024 * 1024;

using MaybeString = std::optional<std::string>;

struct StartArgs {
  std::vector<char*> mArgv;
  std::vector<int> mFds;
};

std::atomic<bool> sStarted{false};

// mozglue must load on API levels that lack these, and libbinder_ndk has no
// stub library at our minimum API level, so we resolve them at runtime.
#define NDK_FUNCTIONS(X)                          \
  X(android, ANativeService_setOnBindCallback)    \
  X(android, ANativeService_setOnDestroyCallback) \
  X(binder, AIBinder_Class_define)                \
  X(binder, AIBinder_new)                         \
  X(binder, AParcel_readInt32)                    \
  X(binder, AParcel_readParcelFileDescriptor)     \
  X(binder, AParcel_readString)                   \
  X(binder, AParcel_readStringArray)              \
  X(binder, AParcel_writeInt32)                   \
  X(binder, AParcel_writeStatusHeader)            \
  X(binder, AStatus_delete)                       \
  X(binder, AStatus_newOk)

struct NATIVE_SERVICE_API NdkFunctions {
#define DECLARE_NDK_FUNCTION(lib, name) decltype(&::name) name;
  NDK_FUNCTIONS(DECLARE_NDK_FUNCTION)
#undef DECLARE_NDK_FUNCTION
};

NATIVE_SERVICE_API NdkFunctions sNdk;

NATIVE_SERVICE_API bool LoadNdkFunctions() {
  void* android = dlopen("libandroid.so", RTLD_NOW);
  void* binder = dlopen("libbinder_ndk.so", RTLD_NOW);
  if (!android || !binder) {
    LOG_ERROR("Failed to load NDK libraries: %s", dlerror());
    return false;
  }
#define LOAD_NDK_FUNCTION(lib, name)                                    \
  sNdk.name = reinterpret_cast<decltype(sNdk.name)>(dlsym(lib, #name)); \
  if (!sNdk.name) {                                                     \
    LOG_ERROR("Missing NDK function %s", #name);                        \
    return false;                                                       \
  }
  NDK_FUNCTIONS(LOAD_NDK_FUNCTION)
#undef LOAD_NDK_FUNCTION
  return true;
}

bool StringAllocator(void* aData, int32_t aLength, char** aBuffer) {
  auto* str = static_cast<MaybeString*>(aData);
  if (aLength < 0) {
    str->reset();
    return true;
  }
  str->emplace(aLength, '\0');
  *aBuffer = str->value().data();
  return true;
}

bool StringArrayAllocator(void* aData, int32_t aLength) {
  auto* array = static_cast<std::vector<MaybeString>*>(aData);
  array->clear();
  if (aLength > 0) {
    array->resize(aLength);
  }
  return true;
}

bool StringArrayElementAllocator(void* aData, size_t aIndex, int32_t aLength,
                                 char** aBuffer) {
  auto* array = static_cast<std::vector<MaybeString>*>(aData);
  return StringAllocator(&(*array)[aIndex], aLength, aBuffer);
}

// The allocators size strings to include the null terminator.
void TrimTerminator(MaybeString& aStr) {
  if (aStr) {
    aStr->resize(strlen(aStr->c_str()));
  }
}

NATIVE_SERVICE_API binder_status_t ReadString(const AParcel* aIn,
                                              MaybeString& aOut) {
  binder_status_t rv = sNdk.AParcel_readString(aIn, &aOut, StringAllocator);
  TrimTerminator(aOut);
  return rv;
}

NATIVE_SERVICE_API binder_status_t
ReadStringArray(const AParcel* aIn, std::vector<MaybeString>& aOut) {
  binder_status_t rv = sNdk.AParcel_readStringArray(
      aIn, &aOut, StringArrayAllocator, StringArrayElementAllocator);
  for (auto& str : aOut) {
    TrimTerminator(str);
  }
  return rv;
}

// Mirrors Parcel.writeTypedArray(ParcelFileDescriptor[]).
NATIVE_SERVICE_API binder_status_t ReadFdArray(const AParcel* aIn,
                                               std::vector<int>& aOut) {
  int32_t count = 0;
  binder_status_t rv = sNdk.AParcel_readInt32(aIn, &count);
  if (rv != STATUS_OK) {
    return rv;
  }
  for (int32_t i = 0; i < count; ++i) {
    int fd = -1;
    rv = sNdk.AParcel_readParcelFileDescriptor(aIn, &fd);
    if (rv != STATUS_OK) {
      return rv;
    }
    aOut.push_back(fd);
  }
  return STATUS_OK;
}

void CloseFds(const std::vector<int>& aFds) {
  for (int fd : aFds) {
    if (fd >= 0) {
      close(fd);
    }
  }
}

void* GeckoThreadMain(void* aArg) {
  auto* args = static_cast<StartArgs*>(aArg);
  int argc = static_cast<int>(args->mArgv.size()) - 1;
  int rv = RunNativeChildProcess(argc, args->mArgv.data(), args->mFds.data(),
                                 args->mFds.size());
  LOG("Child process exited with %d", rv);
  // As with Java child processes, the process only exists to run Gecko.
  _exit(0);
}

NATIVE_SERVICE_API int32_t StartNative(const AParcel* aIn) {
  MaybeString mainProcessId;
  std::vector<MaybeString> args;
  std::vector<MaybeString> env;
  std::vector<int> fds;
  if (ReadString(aIn, mainProcessId) != STATUS_OK ||
      ReadStringArray(aIn, args) != STATUS_OK ||
      ReadStringArray(aIn, env) != STATUS_OK ||
      ReadFdArray(aIn, fds) != STATUS_OK) {
    LOG_ERROR("Failed to read startNative arguments");
    CloseFds(fds);
    return STARTED_FAIL;
  }

  // Each service declaration hosts a single child process, so unlike Java
  // children we never have to share this process with another GeckoRuntime.
  if (sStarted.exchange(true)) {
    LOG_ERROR("Child process already started");
    CloseFds(fds);
    return STARTED_BUSY;
  }

  for (const auto& var : env) {
    if (var) {
      // putenv keeps the string, so it is leaked deliberately.
      putenv(strdup(var->c_str()));
    }
  }

  auto* startArgs = new StartArgs();
  for (const auto& arg : args) {
    startArgs->mArgv.push_back(strdup(arg ? arg->c_str() : ""));
  }
  startArgs->mArgv.push_back(nullptr);
  startArgs->mFds = std::move(fds);

  pthread_attr_t attr;
  pthread_attr_init(&attr);
  pthread_attr_setstacksize(&attr, kGeckoThreadStackSize);
  pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
  pthread_t thread;
  int rv = pthread_create(&thread, &attr, GeckoThreadMain, startArgs);
  pthread_attr_destroy(&attr);
  if (rv != 0) {
    LOG_ERROR("Failed to create Gecko thread: %s", strerror(rv));
    CloseFds(startArgs->mFds);
    delete startArgs;
    return STARTED_FAIL;
  }
  pthread_setname_np(thread, "Gecko");
  return STARTED_OK;
}

NATIVE_SERVICE_API binder_status_t WriteResult(AParcel* aOut, int32_t aValue) {
  AStatus* status = sNdk.AStatus_newOk();
  binder_status_t rv = sNdk.AParcel_writeStatusHeader(aOut, status);
  sNdk.AStatus_delete(status);
  if (rv != STATUS_OK) {
    return rv;
  }
  return sNdk.AParcel_writeInt32(aOut, aValue);
}

void* OnBinderCreate(void* aArgs) { return aArgs; }

void OnBinderDestroy(void* aUserData) {}

NATIVE_SERVICE_API binder_status_t OnTransact(AIBinder* aBinder,
                                              transaction_code_t aCode,
                                              const AParcel* aIn,
                                              AParcel* aOut) {
  switch (aCode) {
    case TRANSACTION_getPid:
      return WriteResult(aOut, getpid());
    case TRANSACTION_start:
      LOG_ERROR("Invalid call to IChildProcess.start for a native child");
      return WriteResult(aOut, STARTED_FAIL);
    case TRANSACTION_crash:
      MOZ_CRASH("Crash requested by parent process");
    case TRANSACTION_startNative:
      return WriteResult(aOut, StartNative(aIn));
    default:
      LOG_ERROR("Unsupported IChildProcess transaction %u", aCode);
      return STATUS_UNKNOWN_TRANSACTION;
  }
}

NATIVE_SERVICE_API AIBinder* OnBind(ANativeService* aService,
                                    uint64_t aBindToken, const char* aAction,
                                    const char* aData) {
  static AIBinder_Class* sClass = sNdk.AIBinder_Class_define(
      kDescriptor, OnBinderCreate, OnBinderDestroy, OnTransact);
  return sNdk.AIBinder_new(sClass, nullptr);
}

void OnDestroy(ANativeService* aService) {
  LOG("Destroying native child service");
  _exit(0);
}

}  // namespace

extern "C" APKOPEN_EXPORT NATIVE_SERVICE_API void
GeckoNativeChildService_onCreate(ANativeService* aService) {
  LOG("onCreate, pid %d", getpid());
  if (!LoadNdkFunctions()) {
    MOZ_CRASH("Native child service cannot load the NDK functions it needs");
  }
  sNdk.ANativeService_setOnBindCallback(aService, OnBind);
  sNdk.ANativeService_setOnDestroyCallback(aService, OnDestroy);
}
