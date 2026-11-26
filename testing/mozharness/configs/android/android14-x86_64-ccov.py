# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

# mozharness configuration for Android x86/x86_64 14 code coverage tests
#
# This configuration should be combined with suite definitions and other
# mozharness configuration from android_common.py, or similar, as well as the
# base unittest emulator config, i.e. android14-x86_64.py.

config = {
    "emulator_memory": 6144,
}
