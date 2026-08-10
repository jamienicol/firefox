#!/usr/bin/python3
# Copyright 2023 The ANGLE Project Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import os
import subprocess
import sys

import buildconfig


def command(args):
    metal_dir = buildconfig.substs.get('METAL_TOOLCHAIN_DIR')
    if metal_dir:
        metal_tool = os.path.join(metal_dir, 'usr', 'bin', args[0])
        fetches_dir = os.environ.get('MOZ_FETCHES_DIR')
        if fetches_dir:
            darling_run = os.path.join(fetches_dir, 'darling', 'bin', 'darling-run')
            if os.path.exists(darling_run):
                darling_args = [
                    os.path.join('/Volumes/SystemRoot', arg.lstrip('/'))
                    if os.path.isabs(arg)
                    else arg
                    for arg in args[1:]
                ]
                return [darling_run, metal_tool] + darling_args

        return [metal_tool] + args[1:]

    return ['xcrun', '--sdk', 'macosx'] + args


def main(args):
    args = command(args)
    return subprocess.run(args, stdout=subprocess.PIPE, text=True).returncode


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
