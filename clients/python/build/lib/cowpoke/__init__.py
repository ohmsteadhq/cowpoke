"""Read, write and follow every Cowpoke tag from Python.

    from cowpoke import Cowpoke

    with Cowpoke("http://localhost:8080", user="admin", password="...") as cp:
        print(cp["plc1.Motor_Speed"])          # latest value, kept live
        cp["plc1.Motor_Speed"] = 50            # write, waits for the answer
        cp.write({"plc1.Run": True, "plc1.Setpoint": 72.5})
        for name, value in cp.watch("plc1.*"): # every change as it happens
            print(name, value)

One WebSocket (`/api/tags/ws`) carries the values, the changes and the writes, so
reads cost nothing and a write is one round trip. Names are `service.tag`.
"""
from .client import Cowpoke, CowpokeError, Tag

__all__ = ["Cowpoke", "CowpokeError", "Tag"]
