"""Runs against a real Cowpoke started by the cowpoke_server fixture (needs the binary)."""
import os
import time

import pytest

from cowpoke import CowpokeError, Process, inject_fault

DEMO = os.path.join(os.path.dirname(__file__), "..", "..", "..", "examples", "python-demo.yaml")


def wait_for(fn, timeout=10):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if fn():
            return True
        time.sleep(0.1)
    return False


@pytest.mark.cowpoke_config(DEMO)
def test_read_write_and_watch(cowpoke_server):
    with cowpoke_server.client() as cp:
        assert "scada.level" in cp
        cp["tank-plc.level"] = 61.5
        assert wait_for(lambda: abs((cp.get("scada.level") or 0) - 61.5) < 0.01)
        with pytest.raises(CowpokeError):
            cp["scada.level"] = 1  # read-only on the client


@pytest.mark.cowpoke_config(DEMO)
def test_services_config_history(cowpoke_server):
    with cowpoke_server.client() as cp:
        ids = {s["id"] for s in cp.services()}
        assert {"tank-plc", "scada"} <= ids
        assert "tank-plc" in cp.export_config()
        for v in (10, 20, 30):
            cp["tank-plc.level"] = v
            time.sleep(0.5)
        h = cp.history("scada.level", since="5m")
        assert len(h) >= 2 and h["value"].max() >= 29
        cp.restart("scada")
        assert wait_for(lambda: cp.service("scada")["status"]["state"] == "running")


@pytest.mark.cowpoke_config(DEMO)
def test_process_and_fault(cowpoke_server):
    with cowpoke_server.client() as cp:
        def tank(cp, dt):
            cp["tank-plc.level"] = round((cp.get("tank-plc.level") or 0) + 5 * dt, 2)

        before = cp.get("tank-plc.level")
        with Process(cp, tank, period=0.1) as p:
            assert wait_for(lambda: (cp.get("scada.level") or 0) > (before or 0) + 2)
        assert not p.errors
        with inject_fault(cp, "scada", tag="level", tag_fields={"register": 2}):
            assert wait_for(lambda: abs(cp.get("scada.level") - cp.get("scada.inflow")) < 0.01)
        assert cp.service("scada")["definition"]["tags"][0]["register"] == 0


@pytest.mark.cowpoke_config(DEMO)
def test_alarms_and_insights(cowpoke_server):
    with cowpoke_server.client() as cp:
        assert isinstance(cp.alarms(), list)
        assert isinstance(cp.alarm_log(5), list)
        cp.ack_all()
        ins = cp.insights()
        assert isinstance(ins, (dict, list))
