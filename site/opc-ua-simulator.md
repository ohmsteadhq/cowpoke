---
title: OPC UA server simulator in Docker
description: A free OPC UA server simulator in one Docker container, with secure policies, user logins, simulated values and an OPC UA client in the same box. Scriptable from Python, usable in CI.
slug: opc-ua-simulator
---

# OPC UA server simulator in Docker

Cowpoke is a free OPC UA server simulator that runs in one Docker container. You start it from a script or a CI job; there is no Windows installer.

```bash
docker run -d -p 8080:8080 -p 4840:4840 -e COWPOKE_ADMIN_PASSWORD=change-me ohmstead/cowpoke
```

Define the server and its tags in the web UI at http://localhost:8080, or as YAML:

```yaml
services:
  - id: boiler
    kind: opcua
    mode: server
    settings: { port: 4840, hostname: localhost }
    tags:
      - { name: Boiler1.SteamPressure, datatype: float64, units: bar, simulate: { kind: walk, min: 7.5, max: 9.5, step: 0.05 } }
      - { name: Boiler1.State, datatype: string, simulate: { kind: sequence, interval_ms: 30000, values: [Purge, Ignition, Firing, Standby] } }
      - { name: Boiler1.PressureSP, datatype: float64, units: bar, initial: 8.5 }
```

Your client connects to `opc.tcp://<host>:4840/` and finds the tags under `ns=2;s=<name>`.

## Security like a real server

- **Policies:** None, Basic256Sha256, Aes128Sha256RsaOaep and Aes256Sha256RsaPss.
- **Logins:** anonymous, and users with a username and password.
- **A trust list per service.** An unknown client certificate is rejected until an admin trusts it, as on a real server. On a test network you can turn that off.

## Built for automated testing

- **One container per test run**, configured from YAML. No GUI to click through.
- **Python:** set values, follow changes and inject faults from a test. The `cowpoke_server` pytest fixture starts a throwaway server for each test.
- **An OPC UA client in the same box.** Use it to read your simulated server back, or a real one, with the same security options.

## Beyond OPC UA

The same container simulates Modbus TCP, EtherNet/IP (Logix), Siemens S7, MQTT with Sparkplug B and IO-Link. It also records history, raises alarms and watches the traffic on the wire.

## Compared with desktop simulators

Desktop simulators such as Prosys and Integration Objects are mature and cover more of the OPC UA model: events, methods, history access. Cowpoke's OPC UA server serves variables only, and its client polls; it does not subscribe. If you need those parts of the model, use a desktop simulator. Pick Cowpoke when you want the server in Docker, driven from code, next to the other protocols on your floor.

Free to self-host (AGPL-3.0). [Docs](/docs) · [Source]([SOURCE URL])
