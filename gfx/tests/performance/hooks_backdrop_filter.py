# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import threading

from mozperftest.test.browsertime import add_option
from mozperftest.utils import get_adb_device_or_emu

FIXTURE_DIR = Path(__file__).parent / "backdrop-filter"

_server = None
_server_thread = None
_device = None
_reverse_port = None


class FixtureHandler(SimpleHTTPRequestHandler):
    def log_message(self, format, *args):
        pass


def _stop_server():
    global _server, _server_thread

    if _server is not None:
        _server.shutdown()
        _server.server_close()
        _server = None
    if _server_thread is not None:
        _server_thread.join()
        _server_thread = None


def before_runs(env, **kwargs):
    global _device, _reverse_port, _server, _server_thread

    handler = partial(FixtureHandler, directory=str(FIXTURE_DIR))
    _server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    _server_thread = threading.Thread(target=_server.serve_forever, daemon=True)
    _server_thread.start()
    port = _server.server_address[1]

    try:
        add_option(env, "firefox.preference", "layout.frame_rate:0")
        add_option(
            env, "firefox.preference", "docshell.event_starvation_delay_hint:1"
        )

        if env.get_arg("android"):
            _device = get_adb_device_or_emu()
            _reverse_port = f"tcp:{port}"
            _device.create_socket_connection("reverse", _reverse_port, _reverse_port)

        add_option(
            env,
            "browsertime.url",
            f"http://127.0.0.1:{port}/backdrop-filter.html",
        )
    except Exception:
        after_runs(env)
        raise


def after_runs(env, **kwargs):
    global _device, _reverse_port

    try:
        if _device is not None and _reverse_port is not None:
            _device.remove_socket_connections("reverse", _reverse_port)
    finally:
        _device = None
        _reverse_port = None
        _stop_server()
