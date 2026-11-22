# Cowpoke

**One container, many industrial communication services.** Cowpoke is an Ohmstead product.

Create *services* in a web UI, a REST API or a YAML file. Each service speaks one protocol, in the mode you pick:

| Protocol | Server mode (Cowpoke is the device) | Client mode (Cowpoke talks to a device) |
|---|---|---|
| **Modbus TCP** | Simulated device: holding/input registers, coils, discrete inputs; several unit ids on one port | Polls a device or gateway; several unit ids per service; writes coils and holding registers |
| **OPC UA** | Server with your tags as variables; None, Basic256Sha256, Aes128Sha256RsaOaep, Aes256Sha256RsaPss; anonymous and username logins; trust list | Reads and writes a server's nodes with any of the same policies, as anonymous or a user |
| **API request** | Mock HTTP API: routes with canned bodies, `:params`, delays, status codes, and `{{tags.NAME}}` placeholders; routes can set tags from the request; plus `GET/PUT /tags` | Calls an endpoint on a timer and picks values from the JSON with JSON pointers; optional write request |
| **IO-Link** | Simulated ifm IoT Core master: `pdin`/`pdout`, device info, `getdatamulti` | Reads sensors through a real ifm IoT Core master (AL13xx and similar) by IODD bit offset; writes `pdout` |

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

A service is `id`, `name`, `kind` (`modbus`, `opcua`, `api`, `iolink`), `mode` (`server` or `client`), `settings` and `tags`. The UI's **New service** page starts from a commented template for each kind and mode; they're also in [`templates/`](templates).

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
| `POST /api/services/<id>/start` · `/stop` · `/restart` | operator |
| `GET /api/services/<id>/tags`, `PUT /api/services/<id>/tags/<tag> {"value": …}` | viewer / operator |
| `GET /api/services/<id>/history/<tag>?from=…&to=…&points=600` | viewer |
| `GET /api/events` (server-sent events: `tag`, `status`) | viewer |
| `GET /api/config` (export YAML), `PUT /api/config` (import, replaces all) | admin |
| `GET /api/users`, `PUT /api/users/<name> {"password","role"}`, `DELETE /api/users/<name>` | admin (anyone can change their own password) |
| `GET /api/services/<id>/certs`, `POST /api/services/<id>/certs/<file>/trust` · `reject` · `delete` | admin |

API server-mode services without their own `port` answer at `/x/<id>/…` on the web port, with no sign-in, because they stand in for third-party APIs.

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

## Historian storage

History is stored under `<data>/history/<service>/<tag>/<YYYY-MM-DD>.bin`, one file per tag per UTC day. Each record is 16 bytes: an i64 timestamp in milliseconds and an f64 value. Writes are appended once a second. Retention deletes whole day files. Queries average the points into at most `points` buckets, with each bucket's min and max. String tags aren't recorded.

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

## Not yet

These aren't built yet:

- Modbus RTU over serial
- IO-Link masters other than ifm IoT Core
- OPC UA subscriptions (the client polls)
- Built-in HTTPS
- Alarms

## License

GNU Affero General Public License v3.0 or later. See [LICENSE](LICENSE). If you run a modified Cowpoke as a service for others, you must offer them its source.
