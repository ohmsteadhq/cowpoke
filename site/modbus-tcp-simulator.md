---
title: Modbus TCP simulator in Docker
description: A free Modbus TCP simulator in one Docker container. Several unit ids on one port, simulated values, a web UI, REST and Python APIs, and a Modbus client in the same box to read it back.
slug: modbus-tcp-simulator
---

# Modbus TCP simulator in Docker

Cowpoke is a free Modbus TCP simulator that runs in one Docker container. Start it, and your HMI, SCADA or gateway has a Modbus device to poll before the real one exists.

```bash
docker run -d -p 8080:8080 -p 5020:5020 -e COWPOKE_ADMIN_PASSWORD=change-me ohmstead/cowpoke
```

Open http://localhost:8080, add a **Modbus TCP** service in **server** mode, and give it tags. Or write the same thing as YAML:

```yaml
services:
  - id: pump-plc
    kind: modbus
    mode: server
    settings: { port: 5020 }
    tags:
      - { name: tank_level, datatype: float32, register: 0, units: "%", simulate: { kind: sine, min: 30, max: 70, period_ms: 60000 } }
      - { name: pump_run, datatype: bool, table: coil, register: 0 }
      - { name: flow, datatype: int16, table: input_register, register: 10, scale: 0.1, simulate: { kind: walk, min: 8, max: 16, step: 0.4 } }
      - { name: drive_speed, datatype: uint16, register: 0, unit: 2 }
```

## What it simulates

- **All four tables:** holding registers, input registers, coils and discrete inputs.
- **Several unit ids on one port.** One container can stand in for a PLC and the drives behind a gateway.
- **Data types and word order:** int16, uint16, int32, float32 and more, with big or little word order per service, and scale and offset per tag.
- **Moving values:** sine, triangle, square, random walk, ramps and sequences, so trends and alarms have something to show.
- **Writes:** when your HMI writes a coil or register, the simulator keeps the value and shows it in the UI.

## What comes with it

- **A Modbus client in the same box.** Add a client-mode service to read your simulator back, or to read a real device, with polling, timeouts and writes.
- **Historian, alarms and boards** for any tag.
- **Wire diagnostics.** The traffic monitor and the insights catch common mistakes: unanswered requests and retransmissions on the wire, and one register read under two names with a different scale or datatype.
- **Scripting.** Drive the simulator from Python with `cp["pump-plc.tank_level"] = 61.5`, or run a whole process model with `Process`. A pytest fixture starts a throwaway Cowpoke for CI.

## Compared with single-purpose Modbus simulators

A small Modbus-only image is fine when all you need is a register to poll. Cowpoke is for when the test grows: more devices, other protocols (OPC UA, EtherNet/IP, S7, MQTT, IO-Link), recorded history, and checks that tell you why a value is wrong.

Free to self-host (AGPL-3.0). [Docs](/docs) · [Source]([SOURCE URL])
