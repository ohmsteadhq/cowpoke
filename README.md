# Cowpoke

**One container, many industrial communication services.** Cowpoke is an Ohmstead product.

Create *services* in a web UI, a REST API or a YAML file. Each service speaks one protocol, in the mode you pick:

| Protocol | Server mode (Cowpoke is the device) | Client mode (Cowpoke talks to a device) |
|---|---|---|
| **Modbus TCP** | Simulated device: holding/input registers, coils, discrete inputs; several unit ids on one port | Polls a device or gateway; several unit ids per service; writes coils and holding registers |
| **OPC UA** | Server with your tags as variables; None, Basic256Sha256, Aes128Sha256RsaOaep, Aes256Sha256RsaPss; anonymous and username logins; trust list | Reads and writes a server's nodes with any of the same policies, as anonymous or a user |
| **API request** | Mock HTTP API: routes with canned bodies, `:params`, delays, status codes, and `{{tags.NAME}}` placeholders; routes can set tags from the request; plus `GET/PUT /tags` | Calls an endpoint on a timer and picks values from the JSON with JSON pointers; optional write request |
| **IO-Link** | Simulated ifm IoT Core master: `pdin`/`pdout`, device info, `getdatamulti` | Reads sensors through a real ifm IoT Core master (AL13xx and similar) by IODD bit offset; writes `pdout` |
| **MQTT** | Built-in MQTT 3.1.1 broker: publishes every tag on change to `<prefix>/<tag>` (retained), takes writes on `<topic>/set`, optional login; optionally a **Sparkplug B edge node** (or device): NBIRTH/DBIRTH, data on change by alias, NDEATH, writes and rebirth through NCMD/DCMD, on its own broker and optionally an outside one (with an NDEATH will); other clients publish and subscribe as with any broker | Subscribes to a broker's topics (wildcards allowed), picks values from JSON or text payloads with JSON pointers; writes publish to a topic template; TLS, login, QoS 0/1, retain. **Sparkplug B**: reads an edge node's or device's metrics from births and data (aliases included), marks tags bad on death, asks for a rebirth, writes with NCMD/DCMD |
| **EtherNet/IP** | Simulated Logix controller: Read Tag / Write Tag by symbolic name, direct or via Unconnected Send to any slot, Multiple Service Packet, ListIdentity | Reads and writes ControlLogix / CompactLogix tags by symbolic name (`Program:Main.Level`, `Tank[3].Temp`): BOOL, SINT, INT, DINT, LINT, REAL, LREAL, STRING |
| **Siemens S7** | Simulated S7 PLC: S7comm variable reads and writes (PUT/GET) on DBs, M, I and Q for any rack and slot; program transfer and run/stop refused | Reads and writes S7-300/400/1200/1500 by Siemens address (`DB1.DBD0`, `DB1.DBX4.0`, `MW10`), batching items per PDU |
| **Traffic monitor** | — | Passive: watches a mirror (SPAN) port or replays a pcap/pcapng file, decodes Modbus TCP, EtherNet/IP (CIP), S7comm, OPC UA (unencrypted) and MQTT, and keeps an inventory of who talks to which device plus events: writes, program transfers, run/stop, logins, errors, new talkers |

Every service runs on its own. Cowpoke is not a bridge between them.

The rest comes with every service:

- **Historian.** Any numeric tag can be recorded on change past a deadband, or on a fixed interval, with its own retention. Trends show in the UI and come from `GET /api/services/<id>/history/<tag>`.
- **Accounts.** Three roles:
  - **Viewer** reads everything.
  - **Operator** also writes values and starts or stops services.
  - **Admin** also edits services, users and certificates.
- **One binary.** The web UI is built into the binary. Data lives in one folder.

Cowpoke is **free to self-host** under the GNU AGPL v3. Ohmstead runs a paid hosted version.

## Run it

```bash
docker build -t cowpoke .
docker run -d --name cowpoke -p 8080:8080 -p 5020:5020 -p 4840:4840 \
  -e COWPOKE_ADMIN_PASSWORD='pick-a-long-one' -v cowpoke-data:/data cowpoke
```

Open http://localhost:8080 and sign in as `admin`. If you don't set `COWPOKE_ADMIN_PASSWORD`, Cowpoke generates a password once and prints it in the log (`docker logs cowpoke`).

Each server-mode service listens on the port its settings name, so publish those ports (`-p`). On Linux with many services, `--network host` is simpler. There is also a [`docker-compose.yml`](docker-compose.yml).

