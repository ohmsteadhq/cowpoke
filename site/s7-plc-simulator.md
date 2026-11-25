---
title: Simulate a Siemens S7 PLC without hardware | Cowpoke
description: Simulate a Siemens S7 PLC on the network in Docker: S7comm reads and writes on DBs, M, I and Q. A free PLCSIM complement.
slug: s7-plc-simulator
---

# Simulate a Siemens S7 PLC without the hardware

Cowpoke simulates a Siemens S7 PLC on the network, in Docker, with no hardware, no TIA Portal licence and no Windows VM. Your HMI, SCADA, historian or gateway connects to it and reads and writes DBs, M, I and Q over S7comm.

It complements PLCSIM; it does not replace it. S7-PLCSIM and PLCSIM Advanced run your **PLC program** in simulation inside the Siemens toolchain, and that is the right tool for testing logic. Cowpoke is for testing the other side: whatever reads the PLC over the network needs something that **answers like the PLC on the wire**.

Define the PLC as a service:

```yaml
services:
  - id: line1-plc
    kind: s7
    mode: server
    settings: { port: 102 }
    tags:
      - { name: tank_level, datatype: float32, s7_address: DB1.DBD0, simulate: { kind: sine, min: 30, max: 70, period_ms: 60000 } }
      - { name: pump_run, datatype: bool, s7_address: DB1.DBX4.0 }
      - { name: batch_count, datatype: int16, s7_address: MW10 }
```

Run the container with the S7 port published:

```bash
docker run -d -p 8080:8080 -p 102:102 -e COWPOKE_ADMIN_PASSWORD=change-me ohmsteadhq/cowpoke
```

## What the simulated PLC does

- Answers S7comm PUT/GET variable reads and writes on **DBs, M, I and Q**, for **any rack and slot**.
- Moves values on its own (waves, walks, ramps, sequences), or takes them from your Python test.
- Refuses program transfer and run/stop, like a protected PLC.

It does **not** run ladder or SCL, and it speaks classic S7comm, not S7comm-plus. To test the program itself, use PLCSIM.

## And the client side

The same container reads and writes real S7-300/400/1200/1500 PLCs by Siemens address (`DB1.DBD0`, `DB1.DBX4.0`, `MW10`), batching items per PDU. It also watches S7 traffic on a mirror port: variable writes, block downloads, stop and run, password attempts.

## PLCSIM and Cowpoke side by side

| | S7-PLCSIM / Advanced | Cowpoke |
|---|---|---|
| Runs your PLC program | Yes | No |
| Answers S7 reads and writes on the network | Advanced, or with NetToPLCsim | Yes |
| Needs TIA Portal / STEP 7 | Yes | No |
| Runs in Docker, scriptable from Python | No | Yes |
| Other protocols in the same box | No | Modbus, OPC UA, EtherNet/IP, MQTT, IO-Link |
| Price | Licensed | Free (MIT) |

Free to self-host (MIT). [Docs](/docs) · [Source]([SOURCE URL])

Related: [Modbus TCP simulator](/modbus-tcp-simulator) · [OPC UA simulator](/opc-ua-simulator) · [Neuron alternative](/neuron-alternative)
