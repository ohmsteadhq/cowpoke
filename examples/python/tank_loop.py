"""Cowpoke Python demo: a tank and its level controller, over real Modbus.

Two loops share one Cowpoke connection:

- The *process* (tank physics) runs against the simulated PLC, `tank-plc`. It fills
  the tank from `inflow`, drains it while `pump_run` is on, and writes `level`.
- The *controller* sees only what a SCADA would: the `scada` service's tags, polled
  over Modbus TCP. It follows `scada.level` live and switches `scada.pump_run`,
  which Cowpoke writes to the PLC's coil over the wire.

Change the setpoints while it runs (in the UI, or `scada.high_sp` from Python) and
watch the trend follow.

    COWPOKE_CONFIG=examples/python-demo.yaml COWPOKE_ADMIN_PASSWORD=dev-password cargo run
    pip install ./clients/python
    python examples/python/tank_loop.py --password dev-password
"""
import argparse
import threading
import time

from cowpoke import Cowpoke

PUMP_OUT = 25.0     # m³/h drained while the pump runs
TANK = 10.0         # m³/h that move the level 1 %/s (sped up so a cycle takes ~30 s)


def process(cp: Cowpoke, stop: threading.Event) -> None:
    """The tank: integrates inflow minus outflow and writes the level to the PLC."""
    level = cp.get("tank-plc.level", 50.0)
    last = time.monotonic()
    while not stop.wait(0.2):
        now = time.monotonic()
        dt, last = now - last, now
        out = PUMP_OUT if cp.get("tank-plc.pump_run") else 0.0
        level += (cp.get("tank-plc.inflow", 12.0) - out) / TANK * dt
        level = max(0.0, min(100.0, level))
        cp["tank-plc.level"] = round(level, 2)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--url", default="http://localhost:8080")
    ap.add_argument("--user", default="admin")
    ap.add_argument("--password", required=True)
    ap.add_argument("--seconds", type=float, default=0, help="stop after this long (0 = run until Ctrl-C)")
    a = ap.parse_args()

    with Cowpoke(a.url, user=a.user, password=a.password, match=["tank-plc.*", "scada.*"]) as cp:
        stop = threading.Event()
        threading.Thread(target=process, args=(cp, stop), daemon=True).start()
        print(f"following {len(cp.tags)} tags; Ctrl-C to stop")

        end = time.monotonic() + a.seconds if a.seconds else None
        switches = 0
        try:
            # The controller: on every level change seen through the SCADA's Modbus poll.
            for name, level in cp.watch("scada.level", timeout=5):
                if end and time.monotonic() > end:
                    break
                if level is None:
                    continue
                pump = bool(cp.get("scada.pump_run"))
                hi, lo = cp.get("scada.high_sp", 70.0), cp.get("scada.low_sp", 30.0)
                want = True if level >= hi else False if level <= lo else pump
                if want != pump:
                    t = time.perf_counter()
                    cp["scada.pump_run"] = want
                    switches += 1
                    print(f"level {level:5.1f} %  pump {'ON ' if want else 'OFF'}  "
                          f"(written over Modbus in {(time.perf_counter() - t) * 1000:.1f} ms)")
        except KeyboardInterrupt:
            pass
        finally:
            stop.set()
        print(f"done: {switches} pump switches")


if __name__ == "__main__":
    main()
