# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import json
import os
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

import buildconfig


def action(fh, *raw_args):
    fh.close()
    os.unlink(fh.name)

    raw_args = list(raw_args)
    objdir = Path(buildconfig.topobjdir)
    topsrcdir = Path(buildconfig.topsrcdir)

    def resolve_src_path(p):
        if os.path.isabs(p) and os.path.exists(p):
            return Path(p)
        if p.startswith("/"):
            return topsrcdir / p.lstrip("/")
        return topsrcdir / p

    def resolve_obj_path(p):
        if os.path.isabs(p) and os.path.exists(p):
            return Path(p)
        if p.startswith("/"):
            return objdir / p.lstrip("/")
        return objdir / p

    def resolve_input_path(p):
        if os.path.isabs(p) and os.path.exists(p):
            return Path(p)
        if p.startswith("!/"):
            return objdir / p[2:]
        if p.startswith("/"):
            if "/gen/" in p:
                return objdir / p.lstrip("/")
            return topsrcdir / p.lstrip("/")
        return topsrcdir / p

    try:
        script_index = max(i for i, arg in enumerate(raw_args) if arg.endswith(".py"))
        inputs = raw_args[:script_index]
        script = raw_args[script_index]
        target_dir = raw_args[script_index + 1]
        args = raw_args[script_index + 2 :]

        abs_target_dir = str(resolve_obj_path(target_dir))
        abs_script = resolve_src_path(script)
        script = [str(abs_script)]
        if abs_script.suffix == ".py":
            script = [sys.executable] + script
        if "{{response_file_name}}" in args:
            response_inputs = [str(resolve_input_path(p)) for p in inputs]
            with tempfile.NamedTemporaryFile(
                "w", dir=abs_target_dir, delete=False, encoding="utf-8"
            ) as response_file:
                response_file.write(shlex.join(response_inputs))
                response_file_name = os.path.relpath(response_file.name, abs_target_dir)
            args = [
                response_file_name if arg == "{{response_file_name}}" else arg
                for arg in args
            ]
        subprocess.check_call(script + args, cwd=abs_target_dir)
    except Exception:
        relative = os.path.relpath(__file__, topsrcdir)
        print(
            "%s:action caught exception. params=%s\n"
            % (relative, json.dumps([script, target_dir] + args, indent=2))
        )
        raise
