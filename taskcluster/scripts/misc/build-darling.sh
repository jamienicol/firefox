#!/bin/bash

set -e -x -o pipefail

export GIT_TERMINAL_PROMPT=0
export PATH="$MOZ_FETCHES_DIR/clang/bin:$PATH"

WORKSPACE=$HOME/workspace
DARLING_SRC=$WORKSPACE/darling
DARLING_BUILD=$WORKSPACE/darling-build
DARLING_DIR=$MOZ_FETCHES_DIR/darling
DARLING_REVISION=f3ae93186481f522f55bd92e3dbb88903261fe99

mkdir -p "$WORKSPACE"

# This fork allows executing Mach-O programs via darling-run without the
# setuid-root launcher, namespaces, and mounts used by normal Darling startup,
# allowing it to run in an unprivileged Docker container.
# Fetch manually rather than using Taskcluster's Git source fetch, setting origin
# to upstream, so that relative submodule URLs resolve against upstream.
git clone --origin jrmuizel https://github.com/jrmuizel/darling "$DARLING_SRC"
git -C "$DARLING_SRC" remote add origin https://github.com/darlinghq/darling
git -C "$DARLING_SRC" checkout "$DARLING_REVISION"
git -C "$DARLING_SRC" submodule update --init --recursive

# Note that a darling installation is *not* portable, so users of this
# toolchain must extract it into $MOZ_FETCHES_DIR.
# See https://docs.darlinghq.org/build-instructions.html#custom-installation-prefix.
cmake -S "$DARLING_SRC" -B "$DARLING_BUILD" -GNinja \
    -DCMAKE_C_COMPILER="$MOZ_FETCHES_DIR/clang/bin/clang" \
    -DCMAKE_CXX_COMPILER="$MOZ_FETCHES_DIR/clang/bin/clang++" \
    -DCMAKE_INSTALL_PREFIX="$DARLING_DIR" \
    -DTARGET_i386=OFF \
    -DCOMPONENTS=system
ninja -C "$DARLING_BUILD"
ninja -C "$DARLING_BUILD" install

test -x "$DARLING_DIR/bin/darling-run"

mkdir -p "$UPLOAD_DIR"
tar caf "$UPLOAD_DIR/darling.tar.zst" \
    -C "$MOZ_FETCHES_DIR" \
    darling
