# A buttercream plant, learned one day at a time

The whole loop on a real plant drawing: Ohmstead → Cowpoke tags → Foreman's twins learning from
production days, retraining their regressions every night, flagging faults.

1. **Ohmstead.** A large real project (a buttercream frosting plant: 255 components, 900 wires, a
   268-item P&ID, six areas) is loaded into Ohmstead. `export_io_points` gives its PLC I/O;
   `export_twins` gives every motor on a drive (22), what each turns, the instruments on a pump's
   piping and the transmitters nearby. Eight direct-on-line motors are skipped with the reason.
2. **Cowpoke config** (`config`): the services and tags a site would have. Drive data for areas 100–300
   comes through the PLC with legacy names (`VFD_301_OutFreq`, decoys like `VFD_301_FreqCmd`); areas
   400–600 drives are their own Modbus services (`vfd-401/OutputFreq`); area 400's instruments keep old
   PLC names (`Skid4.PT401_PV`). The line's recipe number is a PLC tag (`Line_RecipeNo`).
3. **Matching** (`match`): `cowpoke twins match` and `apply`. All 22 twins, all 122 roles (the recipe
   tag is found by name and offered to every twin), no hand edits.
4. **Production days** (`day`, `train`): batches in the premix every hour, a continuous line
   06:00–22:00 (timing pump, scraped-surface heater and cooler, aerator, filler feed pump, four
   conveyors), CIP at night, utilities around the clock, Sundays off. 2 s samples, 24 h a day, 126
   columns named as in Cowpoke.
5. **Training** (`train`): each day is replayed through Foreman's twins (`cowpoke twins replay`), which
   carry what they learned into the next day and retrain their regressions every night. The report
   compares what they flagged with the answer key (`EVENTS`), which they never see.

```bash
# the Ohmstead exports go in out/ (git-ignored: the drawing is a real project used as test data)
python3 buttercream.py config
python3 buttercream.py match
python3 buttercream.py train --days 28     # ~90 s; writes out/train-report.txt, out/timeline.png
```

## What happened in 28 days (seed 0)

- **Night 1, Foreman retrained 5 twins** from nearby transmitters: the aerator from the product's
  density × speed (error 0.43 → 0.11 A), the high-shear mixer from the batch temperature × speed²
  (0.92 → 0.27 A), the cooler from its outlet temperature (0.50 → 0.19 A), the heater from its outlet
  temperature, and the scraper agitator from the batch temperature and, spuriously, the hold tank's
  level (a coincidence in this simulator, the kind a real plant has too).
- **Warm-up:** 16 of 22 twins past warm-up after day 1, 22 by day 8. The small transfer pumps run
  minutes a day, so they take longest.

| Event (answer key) | Days | What the twins did |
|---|---|---|
| Supply sag, 0.4 s | 6 | a −81σ DC bus blip on 13 drives in the same sample: the plant-wide signature |
| Chocolate recipe (normal) | 9–11 | no holds. Every twin saw recipe 2 start and learned it as new (~3 h of running); then Foreman compared: the line machines run differently on it (the mixer's current error 3.3 A with the vanilla model, 0.37 A with its own), so they keep a model per recipe; utilities, conveyors, the blower and the dust collector run the same, so they went back to one model. Back on vanilla on day 12, the line machines took out what they had learned for it |
| Cooler freeze-up, worse each day | 15–18 | HX-302 caught every day, from 520 min in on the first day to at once later |
| Glycol fixed (normal) | 20 | HX-302 held while it re-learned |
| Timing pump lobe wear | 16–24 | P-301 caught every day, ~70 min in on the first two days, then at once |
| Lobes replaced (normal) | 25 | P-301 held while it re-learned |
| Filler nozzle blockage, 40 min | 18 | P-401 at 37σ at once; the drive then tripped |
| Mixer bearing, worse each day | 22–27 | not held until day 25, but the twin's own error rose from day 22 (bottom of the timeline) |
| Conveyor jam | 25 | CV-412 at 20σ a minute in, 4 minutes before the drive tripped |
| Air in the glycol loop | 26 | P-602 within 25 min |
| Thick batch slug (travels down the line) | 4 | P-301, then HX-301, HX-302, P-401 over 18 min: Foreman reports **one** event that started at P-301 |
| Thick batch slug, on a day P-301 already holds for lobe wear | 23 | HX-301, HX-302, P-401 within 12 s: one event, in flow order, "too close to tell where it started" |

16 holds in 28 days matched no event: short ~1σ creeps at Monday start-ups, and the scraper agitator
on the first chocolate morning (7). It runs through the night, so it learned chocolate from the small
night batches before the day's full ones came; the answer key leaves it off the recipe list though
chocolate does load it.

## The plant, not just the machines

Ohmstead exports each twin's `downstream` machines from the P&ID's process lines (through valves, tanks
and off-page connectors). Foreman uses them to group what the twins report (`plant_events`, printed as
`PLANT` lines in the report):

- **Supply:** a DC bus blip on 3+ drives within 2 s. Day 6's sag is one event on 13 drives, not 13.
- **One upset along the line:** holds starting within 15 min on machines up to two apart. It says where
  it started and whether it went down the line or backed up, or, when the holds are under 2 min apart,
  that it can't tell (a strong residual crosses its threshold before a weak one that arrived first).
- **Conditions at start-up aren't chained:** a hold already there within 5 min of a machine starting
  (worn lobes, a frozen cooler) is what that machine started with. Without this rule, lobe wear and the
  freeze-up were grouped just because both showed at 06:00.

## What this taught (and what changed because of it)

- **A noise floor must scale with the motor.** A 0.3 A floor (right for a 40 hp pump) hid a jam that
  tripled a 1 hp conveyor's current. Now: 0.05 A, plus 4 % of the expected value.
- **Inputs must be causes, not other machines' outputs.** Trained on P-401's discharge pressure, the
  aerator's twin raised a false alarm when P-401 tripped. Ohmstead now offers only temperatures,
  weights, densities, levels and analyzers as candidates.
- **A slow fault can slip under the gate.** The bearing crept in a few percent a day and the mixer's
  twin partly learned it; its rising error showed it first. A vibration transmitter would find it weeks
  sooner.
- **Normal changes need context.** Before the recipe tag, the chocolate days held five twins for three
  days, and accepting chocolate as the new normal made vanilla look wrong afterwards. Now each twin keeps
  a model per recipe, and Foreman drops the split where the recipe makes no difference.
- **Short events need fast data.** The 0.4 s sag is one sample at 2 s polling: a blip, not an episode.
  The drives' own fault logs see more.
