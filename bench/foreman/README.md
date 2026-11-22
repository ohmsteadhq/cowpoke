# Foreman benchmark and fault generator

Can a small local model diagnose Cowpoke faults? This folder stages faults with
Cowpoke's own simulators and measures models on them.

- `faults.yaml` and `scenarios.json`: 12 hand-staged scenarios (11 faults, 1 healthy), each a
  simulator and a misconfigured client, with the answer key.
- `bench.py`: runs a model on the scenarios through Ollama.
  - `--mode agent`: the model calls read-only Cowpoke API tools itself.
  - `--mode evidence`: Cowpoke gathers status, definitions and live values; a chat model answers once.
  - `--mode decide`: the same evidence to a decision model (`ollama run --ask/--choice`).
  - `--rules`: add Cowpoke's rule checks (each client compared field by field with its device's
    simulator) to the evidence. In this benchmark the simulator is a perfect reference; on a real
    site the reference has to come from the design, the PLC program or the last configuration that worked.
- `gen.py`: the fault generator. Random devices and clients across Modbus, EtherNet/IP, MQTT,
  Sparkplug, HTTP API and IO-Link, one injected fault or none, run in private Cowpoke instances.
  Writes training data in the tev1 decision format (state, question, lettered options, answer);
  the symptom comes from what actually happened, not from the injected fault.

```bash
COWPOKE_CONFIG=bench/foreman/faults.yaml COWPOKE_BIND=127.0.0.1:18090 COWPOKE_ADMIN_PASSWORD=... target/debug/cowpoke
python3 bench/foreman/bench.py --model qwen3:4b --mode evidence --rules --password ...
python3 bench/foreman/gen.py --scenarios 4000 --seed 1 --out bench/foreman/data/faults-s1.jsonl
```

Generated data (`data/`) stays out of git.
