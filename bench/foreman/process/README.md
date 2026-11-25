# Pump process data: VFD + centrifugal pump

`pump_sim.py` simulates a drive (V/Hz VFD), an induction motor and a centrifugal pump that moves a
slurry from a suction tank into a discharge line. It makes the signals a real skid gives you:
drive frequency, output voltage, DC bus voltage and motor current, plus suction and discharge
pressure (and flow, which many skids don't have). Foreman learns process faults from these runs, the
way `../gen.py` teaches it communication faults.

Faults change the plant (valve position, tank supply, system resistance, density, bearing drag,
supply voltage), not the signals. The symptoms come out of the model: pump and system curves with
the affinity laws, NPSH and cavitation, a radial pump's power curve, inertia, magnetizing current,
flux weakening on a low bus, DC bus droop and regen, I²t overload and the drive's trip levels.

```bash
python3 pump_sim.py --list
python3 pump_sim.py --catalog --out out/catalog           # one run of every scenario + catalog.png
python3 pump_sim.py --dataset 500 --seed 1 --out out/ds   # randomised plants, timing and severity
python3 pump_sim.py --scenario cavitation --out out/one --cowpoke out/cavitation.yaml
COWPOKE_CONFIG=bench/foreman/process/out/cavitation.yaml cargo run   # replays it on Modbus TCP :5030
```

Each run is a CSV at 10 Hz (`impeller_wear`: one sample a minute for 21 days) with a per-sample
`label` that turns from `normal` to the fault name when the fault starts. `manifest.json(l)` lists
the plant, the onset and the trip. `out/` stays out of git.

## What the foreman sees

| Fault | Current | Voltage (bus / output) | Discharge P | Suction P | Ends in |
|---|---|---|---|---|---|
| deadhead (valve shut) | drops ~⅓, steady | normal | jumps to shutoff | normal | runs on, heating |
| discharge_leak (line break) | rises | normal | drops hard | normal | maybe OL |
| leak_pid (PID holding P) | rises with speed | output V rises | holds at setpoint | normal | maybe OL |
| thick_material | rises | normal | rises with it | normal | |
| cavitation | falls, ragged | normal | sags, ragged | normal-low | loss of prime |
| suction_clog | flat, then falls | normal | slow sag, then ragged | trends down | loss of prime |
| dry_run (tank empties) | flat, then drops to ~½ | normal | collapses | trends to 0 | runs dry |
| bearing_failure | climbs, noisy | normal | unchanged | unchanged | OL trip |
| impeller_wear (weeks) | slowly falls | normal | slowly falls | unchanged | |
| line_sag | small bump | bus dips | unchanged | unchanged | UV trip on a deep sag |
| phase_loss | unchanged | bus ripple, droop | unchanged | unchanged | PHL trip |
| fast_decel | regen | bus climbs on stop | coasts down | | OV trip |
| pid_hunting | oscillates | output V oscillates | oscillates ~6 s | | |
| sensor_stuck | normal, moving | normal | flat line | normal | |
| sensor_open | normal | normal | drops below zero | normal | |

The rule of thumb the patterns teach: **current and pressure moving together** points at the
material; **current and pressure moving opposite** points at the hydraulics (valve, leak, suction);
**current moving with pressure unchanged** points at the motor or mechanics; **only the bus moving** is
the supply; **only pressure moving** is the transmitter.

## The twin: always learning

Foreman runs the twin inside Cowpoke (`src/foreman/twin.rs`, documented in the main README under
*Digital twins*). `twin.py` is its reference: the same models, gates and numbers, quick to change and
test against the simulator here. Change both together.

```bash
python3 twin.py demo --out out/twin    # one random pump: learn healthy days, then every fault + weeks of wear
python3 twin.py learn --state twin.json healthy1.csv healthy2.csv
python3 twin.py watch --state twin.json suspect.csv --out residuals.csv
python3 plots.py all --out out/plots   # every run vs. the twin, the dataset grid, and a GIF of leak_pid
python3 plots.py gif --scenario deadhead
```

`demo` writes `fingerprints.json` (mean residual z before and after each fault, learning held or not,
and the wear drift) and `residuals.png`. From one run, seed 3:

| Fault | ΔP z | Current z | Bus z | Learned after onset |
|---|---|---|---|---|
| deadhead | +18 | −22 | 0 | 0 % |
| discharge_leak | −39 | +16 | 0 | 0 % |
| leak_pid (pressure looks fine) | −15 | +10 | 0 | 0 % |
| thick_material | +7 | +11 | 0 | <1 % |
| bearing_failure | 0 | +23, 5× rougher | 0 | 1 % |
| phase_loss | −1 | +1 | −8 | 0 % |
| sensor_open | −120 | 0 | 0 | 0 % |
| impeller_wear | follows it; baseline drift −9 % in 21 days | | | (learned, as it should) |

Weak so far: cavitation (−5σ, ragged), suction clog (shows in suction pressure, which has no model
of its own yet), a stuck transmitter (unusually smooth). Next for fidelity: a suction model (from tank
level when there is one), an inertia term so starts and stops are predicted too, and a flow model
where a flowmeter exists.

Next for Foreman: the residuals (z, trend, roughness, drift) are what it reads. Training data for
naming process faults comes from `pump_sim.py --dataset` run through the twin, in the same tev1
decision format `../gen.py` writes for communication faults.
