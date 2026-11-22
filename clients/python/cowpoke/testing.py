"""A throwaway Cowpoke for tests: no hardware, no shared state.

    from cowpoke.testing import CowpokeServer

    with CowpokeServer("examples/demo.yaml") as srv:
        with srv.client() as cp:
            assert cp["plc-reader.tank_level"] is not None

With pytest the `cowpoke_server` fixture does the same; set the config with a marker:

    @pytest.mark.cowpoke_config("examples/demo.yaml")
    def test_tank(cowpoke_server):
        with cowpoke_server.client() as cp: ...

The binary comes from $COWPOKE_BIN, else `cowpoke` on PATH, else ./target/{release,debug}/cowpoke.
"""
from __future__ import annotations

import os
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request
from typing import Any

from .client import Cowpoke, CowpokeError


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def find_binary() -> str:
    env = os.environ.get("COWPOKE_BIN")
    if env:
        return env
    on_path = shutil.which("cowpoke")
    if on_path:
        return on_path
    for build in ("release", "debug"):
        p = os.path.join(os.getcwd(), "target", build, "cowpoke")
        if os.path.exists(p):
            return p
    raise CowpokeError("no cowpoke binary: set COWPOKE_BIN or put cowpoke on PATH")


class CowpokeServer:
    def __init__(self, config: str | None = None, *, binary: str | None = None, port: int | None = None,
                 password: str | None = None, startup: float = 30.0, env: dict | None = None):
        self.config = os.path.abspath(config) if config else None
        self.binary = binary or find_binary()
        self.port = port or _free_port()
        self.password = password or secrets.token_urlsafe(12)
        self.url = f"http://127.0.0.1:{self.port}"
        self.startup = startup
        self.extra_env = env or {}
        self._dir: str | None = None
        self._proc: subprocess.Popen | None = None

    def start(self) -> "CowpokeServer":
        self._dir = tempfile.mkdtemp(prefix="cowpoke-test-")
        env = dict(os.environ, COWPOKE_DATA=self._dir, COWPOKE_BIND=f"127.0.0.1:{self.port}",
                   COWPOKE_ADMIN_PASSWORD=self.password, COWPOKE_LOG="warn", **self.extra_env)
        if self.config:
            env["COWPOKE_CONFIG"] = self.config
        self._log = open(os.path.join(self._dir, "cowpoke.log"), "w")
        self._proc = subprocess.Popen([self.binary], env=env, stdout=self._log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + self.startup
        while time.monotonic() < deadline:
            if self._proc.poll() is not None:
                raise CowpokeError(f"cowpoke exited ({self._proc.returncode}); see {self._log.name}")
            try:
                with urllib.request.urlopen(self.url + "/healthz", timeout=1):
                    return self
            except Exception:
                time.sleep(0.1)
        self.stop()
        raise CowpokeError(f"cowpoke didn't start in {self.startup} s")

    def client(self, **kw: Any) -> Cowpoke:
        return Cowpoke(self.url, user="admin", password=self.password, **kw)

    def stop(self) -> None:
        if self._proc and self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self._proc.kill()
        if self._dir:
            self._log.close()
            shutil.rmtree(self._dir, ignore_errors=True)
            self._dir = None

    def __enter__(self) -> "CowpokeServer":
        return self.start()

    def __exit__(self, *exc: Any) -> None:
        self.stop()


try:
    import pytest

    def pytest_configure(config: Any) -> None:
        config.addinivalue_line("markers", "cowpoke_config(path): the YAML the cowpoke_server fixture starts with")

    @pytest.fixture
    def cowpoke_server(request: Any):
        m = request.node.get_closest_marker("cowpoke_config")
        with CowpokeServer(m.args[0] if m else None) as srv:
            yield srv
except ImportError:  # pytest is optional
    pass
