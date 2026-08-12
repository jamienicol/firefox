# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import shutil
import subprocess
import sys
from pathlib import Path


def main(destination):
    destination = Path(destination)
    tools = {
        name: Path(
            subprocess.check_output(
                ["xcrun", "--sdk", "macosx", "--find", name], text=True
            ).strip()
        )
        for name in ("metal", "metallib")
    }
    root = next(p for p in tools["metal"].parents if p.name.endswith(".xctoolchain"))

    bin_dir = destination / "usr" / "bin"
    bin_dir.mkdir(parents=True)
    for name, executable in tools.items():
        shutil.copy2(executable, bin_dir / name)
    shutil.copytree(
        root / "usr" / "metal", destination / "usr" / "metal", symlinks=True
    )


if __name__ == "__main__":
    main(sys.argv[1])
