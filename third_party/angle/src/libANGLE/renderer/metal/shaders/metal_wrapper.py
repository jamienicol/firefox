#!/usr/bin/python3
# Copyright 2023 The ANGLE Project Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import os
import subprocess
import sys


def command(args):
    darling_prefix = os.environ.get('DARLING_PREFIX')
    if not darling_prefix:
        return ['xcrun'] + args

    metal_prefix = os.environ.get('METAL_PREFIX')
    if not metal_prefix:
        raise RuntimeError('METAL_PREFIX must be set when DARLING_PREFIX is set')

    darling_run = os.path.join(darling_prefix, 'bin', 'darling-run')
    metal_tool = os.path.join(metal_prefix, 'usr', 'bin', args[0])
    return [darling_run, metal_tool] + args[1:]


def main(args):
    try:
        args = command(args)
    except RuntimeError as error:
        print(error, file=sys.stderr)
        return 1

    return subprocess.run(args, stdout=subprocess.PIPE, text=True).returncode


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
