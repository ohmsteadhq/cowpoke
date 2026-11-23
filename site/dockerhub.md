<!-- Docker Hub overview for ohmsteadhq/cowpoke. Short description (100 chars max):
Industrial protocol simulator, client and historian: Modbus, OPC UA, EtherNet/IP, S7, MQTT, IO-Link -->

# Cowpoke

**Simulate, read and watch industrial devices from one container.** Cowpoke speaks Modbus TCP, OPC UA, EtherNet/IP (Logix), Siemens S7, MQTT with Sparkplug B, IO-Link and HTTP APIs, each as a **simulated device** or as a **client** to a real one. A historian, alarms, dashboards, a passive traffic monitor and insights are built in.

- About 70 MB, distroless, runs as a non-root user, with a healthcheck.
- Configure it in the web UI, over REST, or with one YAML file.
- Drive it from Python in real time (the client in `clients/python`): read and write every tag, run a process model, start a throwaway instance in pytest.
- Free under the AGPL-3.0. Ohmstead runs a paid hosted version.

## Quick start

```bash
docker run -d --name cowpoke -p 8080:8080 -p 5020:5020 -p 4840:4840 \
  -e COWPOKE_ADMIN_PASSWORD='pick-a-long-one' -v cowpoke-data:/data ohmsteadhq/cowpoke
```

Open http://localhost:8080 and sign in as `admin`. Each simulated device listens on the port its settings name, so publish those ports too, or use `--network host` on Linux.

## What it's for

- **A Modbus TCP or OPC UA simulator** for testing an HMI, SCADA or gateway before the hardware arrives.
- **A simulated Logix or S7 PLC** that answers tag reads by name and DB reads by address.
- **A Sparkplug B edge node or MQTT broker** with your tags, for testing an MQTT or Sparkplug host.
- **A wire monitor** on a SPAN port: who talks to which PLC, writes, program downloads, run/stop, response times, retransmissions.
- **CI for automation code**, with a fresh Cowpoke per test.

## Environment

| Variable | Default | |
|---|---|---|
| `COWPOKE_ADMIN_PASSWORD` | random, printed once in the log | Password for the first `admin` user |
| `COWPOKE_DATA` | `/data` | Config, users, certificates, history |
| `COWPOKE_CONFIG` | `/data/cowpoke.yaml` | Service configuration |
| `COWPOKE_BIND` | `0.0.0.0:8080` | Web UI and API |

Live capture for the traffic monitor needs root and the capture capabilities: `--user 0 --cap-add NET_RAW --cap-add NET_ADMIN --network host`. Run the monitor in its own container if the hub should stay unprivileged. Replaying pcap files needs none of that.

Tags: `latest`, `<version>`.

Docs: https://ohmstead.io/cowpoke/ · Source: https://github.com/ohmsteadhq/cowpoke
