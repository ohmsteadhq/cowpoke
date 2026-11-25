---
title: Neuron alternative: free Modbus, OPC UA, S7 | Cowpoke
description: Open-source Neuron alternative: Modbus, OPC UA, EtherNet/IP, S7, MQTT and IO-Link free, each as client or simulator.
slug: neuron-alternative
---

# An open-source Neuron alternative

Cowpoke is an open-source (MIT) alternative to Neuron with every protocol in the free build: Modbus TCP, OPC UA, EtherNet/IP, Siemens S7, MQTT with Sparkplug B and IO-Link. Each one runs as a client to a real device or as a **simulator** of that device.

[Neuron](https://github.com/emqx/neuron) from EMQ is a capable industrial gateway that converts many protocols to MQTT. Its open-source edition includes the Modbus and MQTT drivers; the rest of its driver catalogue comes with a commercial licence.

| | Neuron (open-source edition) | Cowpoke |
|---|---|---|
| Licence | Open-source core; most drivers commercial | MIT, every protocol included |
| Modbus TCP | Yes | Client and simulator |
| OPC UA | Commercial driver | Client and simulator, secure policies |
| EtherNet/IP (Logix) | Commercial driver | Client and simulated Logix controller |
| Siemens S7 | Commercial driver | Client and simulated S7 PLC |
| MQTT / Sparkplug B | Yes | Client, built-in broker, Sparkplug edge node, and a client that reads edge nodes |
| IO-Link | — | Client and simulated ifm IoT Core master |
| Historian, alarms, dashboards | No; send to another system | Built in |
| Passive traffic monitor | No | Built in |
| Driver catalogue size | Much larger (commercial) | Seven protocols today |

## When Neuron is the better pick

- You need a protocol Cowpoke doesn't have yet: Modbus RTU over serial, BACnet, DNP3, or one of many vendor-specific drivers.
- You are publishing into EMQX at scale.
- You want a vendor's commercial support behind the gateway.
- You need protocol conversion: Neuron forwards device data to MQTT. Cowpoke's services each run on their own; it doesn't bridge one protocol into another today.

## When Cowpoke is the better pick

- You want the common plant-floor protocols without a licence.
- You want simulators of the same devices, so you can test before the hardware is wired.
- You want history, alarms and wire diagnostics in the same container instead of in three more systems.

The Neuron details were checked against its public repository in November 2026. Confirm what its current edition includes before you decide.

Free to self-host (MIT). [Docs](/docs) · [Source]([SOURCE URL])

Related: [Modbus TCP simulator](/modbus-tcp-simulator) · [OPC UA simulator](/opc-ua-simulator) · [Simulate an S7 PLC](/s7-plc-simulator)
