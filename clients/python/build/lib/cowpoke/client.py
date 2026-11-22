from __future__ import annotations

import asyncio
import fnmatch
import itertools
import json
import queue
import threading
import urllib.request
from dataclasses import dataclass
from typing import Any, Callable, Iterator, Optional

import websockets


class CowpokeError(Exception):
    pass


@dataclass
class Tag:
    name: str
    value: Any = None
    quality: Optional[str] = None
    ts: Optional[str] = None
    datatype: Optional[str] = None
    writable: bool = False
    units: Optional[str] = None


class Cowpoke:
    """A live connection to a Cowpoke server.

    Every tag's latest value is kept in memory and updated as changes arrive, so
    `cp["svc.tag"]` and `cp.tags` never touch the network. Give `token`, or
    `user` and `password`. `match` limits what is followed (`"plc1.*,line2.*"`).
    """

    def __init__(self, url: str = "http://localhost:8080", *, token: str | None = None,
                 user: str | None = None, password: str | None = None,
                 match: str | list[str] | None = None, timeout: float = 10.0):
        self.url = url.rstrip("/")
        self.timeout = timeout
        self.token = token or self._login(user, password)
        self._pats = [match] if isinstance(match, str) else list(match or [])
        self._tags: dict[str, Tag] = {}
        self._lock = threading.Lock()
        self._ids = itertools.count(1)
        self._pending: dict[int, "asyncio.Future[dict]"] = {}
        self._watchers: list[Callable[[Tag], None]] = []
        self._ready = threading.Event()
        self._error: BaseException | None = None
        self._loop = asyncio.new_event_loop()
        self._thread = threading.Thread(target=self._run, name="cowpoke", daemon=True)
        self._thread.start()
        if not self._ready.wait(timeout):
            self.close()
            raise CowpokeError(f"no answer from {self.url} in {timeout} s")
        if self._error:
            raise CowpokeError(str(self._error))

    # -------------------------------------------------------------- reading

    @property
    def tags(self) -> dict[str, Tag]:
        """Every followed tag by name, as it stands now."""
        with self._lock:
            return dict(self._tags)

    @property
    def values(self) -> dict[str, Any]:
        with self._lock:
            return {n: t.value for n, t in self._tags.items()}

    def __getitem__(self, name: str) -> Any:
        with self._lock:
            if name not in self._tags:
                raise KeyError(name)
            return self._tags[name].value

    def __contains__(self, name: str) -> bool:
        with self._lock:
            return name in self._tags

    def get(self, name: str, default: Any = None) -> Any:
        with self._lock:
            t = self._tags.get(name)
        return default if t is None else t.value

    def read(self, *patterns: str) -> dict[str, Any]:
        """Fresh values from the server (not the local copy), for any tags, followed or not."""
        r = self._call({"read": list(patterns) or ["*"]})
        return {t["name"]: t["value"] for t in r["tags"]}

    # -------------------------------------------------------------- writing

    def __setitem__(self, name: str, value: Any) -> None:
        self.write({name: value})

    def write(self, values: dict[str, Any]) -> dict[str, Any]:
        """Writes several tags; raises if any fails. Returns the values written."""
        res = self._call({"write": values})["results"]
        bad = {n: r["error"] for n, r in res.items() if not r.get("ok")}
        if bad:
            raise CowpokeError("; ".join(f"{n}: {e}" for n, e in bad.items()))
        return {n: r["value"] for n, r in res.items()}

    # -------------------------------------------------------------- following

    def on_change(self, fn: Callable[[Tag], None]) -> Callable[[Tag], None]:
        """Calls `fn(tag)` on every change (on the client's thread; keep it quick)."""
        self._watchers.append(fn)
        return fn

    def watch(self, *patterns: str, timeout: float | None = None) -> Iterator[tuple[str, Any]]:
        """Yields `(name, value)` for each change matching the patterns, as it happens."""
        q: "queue.Queue[Tag]" = queue.Queue()
        pats = list(patterns)
        fn = self.on_change(lambda t: q.put(t) if not pats or any(fnmatch.fnmatchcase(t.name, p) for p in pats) else None)
        try:
            while True:
                try:
                    t = q.get(timeout=timeout)
                except queue.Empty:
                    return
                yield t.name, t.value
        finally:
            self._watchers.remove(fn)

    def close(self) -> None:
        if self._loop.is_running():
            try:
                asyncio.run_coroutine_threadsafe(self._shutdown(), self._loop).result(3)
            except Exception:
                pass
            self._loop.call_soon_threadsafe(self._loop.stop)
        self._thread.join(timeout=3)

    async def _shutdown(self) -> None:
        self._task.cancel()
        try:
            await self._task
        except BaseException:
            pass

    def __enter__(self) -> "Cowpoke":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    # -------------------------------------------------------------- internals

    def _login(self, user: str | None, password: str | None) -> str:
        if not user:
            raise CowpokeError("give token=, or user= and password=")
        req = urllib.request.Request(
            f"{self.url}/api/login", data=json.dumps({"username": user, "password": password}).encode(),
            headers={"Content-Type": "application/json"}, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                return json.load(r)["token"]
        except Exception as e:
            raise CowpokeError(f"login failed: {e}") from e

    def _call(self, msg: dict) -> dict:
        if self._error:
            raise CowpokeError(f"disconnected: {self._error}")
        fut = asyncio.run_coroutine_threadsafe(self._request(msg), self._loop)
        r = fut.result(self.timeout)
        if r.get("type") == "error":
            raise CowpokeError(r.get("error"))
        return r

    async def _request(self, msg: dict) -> dict:
        i = next(self._ids)
        fut = self._loop.create_future()
        self._pending[i] = fut
        await self._ws.send(json.dumps({**msg, "id": i}))
        return await fut

    def _run(self) -> None:
        asyncio.set_event_loop(self._loop)
        self._task = self._loop.create_task(self._main())
        self._loop.run_forever()
        self._loop.close()

    async def _main(self) -> None:
        ws_url = self.url.replace("http", "ws", 1) + "/api/tags/ws"
        if self._pats:
            ws_url += "?match=" + ",".join(self._pats)
        while True:
            try:
                async with websockets.connect(
                        ws_url, additional_headers={"Authorization": f"Bearer {self.token}"},
                        max_size=None) as ws:
                    self._ws = ws
                    self._error = None
                    async for raw in ws:
                        self._handle(json.loads(raw))
            except asyncio.CancelledError:
                return
            except Exception as e:  # reconnect; the next snapshot catches up
                self._error = e
                for f in self._pending.values():
                    if not f.done():
                        f.set_exception(CowpokeError(f"disconnected: {e}"))
                self._pending.clear()
                if not self._ready.is_set():
                    self._ready.set()
                    return
                await asyncio.sleep(1)

    def _handle(self, m: dict) -> None:
        kind = m.get("type")
        if kind == "snapshot" and m.get("id") is None:
            with self._lock:
                self._tags = {t["name"]: _tag(t) for t in m["tags"]}
            self._ready.set()
            return
        if kind == "tag":
            with self._lock:
                t = self._tags.get(m["name"]) or Tag(m["name"])
                t.value, t.quality, t.ts = m.get("value"), m.get("quality"), m.get("ts")
                self._tags[m["name"]] = t
            for fn in list(self._watchers):
                try:
                    fn(t)
                except Exception:
                    pass
            return
        fut = self._pending.pop(m.get("id"), None)
        if fut and not fut.done():
            fut.set_result(m)


def _tag(t: dict) -> Tag:
    return Tag(t["name"], t.get("value"), t.get("quality"), t.get("ts"),
               t.get("datatype"), bool(t.get("writable")), t.get("units"))
