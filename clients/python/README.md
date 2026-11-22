# cowpoke (Python)

Read, write and follow every Cowpoke tag in real time.

```bash
pip install ./clients/python
```

```python
from cowpoke import Cowpoke

with Cowpoke("http://localhost:8080", user="admin", password="...") as cp:
    print(cp.values)                       # every tag: {"plc1.Motor_Speed": 41.2, ...}
    print(cp["plc1.Motor_Speed"])          # one value, from the live local copy
    cp["plc1.Motor_Speed"] = 50            # write (Operator role)
    cp.write({"plc1.Run": True, "plc1.Setpoint": 72.5})
    for name, value in cp.watch("plc1.*"): # every change as it arrives
        print(name, value)
```

- Names are `service.tag`. `match="plc1.*,line2.*"` follows only some tags; `*` is a wildcard.
- One WebSocket (`/api/tags/ws`) carries the snapshot, every change and the writes. Reads are local; a write is one round trip and raises `CowpokeError` if any tag fails.
- `cp.read("plc1.*")` asks the server instead of the local copy. `@cp.on_change` registers a callback.
- It reconnects on its own and takes a fresh snapshot.
- Use `token=` with an API token instead of a user and password.

## More than tags

```python
df = cp.history("scada.level", since="1h")   # pandas DataFrame (pip install "cowpoke[pandas]")
cp.services(); cp.restart("scada")           # service state and control
cp.alarms(); cp.ack("high-level"); cp.ack_all()
cp.on_alarm(lambda ev: print(ev))            # raises, clears and acks as they happen
cp.insights(); cp.dismiss(insight_id)
yaml_text = cp.export_config(); cp.apply_config(yaml_text)   # config as code (admin)
```

**Simulation.** Writing a server-mode tag changes what every client reads over the wire, so a Python model can play the plant:

```python
from cowpoke import Process, inject_fault

def tank(cp, dt):
    out = 25 if cp.get("tank-plc.pump_run") else 0
    cp["tank-plc.level"] += (cp.get("tank-plc.inflow") - out) / 10 * dt

with Process(cp, tank, period=0.2):                          # runs while the block does
    with inject_fault(cp, "scada", tag="level", tag_fields={"register": 2}):
        ...                                                   # scada reads the wrong register; restored after
```

**Tests without hardware.** `pip install "cowpoke[test]"` adds a pytest fixture that starts a throwaway Cowpoke (the binary from `$COWPOKE_BIN`, `PATH` or `./target`):

```python
import pytest

@pytest.mark.cowpoke_config("examples/python-demo.yaml")
def test_level_reaches_scada(cowpoke_server):
    with cowpoke_server.client() as cp:
        cp["tank-plc.level"] = 61.5
        ...
```

`CowpokeServer` does the same outside pytest. `tests/test_live.py` exercises all of it.

## Demo: a tank control loop

`examples/python/tank_loop.py` plays a tank (fills from `inflow`, drains while the pump runs) against a simulated Modbus PLC, and runs a level controller that sees only a SCADA's view of it, polled over Modbus TCP. The pump switches at the high and low setpoints; change `scada.high_sp` or `scada.low_sp` while it runs and watch the trend follow.

```bash
COWPOKE_CONFIG=examples/python-demo.yaml COWPOKE_ADMIN_PASSWORD=dev-password cargo run
```

```bash
python examples/python/tank_loop.py --password dev-password
```
