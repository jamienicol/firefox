#!/bin/bash

set -e -x -o pipefail

export GIT_TERMINAL_PROMPT=0

WORKSPACE=$HOME/workspace
FETCHES_ROOT=$HOME/fetches
DARLING_SRC=$WORKSPACE/darling
DARLING_BUILD=$WORKSPACE/darling-build
DARLING_DIR=$FETCHES_ROOT/darling
DARLING_BASE_REVISION=6efdaf4246ef01da66ebb57f27c5645d6cf95b4c
DARLING_REVISION=f3ae93186481f522f55bd92e3dbb88903261fe99

mkdir -p "$WORKSPACE" "$FETCHES_ROOT"

git clone https://github.com/darlinghq/darling "$DARLING_SRC"
git -C "$DARLING_SRC" checkout "$DARLING_BASE_REVISION"
git -C "$DARLING_SRC" submodule update --init --recursive
git -C "$DARLING_SRC" remote add jrmuizel https://github.com/jrmuizel/darling
git -C "$DARLING_SRC" fetch --no-tags jrmuizel standalone
git -C "$DARLING_SRC" checkout "$DARLING_REVISION"
git -C "$DARLING_SRC" submodule sync
git -C "$DARLING_SRC" submodule update --init --recursive

cmake -S "$DARLING_SRC" -B "$DARLING_BUILD" -GNinja \
    -DCMAKE_INSTALL_PREFIX="$DARLING_DIR" \
    -DTARGET_i386=OFF \
    -DCOMPONENTS=system
ninja -C "$DARLING_BUILD" -j"$(nproc)"
ninja -C "$DARLING_BUILD" install

test -x "$DARLING_DIR/bin/darling-run"

mkdir -p "$UPLOAD_DIR"
tar caf "$UPLOAD_DIR/darling.tar.zst" \
    -C "$FETCHES_ROOT" \
    darling
