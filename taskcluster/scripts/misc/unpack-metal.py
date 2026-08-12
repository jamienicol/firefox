# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this file,
# You can obtain one at http://mozilla.org/MPL/2.0/.

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def toolchain_root(path):
    for parent in path.parents:
        if parent.name.endswith(".xctoolchain"):
            return parent
    raise RuntimeError(f"Could not find the toolchain containing {path}")


def copy_bundled_toolchain(destination):
    tools = {
        name: Path(
            subprocess.check_output(
                ["xcrun", "--sdk", "macosx", "--find", name], text=True
            ).strip()
        )
        for name in ("metal", "metallib")
    }
    root = toolchain_root(tools["metal"])

    bin_dir = destination / "usr" / "bin"
    bin_dir.mkdir(parents=True)
    for name, executable in tools.items():
        shutil.copy2(executable, bin_dir / name)
    shutil.copytree(
        root / "usr" / "metal", destination / "usr" / "metal", symlinks=True
    )


def download_toolchain_component(destination):
    with tempfile.TemporaryDirectory() as temp:
        temp_dir = Path(temp)
        subprocess.check_call([
            "xcodebuild",
            "-downloadComponent",
            "MetalToolchain",
            "-exportPath",
            str(temp_dir),
        ])

        dmgs = list(temp_dir.rglob("*.dmg"))
        if len(dmgs) != 1:
            raise RuntimeError(f"Expected one Metal toolchain DMG, found {len(dmgs)}")

        mountpoint = temp / "mount"
        mountpoint.mkdir()
        subprocess.check_call([
            "hdiutil",
            "attach",
            "-nobrowse",
            "-readonly",
            "-mountpoint",
            str(mountpoint),
            str(dmgs[0]),
        ])
        try:
            shutil.copytree(
                mountpoint / "Metal.xctoolchain",
                destination,
                symlinks=True,
            )
        finally:
            subprocess.check_call(["hdiutil", "detach", str(mountpoint)])


def can_download_toolchain_component():
    result = subprocess.run(
        ["xcodebuild", "-help"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    return "-downloadComponent" in result.stdout


def validate_toolchain(destination):
    tools = [destination / "usr" / "bin" / name for name in ("metal", "metallib")]
    for tool in tools:
        if not os.access(tool, os.X_OK):
            raise RuntimeError(
                f"Metal toolchain is missing {tool.relative_to(destination)}"
            )
    subprocess.check_call([tools[0], "--version"])
    for tool in tools:
        if (
            "x86_64"
            not in subprocess.check_output(
                ["xcrun", "lipo", "-archs", tool], text=True
            ).split()
        ):
            raise RuntimeError(f"{tool} does not contain an x86_64 binary")


def main(destination):
    destination = Path(destination)
    if sys.platform != "darwin":
        raise RuntimeError("Local production of the Metal toolchain requires macOS")
    if can_download_toolchain_component():
        download_toolchain_component(destination)
    else:
        copy_bundled_toolchain(destination)
    validate_toolchain(destination)


if __name__ == "__main__":
    main(sys.argv[1])
