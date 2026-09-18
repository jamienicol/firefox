# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.
import re
import threading
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from mozperftest.test.browsertime import add_option


class QuietRequestHandler(SimpleHTTPRequestHandler):
    def log_message(self, format, *args):
        pass


_device = None
_server = None
_server_thread = None
_server_port = None


def _get_page(env):
    for option in env.get_arg("browsertime-extra-options", "").split(","):
        name, separator, value = option.strip().partition("=")
        if separator and name == "browsertime.page":
            if not re.fullmatch(r"[a-z0-9-]+", value):
                raise ValueError(f"Invalid scroll test page: {value}")
            return value
    raise ValueError(
        "Specify a scroll test page with "
        "--browsertime-extra-options browsertime.page=<page>"
    )


def _stop_server():
    global _server, _server_port, _server_thread

    if _server is not None:
        _server.shutdown()
        _server.server_close()
        _server = None
    if _server_thread is not None:
        _server_thread.join()
        _server_thread = None
    _server_port = None


def before_runs(env, **kw):
    global _device, _server, _server_port, _server_thread

    page = _get_page(env)
    directory = Path(__file__).parent
    filename = f"perftest_{page.replace('-', '_')}.html"
    if not (directory / filename).is_file():
        raise FileNotFoundError(f"Unknown scroll test page: {filename}")

    handler = partial(QuietRequestHandler, directory=directory)
    _server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    _server_port = _server.server_port
    _server_thread = threading.Thread(target=_server.serve_forever, daemon=True)
    _server_thread.start()

    try:
        if env.get_arg("android"):
            from mozperftest.utils import get_adb_device_or_emu

            _device = get_adb_device_or_emu(verbose=env.get_arg("verbose"))
            port = f"tcp:{_server_port}"
            _device.create_socket_connection("reverse", port, port)

        add_option(
            env,
            "browsertime.url",
            f"http://127.0.0.1:{_server_port}/{filename}",
        )
    except Exception:
        _device = None
        _stop_server()
        raise


def before_cycle(metadata, env, cycle, script):
    script["name"] = _get_page(env).replace("-", " ").title()


def after_runs(env, **kw):
    global _device, _server, _server_port, _server_thread

    try:
        if _device is not None:
            _device.remove_socket_connections("reverse", f"tcp:{_server_port}")
    finally:
        _device = None
        _stop_server()
