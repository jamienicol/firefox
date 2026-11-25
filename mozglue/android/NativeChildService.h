/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef NativeChildService_h
#define NativeChildService_h

#include <stddef.h>

// Loads the Gecko libraries and runs a child process without a JVM. aFds are
// the file descriptors passed by the parent, which this takes ownership of.
// Returns once the child process has shut down.
int RunNativeChildProcess(int aArgc, char* aArgv[], int* aFds, size_t aFdCount);

#endif  // NativeChildService_h
