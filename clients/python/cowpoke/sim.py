"""Helpers for driving Cowpoke's simulators from Python: process models and staged faults.

A server-mode service is a simulated device; writing its tags changes what every client
reads over the wire. `Process` runs a model of the plant against those tags at a fixed
rate, and `inject_fault` applies a misconfiguration to a service and puts it back after.
"""
from __future__ import annotations

import copy
import threading
import time
from contextlib import contextmanager
from typing import Any, Callable, Iterator

from .client import Cowpoke


class Process:
    """Runs `step(cp, dt)` every `period` seconds on a thread of its own.

        def tank(cp, dt):
            out = 25 if cp.get("tank-plc.pump_run") else 0
            cp["tank-plc.level"] = cp.get("tank-plc.level", 50) + (cp.get("tank-plc.inflow", 12) - out) / 10 * dt

        with Process(cp, tank, period=0.2):
            ...  # the tank runs while this block does
    """

    def __init__(self, cp: Cowpoke, step: Callable[[Cowpoke, float], Any], period: float = 0.2):
        self.cp, self.step, self.period = cp, step, period
        self.errors: list[BaseException] = []
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> "Process":
        def run() -> None:
            last = time.monotonic()
            while not self._stop.wait(self.period):
                now = time.monotonic()
                try:
                    self.step(self.cp, now - last)
                except Exception as e:  # keep running; report what went wrong
                    self.errors.append(e)
                last = now
        self._thread = threading.Thread(target=run, name="cowpoke-process", daemon=True)
        self._thread.start()
        return self

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=2)

    def __enter__(self) -> "Process":
        return self.start()

    def __exit__(self, *exc: Any) -> None:
        self.stop()


def _definition(svc: dict) -> dict:
    """The editable part of GET /api/services/<id> (it also carries status fields)."""
    d = svc.get("definition", svc)
    return copy.deepcopy(d)


@contextmanager
def inject_fault(cp: Cowpoke, service: str, change: Callable[[dict], Any] | None = None, *,
                 settings: dict | None = None, tag: str | None = None, tag_fields: dict | None = None,
                 settle: float = 1.0) -> Iterator[dict]:
    """Misconfigures a service for the length of the block, then restores it (admin).

        with inject_fault(cp, "scada", tag="level", tag_fields={"register": 1}):
            ...  # scada reads the wrong register; see what the insights say

    Give `settings` to merge into the service's settings, `tag` with `tag_fields` to change one
    tag, or `change(definition)` to edit the definition dict in place. Yields the faulty definition.
    """
    original = _definition(cp.service(service))
    faulty = copy.deepcopy(original)
    if settings:
        faulty.setdefault("settings", {}).update(settings)
    if tag:
        t = next((t for t in faulty.get("tags", []) if t.get("name") == tag), None)
        if t is None:
            raise KeyError(f"{service} has no tag {tag}")
        t.update(tag_fields or {})
    if change:
        change(faulty)
    cp.update_service(service, faulty)
    time.sleep(settle)
    try:
        yield faulty
    finally:
        cp.update_service(service, original)