To try every protocol at once, start with the demo. It has four simulated devices, and a client reading each one back:

```bash
COWPOKE_CONFIG=examples/demo.yaml COWPOKE_ADMIN_PASSWORD=dev-password cargo run
```

For something closer to a real plant, `examples/plant.yaml` has four machines on four protocols: a Modbus pump station (a PLC and two drives on unit ids 1 to 3, with tag names from Ohmstead's Pump and Tank project), an OPC UA boiler house, a packaging line on an MQTT broker and an EtherNet/IP filler. Each one has a client reading it back, like a SCADA, HMI, MES or gateway would.

### Environment

| Variable | Default | |
|---|---|---|
| `COWPOKE_DATA` | `./data` (`/data` in Docker) | Config, users, certificates and history |
| `COWPOKE_CONFIG` | `$COWPOKE_DATA/cowpoke.yaml` | The service configuration file |
| `COWPOKE_BIND` | `0.0.0.0:8080` | Web UI and API address |
| `COWPOKE_ADMIN_PASSWORD` | random, logged once | Password for the first `admin` user |
| `COWPOKE_LOG` | `info` | Log filter, for example `debug` or `cowpoke=debug` |
| `COWPOKE_HUB` | | Agent only: the hub's URL (`https://…`) |
| `COWPOKE_AGENT_TOKEN` | | Agent only: the token the hub showed when the agent was created |

The web UI has no HTTPS of its own. Off a trusted network, put it behind a reverse proxy (Caddy, Traefik, nginx) that terminates TLS.

## Define services

The web UI builds services with a form for each protocol and mode, with checks as you type and a **Test** button that runs the service for a few seconds without saving; its YAML tab edits the same definition. A service is `id`, `name`, `kind` (`modbus`, `opcua`, `api`, `iolink`, `mqtt`, `eip`, `s7`, `monitor`), `mode` (`server` or `client`), `settings` and `tags`. The UI's **New service** page starts from a commented template for each kind and mode; they're also in [`templates/`](templates).

```yaml
services:
  - id: plc-sim
    name: PLC simulator
    kind: modbus
    mode: server
    settings: { port: 5020, word_order: big }
    tags:
      - name: tank_level
        datatype: float32          # bool, int16, uint16, int32, uint32, int64, uint64, float32, float64, string
        register: 0                # zero-based: 40001 is holding register 0
        initial: 42.5
        history: { mode: deadband, deadband: 0.5, retention_days: 30 }
      - { name: flow, datatype: int16, table: input_register, register: 10, scale: 0.1, unit: 2 }
```

These tag fields work for every kind: `datatype`, `initial`, `writable`, `scale`/`offset` (engineering = raw × scale + offset), `units`, `description` and `history`.

The address fields depend on the kind:

- **Modbus:** `unit`, `table`, `register`, and `length` (strings only, in registers).
- **OPC UA:**
  - Client mode: `node_id` (`ns=2;s=Line1.Temp`).
  - Server mode: optional `node_id` string id. By default the tag name is used, in Cowpoke's own namespace.
- **API:** `pointer`, a JSON pointer such as `/main/temp`. The default is `/<tag name>`.
- **MQTT:**
  - Client mode: `topic` (a filter; `+` and `#` work) and optional `pointer` into a JSON payload. Non-JSON payloads are read as text.
  - Client mode with `sparkplug` (`group_id`, `edge_node`, optional `device`): `metric`, the Sparkplug metric name. The default is the tag name.
  - Server mode with `sparkplug` (`group_id`, `edge_node`, optional `device`): optional `metric`, the Sparkplug metric name. The default is the tag name. Plain topics are published as well. Add `broker: { host, port, tls, ca, username, password, client_id }` under `sparkplug` to also publish to an outside broker.
  - Server mode: optional `topic`. By default the tag publishes to `<prefix>/<tag name>`; a write is a publish to `<topic>/set`.
- **EtherNet/IP:** `cip_tag`, the Logix tag name (`Program:Main.Level`). Required in client mode; in server mode it defaults to the tag name. Pick a Cowpoke datatype that holds the value (BOOL → `bool`, SINT/INT → `int16`, DINT → `int32`, LINT → `int64`, REAL → `float32`, LREAL → `float64`, STRING → `string`); client writes use the type the controller reports.
- **Siemens S7:** `s7_address` in Siemens notation (`DB1.DBD0`, `DB1.DBX4.0`, `DB1.DBW6`, `MW10`, `I0.1` or `E0.1`, `QW4` or `AW4`), and `length` (characters) for strings. The address width must fit the datatype: bool at X, int16/uint16 at W, int32/uint32/float32 at D, 64-bit types and strings at B (or D). Values are big-endian; `scale` and `offset` work as for Modbus. S7-1200/1500 need "optimized block access" off on the data blocks and PUT/GET allowed in the CPU's protection settings.
- **IO-Link:** `port`, `pd` (`in` or `out`), `bit_offset` and `bit_length`. Bit offsets are IODD-style: counted from the least significant bit of the last byte.

What you write in the UI or the API depends on the mode:

- **Server mode:** the value is stored and served at once.
- **Client mode:** the value goes to the device, and Cowpoke shows it once the device takes it.

### OPC UA certificates

Each OPC UA service has its own certificate store under `<data>/services/<id>/pki`. A certificate Cowpoke hasn't seen before is saved as *rejected*. An admin trusts it from the service's **Certificates** tab, and the next connection goes through. You can skip this with `trust_client_certs: true` (server) or `trust_server_certs: true` (client), but only on networks you trust.

## REST API

Sign in with `POST /api/login {"username","password"}`. It returns a `token`; send it as `Authorization: Bearer <token>`. The browser uses a cookie instead. Main routes:

| Route | Role |
|---|---|
| `GET /api/services`, `GET /api/services/<id>` | viewer |
| `POST /api/services` (JSON or YAML), `PUT /api/services/<id>`, `DELETE /api/services/<id>` | admin |
| `POST /api/services/check?editing=<id>`: parse and check a service without saving; returns `definition`, `yaml`, `errors`, `warnings` (unknown settings, id clashes, port clashes, overlapping Modbus registers) | admin |
| `POST /api/services/test?editing=<id>`: run a service for up to 8 s without saving; returns `state`, `detail` and the first value of each tag | admin |
| `POST /api/services/<id>/start` · `/stop` · `/restart` | operator |
| `GET /api/services/<id>/tags`, `PUT /api/services/<id>/tags/<tag> {"value": …}` | viewer / operator |
| `GET /api/services/<id>/history/<tag>?from=…&to=…&points=600` | viewer |
| `GET /api/events` (server-sent events: `tag`, `status`) | viewer |
| `GET /api/config` (export YAML), `PUT /api/config` (import, replaces all) | admin |
| `POST /api/import/ohmstead`: tags from an Ohmstead project's PLC I/O (see below) | admin |
| `GET /api/users`, `PUT /api/users/<name> {"password","role"}`, `DELETE /api/users/<name>` | admin (anyone can change their own password) |
| `GET /api/services/<id>/certs`, `POST /api/services/<id>/certs/<file>/trust` · `reject` · `delete` | admin |

API server-mode services without their own `port` answer at `/x/<id>/…` on the web port, with no sign-in, because they stand in for third-party APIs.

## Import from Ohmstead

Draw the panel in [Ohmstead](https://ohmstead.io), then test its I/O here without retyping a tag. Ohmstead lists each wired digital or analog channel of a PLC or I/O module as an *I/O point*, named by the field device on it (`LT-101`, or `K1.A1` when a device has several). Cowpoke turns the points into tags on one service.

In the service builder, open **Import from Ohmstead** above the tag table, then either:

- **Paste the export.** Ask your assistant to run `export_io_points` on the project through the Ohmstead MCP, and paste the JSON.
- **Fetch from Ohmstead.** Give the project id and a personal access token (Ohmstead → Settings → MCP access tokens). The address defaults to `https://mcp.ohmstead.io/mcp`; point it at your own Ohmstead MCP if you host one. Cowpoke uses the token for that one call and doesn't store it.

**Preview** shows each tag, its address and whether it's new, updated or unchanged. **Add** puts them in the form; nothing is saved until you save the service.

The mapping rules are fixed:

| Service | Digital in | Digital out | Analog in | Analog out |
|---|---|---|---|---|
| Modbus | discrete input, `bool` | coil, `bool` | input register, `float32` | holding register, `float32` |
| OPC UA | `bool` | `bool` | `float32` | `float32` |
| IO-Link | pd `in`, 1 bit | pd `out`, 1 bit | pd `in`, `uint16` | pd `out`, `uint16` |
| Others | `bool` | `bool` | `float32` | `float32` |

- **Modbus:** addresses count up in each table from *First address* (default 0). A float32 takes two registers.
- **OPC UA client:** node ids are `ns=<index>;s=<prefix><name>` (index 2, no prefix by default). Server tags use Cowpoke's default node ids.
- **IO-Link:** each device gets its own port, from *First port* up to the master's `ports` (default 8). Its points share that port and stack bit offsets from 0. Devices past the last port are skipped and listed.
- Every tag gets a description (`device terminal → module:terminal`) and the units of the P&ID range, if the project has one. Outputs are writable. Inputs are writable only on server-mode services, where the simulator sets them.
- **Importing again** matches tags by name. An existing tag keeps its address, history, simulation and scale. New points get the next free address. Tags that aren't in the project are left alone, and the preview lists them.

From a script, send the export (an object, or the text) or a source:

```bash
curl -X POST localhost:8080/api/import/ohmstead -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"service":"plc-sim","io":'"$(cat io.json)"',"options":{"start":100},"apply":true}'
```

| Field | |
|---|---|
| `service` | An existing service's id. Or send `definition` (a whole service, unsaved) to get the merged one back. |
| `io` | The `ohmstead-io` JSON, a bare array of points, or the MCP tool reply. |
| `source` | Or `{"url","token","project_id"}` to fetch it from Ohmstead. |
| `options` | `start`, `unit` (Modbus), `ns`, `prefix` (OPC UA client), `port_start` (IO-Link). All optional. |
| `apply` | Save into `service`. Without it, Cowpoke only previews. |

The reply has `preview` (`added`, `updated`, `unchanged`, `not_in_import`, `skipped`, `rows`), `definition` and `saved`.

## Edge agents

A hub (hosted, or your own central Cowpoke) often can't reach the devices it should talk to: they sit on a plant network behind a firewall. An **edge agent** is the same Cowpoke binary running on that network as `cowpoke agent`. It dials out to the hub over one WebSocket, so the plant needs no inbound firewall rules, and it runs the services the hub assigns to it.

1. On the hub, an admin creates the agent: `POST /api/agents {"id": "plant-1", "name": "Plant 1"}`, or the Agents page. The answer holds the agent's **token**, shown only once.
2. At the plant:

   ```bash
   docker run -d --name cowpoke-agent --network host -v cowpoke-agent:/data \
     -e COWPOKE_HUB=https://cowpoke.example.com -e COWPOKE_AGENT_TOKEN=<token> cowpoke agent
   ```

3. Assign services to it with `agent: plant-1` in their definition. Client and server services can both run on an agent.

How it behaves:

- **Live values and status** flow up to the hub every 200 ms and 2 s. Writes, start/stop and certificate actions on the hub go down to the agent.
- **The plant keeps running when the link drops.** The agent keeps polling and serving, and starts from its last configuration (`agent-config.yaml`) even if it boots while the hub is unreachable. On the hub the agent's tags turn bad and its services show "agent offline since …".
- **History is backfilled.** The agent records historized tags locally the whole time. When it reconnects it sends the points recorded while it was offline (up to 7 days) to the hub's historian.
- **Tokens** are stored on the hub only as SHA-256 hashes. Rotating one (`POST /api/agents/<id>/token`) disconnects the agent until it gets the new token. An agent can't be deleted while services are assigned to it.

| Route | Role |
|---|---|
| `GET /api/agents`: `id`, `name`, `online`, `version`, `hostname`, `connected_since`, `last_seen`, `created`, `services` | viewer |
| `POST /api/agents {"id","name"}` → `{"id","token"}` | admin |
| `POST /api/agents/<id>/token` → `{"id","token"}` | admin |
| `DELETE /api/agents/<id>` | admin |
| `GET /api/agents/connect` (WebSocket, `Authorization: Bearer <agent token>`) | the agent |

## Boards, alarms and hooks

**Boards** are dashboards you build in the UI: a 12-column grid of widgets that everyone sees, edited by operators and admins. They're stored in `<data>/dashboards.json`.

| Widget | Shows |
|---|---|
| Trend | Up to 8 tags on one time chart (15 min to 30 days) from the historian, with the live values added on as they arrive |
| Value | One big number with units and when it last changed |
| Gauge | A dial with its scale and the tag's high and low alarm limits marked |
| Lamp | A bool as a light, optionally with a button so operators can switch it |
| Alarms | The active and unacknowledged alarms, optionally for one service, with Ack buttons |

**Alarms** watch one tag each. Each alarm has one of these conditions:

| Condition | Raises when |
|---|---|
| `high` / `low` | The value is past `limit`. `deadband` sets how far back it must go before the alarm clears |
| `equals` | The value equals `value` (a fault bit, a mode number) |
| `bad_quality` | The tag goes bad: the device is unreachable or the edge agent is offline |
| `stale` | The tag hasn't updated for `stale_s` seconds |
| `rate` | The value changes faster than `rate_per_min`, in either direction |

- `delay_s` makes the condition hold for that many seconds before the alarm raises.
- Each alarm has a severity: `low`, `medium`, `high` or `critical`.
- A raised alarm stays listed until it has both cleared and been acknowledged. Operators acknowledge alarms.
- Values that come in with bad quality never raise or clear a limit alarm.
- Services on edge agents get alarms too, because alarms are evaluated on the hub.
- Every raise, clear and acknowledge goes to `<data>/alarm-log.jsonl`. The current alarm states are kept in `alarm-state.json`, so a restart doesn't send the same notifications again.

**Hooks** run when an alarm raises, clears or is acknowledged:

| Kind | Does |
|---|---|
| `webhook` | Sends a request to a URL: the alarm event as JSON, or your own body. Works for Slack, Teams, Discord, PagerDuty, n8n and Node-RED |
| `write` | Writes `value` to any tag when the alarm raises, and `clear_value` when it clears, for example a horn or a stack light |
| `email` | Sends mail through SMTP (STARTTLS, TLS or none) |
| `mqtt` | Publishes to a broker topic, at QoS 0 or 1, optionally retained |

- Bodies, subjects, topics and URLs can use placeholders: `{{name}}`, `{{alarm}}`, `{{event}}`, `{{severity}}`, `{{message}}`, `{{value}}`, `{{service}}`, `{{tag}}`, `{{time}}` and `{{user}}`. A JSON string that is just `"{{value}}"` is sent as the value itself, with its type.
- A failed hook is retried twice.
- The Hooks tab shows each hook's last result and has a **Test** button.

Alarms and hooks are saved in `<data>/alarms.yaml`. Download or import that file from the Alarms tab, or use `GET`/`PUT /api/alarms/config`.

```yaml
alarms:
  - { id: tank-high, name: Tank level high, service: tank, tag: level, kind: high, limit: 80, deadband: 3, severity: high, hooks: [horn, chat] }
hooks:
  - { id: horn, kind: write, service: tank, tag: horn, value: true, clear_value: false }
  - { id: chat, kind: webhook, url: "https://hooks.slack.com/services/…", body: { text: "{{message}}" }, on: [raise, clear] }
```

`alarms.yaml` holds SMTP and MQTT passwords as plain text, so keep the data volume private.

| Route | Role |
|---|---|
| `GET /api/alarms` (definitions with `state` and current value), `GET /api/alarms/log?limit=`, `GET /api/alarms/events` (server-sent events) | viewer |
| `POST /api/alarms/<id>/ack`, `POST /api/alarms/ack-all` | operator |
| `POST /api/alarms`, `PUT`/`DELETE /api/alarms/<id>`, `GET`/`PUT /api/alarms/config` | admin |
| `GET`/`POST /api/hooks`, `PUT`/`DELETE /api/hooks/<id>`, `POST /api/hooks/<id>/test` (an optional body tries an unsaved hook) | admin |
| `GET /api/dashboards`, `GET /api/dashboards/<id>` | viewer |
| `POST /api/dashboards`, `PUT`/`DELETE /api/dashboards/<id>` | operator |

## Historian storage

History lives under `<data>/history/<service>/<tag>/`, one set of files per UTC day:

| File | What it holds |
|---|---|
| `<day>.bin` | Points as they arrive: 16 bytes each (i64 milliseconds, f64 value), appended once a second |
| `<day>.gor` | A finished day, compressed Gorilla-style: timestamps as delta-of-deltas, values XORed with the previous one |
| `<day>.hour` | That day's hourly rollups: count, min, max, average, first and last value per hour |

- **Compaction:** finished days are compacted at startup and every hour after. A point that arrives late for a compacted day, such as an edge agent's backfill, goes into a new `.bin` beside the `.gor`. It's readable right away and merged at the next compaction.
- **Compression:** a sensor whose value changes every second compresses about 4×. A steady value, such as a setpoint or a bool, compresses about 60×. Values recorded on change take less space still.
- **Queries:** `?points=` sets the most points to return. If the raw points fit, they're returned as they are. If not, they're grouped into buckets with an average, min and max. Buckets an hour wide or wider are built from the hourly rollups, so a month-long trend reads about 720 rollups instead of millions of points. A bucket includes every whole hour that overlaps the range.
- **Retention** deletes whole days. String tags aren't recorded.

## Develop

```bash
cargo test     # unit tests, plus a client-to-server loopback test for every protocol (OPC UA in three security modes)
cargo run      # data in ./data
```

The code is laid out like this:

- `src/services/`: one file per protocol, each with `server` and `client` modes.
- `src/manager.rs`: starts and supervises services, restarts them with backoff, and routes writes.
- `src/web.rs`: the REST API.
- `ui/`: the web UI, plain JS, embedded with `include_str!`.

The UI follows the Ohmstead design language (`ohmstead/design/DESIGN.md`).

## Traffic monitor

A `monitor` service (mode `client`) listens without sending anything. Point it at a mirror (SPAN) port with `interface`, or replay a capture with `file`. It reassembles TCP streams and decodes Modbus TCP (including Schneider UMAS program and run/stop functions), EtherNet/IP and CIP (tag reads and writes, Forward Open, reset/start/stop, file transfers, inside Unconnected Send and Multiple Service Packets), S7comm (variable reads and writes, block download and upload, PLC stop and run, passwords, SZL reads), OPC UA binary (services, when the channel isn't encrypted) and MQTT (connects, logins, publishes, subscriptions, refused connections).

The **Traffic** tab lists talkers (client → device, protocol, counts) and events. Reads are counted, not listed; a repeated event from the same talker is listed once a minute. Talkers first seen after `learn_minutes` are flagged as new. The counters (`talkers`, `devices`, `events`, `writes`, `program_changes`, `control_actions`, `logins`, `errors`, `new_talkers`, `last_event`) are tags: record them, alarm on them, put them on boards. `GET /api/services/<id>/monitor` returns the inventory and events.

Live capture in Docker needs `--cap-add NET_RAW --network host` (or the mirror interface passed into the container). Builds without the default `live-capture` feature (no libpcap) can only replay files.

This is change and anomaly monitoring for the industrial traffic it sees, not a certified intrusion detection system.

## Insights

Cowpoke watches every tag update and samples every service's status, building a baseline per tag: how often it's read and how often it actually changes, bad reads, range, typical step size and the time between changes. Every 15 minutes (or on **Analyze now**) it turns the baseline, the service definitions and the alarm log into insights on the **Insights** page:

- **Communications:** a client polling far faster than any of its tags change (with a poll interval to use), connections that keep dropping, tags that read bad intermittently, live measurements that never change.
- **Historian:** deadbands smaller than a tag's noise, and interval recording of tags that rarely change.
- **Alarms:** chattering, floods (with the alarms behind them), standing alarms and alarms nobody acknowledges.

A service's page lists its open insights under its status and marks the tags they concern. Insights that name an exact setting (a poll interval, a history deadband) have **Apply…**: it opens the service's Settings with the change filled in, and nothing changes until someone saves. Insights are suggestions with evidence; nothing changes until someone changes it. Dismiss one and it stays dismissed. Values typed in the UI and simulator generators don't count toward the baseline. The baseline is saved in `baselines.json` in the data folder. Most insights need 30 minutes of watching, some need hours; `COWPOKE_INSIGHTS_QUICK=1` shortens that to minutes for demos. `GET /api/insights`, `POST /api/insights/run`, and `POST`/`DELETE /api/insights/<id>/dismiss`.

## Not yet

These aren't built yet:

- Modbus RTU over serial
- IO-Link masters other than ifm IoT Core
- OPC UA subscriptions (the client polls)
- Built-in HTTPS
- S7: S7comm-plus (symbolic access on S7-1200/1500 with optimized blocks), userdata services (SZL, block lists) in the simulator
- Traffic monitor: S7comm-plus (S7-1200/1500) and encrypted OPC UA contents, DNP3, PROFINET, out-of-order TCP reassembly
- MQTT: Sparkplug datasets and templates, Sparkplug primary-host STATE awareness and a new bdSeq per reconnect (it changes per start), MQTT 5, TLS on the built-in broker, persistent sessions and QoS 2 delivery (QoS 2 publishes are accepted and passed on at QoS 1)
- EtherNet/IP: connected (Forward Open) messaging, arrays beyond one element, UDTs other than STRING, tag browsing, batching client reads into Multiple Service Packets

## License

GNU Affero General Public License v3.0 or later. See [LICENSE](LICENSE). If you run a modified Cowpoke as a service for others, you must offer them its source.
