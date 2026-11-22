from __future__ import annotations

import asyncio
import fnmatch
import itertools
import json
import queue
import threading
import re
import urllib.error
import urllib.parse
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
        self._closed = threading.Event()
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
        self._closed.set()
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

    # -------------------------------------------------------------- history

    def history(self, name: str, since: str | float = "1h", until: Any = None, points: int = 600,
                frame: bool = True):
        """A tag's recorded history as a pandas DataFrame (index: time; columns: value[, min, max]).

        `since` is a duration ("15m", "1h", "7d") or a datetime; `until` defaults to now.
        Without pandas, or with frame=False, returns a list of (datetime, value) pairs.
        """
        import datetime as _dt
        sid, tag = _split(name)
        now = _dt.datetime.now(_dt.timezone.utc)
        start = now - _duration(since) if isinstance(since, (str, int, float)) else since
        end = until or now
        q = urllib.parse.urlencode({"from": _iso(start), "to": _iso(end), "points": points})
        res = self._rest("GET", f"/api/services/{_q(sid)}/history/{_q(tag)}?{q}")
        pts = res.get("points", [])
        times = [_dt.datetime.fromtimestamp(p["t"] / 1000, _dt.timezone.utc) for p in pts]
        if frame:
            try:
                import pandas as pd
            except ImportError:
                frame = False
        if not frame:
            return [(t, p["v"]) for t, p in zip(times, pts)]
        df = pd.DataFrame({"value": [p["v"] for p in pts]}, index=pd.DatetimeIndex(times, name="time"))
        if any("min" in p for p in pts):
            df["min"] = [p.get("min") for p in pts]
            df["max"] = [p.get("max") for p in pts]
        df.attrs["tag"] = name
        return df

    # -------------------------------------------------------------- services

    def services(self) -> list[dict]:
        """Every service with its kind, mode and state."""
        return self._rest("GET", "/api/services")

    def service(self, sid: str) -> dict:
        """One service's full definition and status."""
        return self._rest("GET", f"/api/services/{_q(sid)}")

    def start(self, sid: str) -> None:
        self._rest("POST", f"/api/services/{_q(sid)}/start")

    def stop(self, sid: str) -> None:
        self._rest("POST", f"/api/services/{_q(sid)}/stop")

    def restart(self, sid: str) -> None:
        self._rest("POST", f"/api/services/{_q(sid)}/restart")

    def update_service(self, sid: str, definition: dict) -> dict:
        """Replace a service's definition (admin)."""
        return self._rest("PUT", f"/api/services/{_q(sid)}", definition)

    # -------------------------------------------------------------- alarms

    def alarms(self) -> list[dict]:
        """Alarm definitions with their state and current value."""
        return self._rest("GET", "/api/alarms")

    def alarm_log(self, limit: int = 100) -> list[dict]:
        return self._rest("GET", f"/api/alarms/log?limit={limit}")

    def ack(self, alarm_id: str) -> None:
        self._rest("POST", f"/api/alarms/{_q(alarm_id)}/ack")

    def ack_all(self) -> None:
        self._rest("POST", "/api/alarms/ack-all")

    def on_alarm(self, fn: Callable[[dict], None]) -> Callable[[dict], None]:
        """Calls `fn(event)` for every alarm raise, clear and acknowledgement, on a thread of its own."""
        def follow() -> None:
            while not self._closed.is_set():
                try:
                    for ev, data in self._sse("/api/alarms/events"):
                        if ev == "alarm":
                            try:
                                fn(data)
                            except Exception:
                                pass
                except Exception:
                    self._closed.wait(1)
        threading.Thread(target=follow, name="cowpoke-alarms", daemon=True).start()
        return fn

    # -------------------------------------------------------------- insights

    def insights(self) -> dict:
        """Open, resolved and dismissed insights, as the Insights page shows them."""
        return self._rest("GET", "/api/insights")

    def run_insights(self) -> dict:
        """Run the analyzers now instead of waiting for the next pass."""
        return self._rest("POST", "/api/insights/run")

    def dismiss(self, insight_id: str) -> None:
        self._rest("POST", f"/api/insights/{_q(insight_id)}/dismiss")

    def restore(self, insight_id: str) -> None:
        self._rest("DELETE", f"/api/insights/{_q(insight_id)}/dismiss")

    # -------------------------------------------------------------- config as code

    def export_config(self) -> str:
        """Every service as YAML (admin)."""
        return self._rest("GET", "/api/config", raw=True)

    def apply_config(self, config: str | dict) -> None:
        """Replace every service with this config, YAML text or a dict (admin)."""
        body = config if isinstance(config, str) else json.dumps(config)
        self._rest("PUT", "/api/config", body, raw=True)

    # -------------------------------------------------------------- internals

    def _rest(self, method: str, path: str, body: Any = None, raw: bool = False) -> Any:
        data = None
        headers = {"Authorization": f"Bearer {self.token}"}
        if body is not None:
            if isinstance(body, str):
                data = body.encode()
                headers["Content-Type"] = "text/yaml"
            else:
                data = json.dumps(body).encode()
                headers["Content-Type"] = "application/json"
        req = urllib.request.Request(self.url + path, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                text = r.read().decode()
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace")
            try:
                detail = json.loads(detail).get("error", detail)
            except Exception:
                pass
            raise CowpokeError(f"{method} {path}: {e.code} {detail}") from None
        if raw:
            return text
        return json.loads(text) if text.strip() else None

    def _sse(self, path: str) -> Iterator[tuple[str, Any]]:
        req = urllib.request.Request(self.url + path, headers={"Authorization": f"Bearer {self.token}",
                                                               "Accept": "text/event-stream"})
        with urllib.request.urlopen(req) as r:
            ev, data = "message", []
            for raw in r:
                if self._closed.is_set():
                    return
                line = raw.decode().rstrip("\r\n")
                if not line:
                    if data:
                        try:
                            yield ev, json.loads("\n".join(data))
                        except ValueError:
                            pass
                    ev, data = "message", []
                elif line.startswith("event:"):
                    ev = line[6:].strip()
                elif line.startswith("data:"):
                    data.append(line[5:].strip())


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


def _split(name: str) -> tuple[str, str]:
    sid, _, tag = name.partition(".")
    if not sid or not tag:
        raise CowpokeError(f"`{name}` is not service.tag")
    return sid, tag


def _q(s: str) -> str:
    return urllib.parse.quote(s, safe="")


def _duration(d: str | float):
    import datetime as _dt
    if isinstance(d, (int, float)):
        return _dt.timedelta(seconds=d)
    m = re.fullmatch(r"\s*(\d+(?:\.\d+)?)\s*([smhdw])\s*", d)
    if not m:
        raise CowpokeError(f"duration `{d}`: use 30s, 15m, 1h, 7d or 2w")
    n, u = float(m.group(1)), m.group(2)
    return _dt.timedelta(seconds=n * {"s": 1, "m": 60, "h": 3600, "d": 86400, "w": 604800}[u])


def _iso(t) -> str:
    import datetime as _dt
    if t.tzinfo is None:
        t = t.replace(tzinfo=_dt.timezone.utc)
    return t.astimezone(_dt.timezone.utc).isoformat().replace("+00:00", "Z")


def _tag(t: dict) -> Tag:
    return Tag(t["name"], t.get("value"), t.get("quality"), t.get("ts"),
               t.get("datatype"), bool(t.get("writable")), t.get("units"))
