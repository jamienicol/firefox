#!/bin/bash

set -x

# Investigation for bug 2056565: builds darling
# (https://github.com/darlinghq/darling) from source and then tries to run a
# trivial macOS-side command through it, to find out whether darling's
# runtime (mount namespace + overlayfs prefix) works unprivileged inside a
# b-linux-docker-amd docker-worker task, before investing in the real
# toolchain/fetch plumbing needed to run `metal`/`metallib` under it.

WORKSPACE=$HOME/workspace
DARLING_SRC=$WORKSPACE/darling
DARLING_BUILD=$WORKSPACE/darling-build
DARLING_PREFIX=$WORKSPACE/darling-prefix
DARLING_REVISION=6efdaf4246ef01da66ebb57f27c5645d6cf95b4c
LOG=$WORKSPACE/darling.log

mkdir -p "$WORKSPACE"
exec > >(tee "$LOG") 2>&1

RESULT=BUILD_FAILED

upload_and_exit() {
    echo "RESULT: $RESULT"
    mkdir -p "$UPLOAD_DIR"
    tar caf "$UPLOAD_DIR/darling.tar.zst" -C "$WORKSPACE" "$(basename "$LOG")"
    if [ "$RESULT" = "RUNTIME_OK" ]; then
        exit 0
    else
        exit 1
    fi
}
trap upload_and_exit EXIT

set -e -o pipefail

echo "== cloning darling@$DARLING_REVISION =="
# darling has ~150 submodules, some of which have their own nested
# submodules (e.g. src/external/openpam -> darling/submodules/pam_modules).
# The standard `fetch` toolchain kind only does a non-recursive
# `git submodule update --init`, which isn't enough here, so this is cloned
# directly, similar to how build-custom-v8.sh/build-custom-car.sh clone
# depot_tools directly instead of using a fetch task.
git clone https://github.com/darlinghq/darling "$DARLING_SRC"
cd "$DARLING_SRC"
git checkout "$DARLING_REVISION"
git submodule update --init --recursive

echo "== configuring =="
mkdir -p "$DARLING_BUILD"
cd "$DARLING_BUILD"
# Only the "core" component (always on regardless of COMPONENTS) is needed
# for darlingserver/dyld/mldr/libSystem/the host-side `darling` executable.
# "cli" (and other optional components) pull in far more than we need here,
# including a keychain/Security stack with an unrelated broken dependency on
# a CoreData stub that's only built as part of the "dev_gui_common"
# component.
cmake -GNinja "$DARLING_SRC" \
    -DCMAKE_INSTALL_PREFIX="$DARLING_PREFIX" \
    -DTARGET_i386=OFF \
    -DCOMPONENTS=

echo "== building (this is the slow part) =="
ninja -j"$(nproc)"

echo "== installing =="
ninja install

RESULT=RUNTIME_FAILED
set +e -o pipefail

echo "== runtime smoke test: darling shell -- echo =="
# darling itself just checks geteuid() == 0 (src/startup/darling.c) before
# setting up its container (new mount namespace + overlayfs prefix) - it
# doesn't stat its own binary for a real setuid-root bit. We can't chown the
# installed binary to root (this script runs as the unprivileged `worker`
# user), but we don't need to: an unprivileged user namespace maps our own
# uid to 0 within it, which is enough to satisfy that check. That alone
# isn't sufficient for the mounts darling performs, though: CAP_SYS_ADMIN in
# a fresh user namespace only applies to namespaces *owned by* that user
# namespace, and there isn't a mount namespace one yet unless we also
# unshare one at the same time - hence also passing --mount here, so the new
# mount namespace is owned by our new user namespace (the standard
# rootless-containers pairing). This is the actual question this task
# exists to answer: does that work from inside an already-containerized,
# unprivileged docker-worker task?
#
# unshare defaults to making the new mount namespace's `/` MS_PRIVATE, which
# needs authority over the *inherited* `/` mount (owned by the outer/real
# root's mount namespace, not ours) and fails with EPERM under docker-worker.
# --propagation unchanged skips that step; darling's own mounts are all new
# ones it creates itself under its own namespace, which doesn't need it.

echo "== diagnostic: can we mount *anything* in a fresh userns+mountns? =="
# Isolates whether mount(2) itself is blocked (e.g. by a seccomp policy on
# the docker-worker container) from anything darling-specific: this doesn't
# touch darling at all, just tries to mount a plain tmpfs.
mkdir -p "$WORKSPACE/mount-test"
if unshare --user --mount --map-root-user --propagation unchanged -- mount -t tmpfs tmpfs "$WORKSPACE/mount-test"; then
    echo "diagnostic: tmpfs mount succeeded"
else
    echo "diagnostic: tmpfs mount failed"
fi

if unshare --user --mount --map-root-user --propagation unchanged -- "$DARLING_PREFIX/bin/darling" shell -- /bin/echo darling-runtime-ok | grep -q darling-runtime-ok; then
    RESULT=RUNTIME_OK
fi
