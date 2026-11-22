//! Value generators for server-mode tags: waves, noise, counters and sequences,
//! so a simulated device has moving values without anything writing to it.

use std::time::Duration;

use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::model::{DataType, ServiceDef, TagDef};
use crate::tags::{Origin, Quality, TagStore, as_f64, coerce_tag};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GenKind {
    /// Smooth wave between min and max.
    Sine,
    /// Straight up, straight down.
    Triangle,
    /// Rises from min to max, then jumps back.
    Ramp,
    /// Half the period at max, half at min (true/false for bools).
    Square,
    /// A new random value each interval.
    Random,
    /// Wanders by up to `step` each interval, inside min..max.
    Walk,
    /// Adds `step` each interval, wrapping from max back to min.
    Counter,
    /// Flips between min and max (or true and false) each interval.
    Toggle,
    /// Steps through `values` in order, one per interval.
    Sequence,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Generator {
    pub kind: GenKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Wave period. Default 60 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_ms: Option<u64>,
    /// How often the value updates. Default 1 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
    /// Counter increment, or the largest move per interval for a walk. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// Random noise added to the value, up to ± this much.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise: Option<f64>,
    /// For `sequence`: the values to step through.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<Value>,
}

impl Generator {
    fn min(&self) -> f64 {
        self.min.unwrap_or(0.0)
    }
    fn max(&self) -> f64 {
        self.max.unwrap_or(if self.kind == GenKind::Counter { 65535.0 } else { 100.0 })
    }
    fn period(&self) -> f64 {
        self.period_ms.unwrap_or(60_000).max(100) as f64
    }
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.unwrap_or(1000).max(50))
    }

    /// Check the generator suits the tag's data type.
    pub fn validate(&self, t: &TagDef) -> Result<(), String> {
        let name = &t.name;
        if self.min() > self.max() {
            return Err(format!("tag `{name}`: simulate min is above max"));
        }
        match (t.datatype, self.kind) {
            (DataType::String, k) if k != GenKind::Sequence => {
                Err(format!("tag `{name}`: string tags can only simulate a sequence"))
            }
            (_, GenKind::Sequence) if self.values.is_empty() => {
                Err(format!("tag `{name}`: a sequence needs `values`"))
            }
            (_, GenKind::Sequence) => self
                .values
                .iter()
                .try_for_each(|v| coerce_tag(t, v).map(|_| ()))
                .map_err(|e| format!("tag `{name}`: sequence value: {e}")),
            (DataType::Bool, GenKind::Ramp | GenKind::Counter | GenKind::Walk | GenKind::Sine | GenKind::Triangle) => {
                Err(format!("tag `{name}`: bool tags simulate square, toggle, random or a sequence"))
            }
            _ => Ok(()),
        }
    }

    /// The next value. `phase` is 0..1 through the period; `prev` is the last generated value; `n` counts ticks.
    fn next(&self, t: &TagDef, phase: f64, prev: Option<&Value>, n: u64, rng: &mut impl Rng) -> Value {
        let (lo, hi) = (self.min(), self.max());
        let span = hi - lo;
        if t.datatype == DataType::Bool {
            return Value::Bool(match self.kind {
                GenKind::Square => phase < 0.5,
                GenKind::Random => rng.gen_bool(0.5),
                GenKind::Toggle => n % 2 == 0,
                GenKind::Sequence => crate::codec::truthy(&self.values[(n as usize) % self.values.len()]),
                _ => false,
            });
        }
        let prev_n = prev.and_then(as_f64);
        let v = match self.kind {
            GenKind::Sine => lo + span * (0.5 - 0.5 * (std::f64::consts::TAU * phase).cos()),
            GenKind::Triangle => lo + span * if phase < 0.5 { 2.0 * phase } else { 2.0 - 2.0 * phase },
            GenKind::Ramp => lo + span * phase,
            GenKind::Square => if phase < 0.5 { hi } else { lo },
            GenKind::Random => lo + span * rng.r#gen::<f64>(),
            GenKind::Walk => {
                let step = self.step.unwrap_or(span / 50.0).abs();
                let start = prev_n.unwrap_or(lo + span / 2.0);
                (start + rng.gen_range(-1.0..=1.0) * step).clamp(lo, hi)
            }
            GenKind::Counter => {
                let next = prev_n.map(|p| p + self.step.unwrap_or(1.0)).unwrap_or(lo);
                if next > hi { lo } else { next }
            }
            GenKind::Toggle => if n % 2 == 0 { hi } else { lo },
            GenKind::Sequence => return self.values[(n as usize) % self.values.len()].clone(),
        };
        let noisy = match self.noise {
            Some(a) if a > 0.0 && !matches!(self.kind, GenKind::Counter | GenKind::Walk) => v + rng.gen_range(-a..=a),
            _ => v,
        };
        // Keep floats readable.
        json!((noisy * 1000.0).round() / 1000.0)
    }
}

/// Run every tag's generator for one service until cancelled.
pub async fn run(def: ServiceDef, store: TagStore, cancel: CancellationToken) {
    let gens: Vec<(TagDef, Generator)> = def
        .tags
        .iter()
        .filter_map(|t| t.simulate.clone().map(|g| (t.clone(), g)))
        .collect();
    if gens.is_empty() {
        return;
    }
    let mut tasks = tokio::task::JoinSet::new();
    for (t, g) in gens {
        let (store, id, cancel) = (store.clone(), def.id.clone(), cancel.clone());
        tasks.spawn(async move {
            let mut tick = tokio::time::interval(g.interval());
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut rng = rand::rngs::StdRng::from_entropy();
            let mut n: u64 = 0;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tick.tick() => {}
                }
                let now = chrono::Utc::now().timestamp_millis() as f64;
                let phase = (now % g.period()) / g.period();
                let prev = store.value(&id, &t.name);
                let v = quantize(&t, g.next(&t, phase, prev.as_ref(), n, &mut rng));
                if let Ok(v) = coerce_tag(&t, &v) {
                    store.set(&id, &t.name, v, Quality::Good, Origin::Generator);
                }
                n = n.wrapping_add(1);
            }
        });
    }
    cancel.cancelled().await;
    tasks.shutdown().await;
}

use rand::SeedableRng;

/// A scaled integer tag can only hold whole raw steps: round to what the device would report.
fn quantize(t: &TagDef, v: Value) -> Value {
    let int = matches!(
        t.datatype,
        DataType::Int16 | DataType::Uint16 | DataType::Int32 | DataType::Uint32 | DataType::Int64 | DataType::Uint64
    );
    match (int && t.is_scaled(), as_f64(&v)) {
        (true, Some(x)) => {
            let eng = t.to_eng(t.to_raw(x).round());
            // As many decimals as the scale has (0.1 → 1), so 13.7 doesn't print as 13.700000000000001.
            let places = (-(t.scale.unwrap_or(1.0).abs().max(1e-9)).log10()).ceil().clamp(0.0, 9.0) as i32;
            let f = 10f64.powi(places);
            json!((eng * f).round() / f)
        }
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(dt: DataType) -> TagDef {
        TagDef {
            name: "t".into(),
            datatype: dt,
            ..Default::default()
        }
    }
    fn g(kind: GenKind) -> Generator {
        Generator {
            kind,
            min: Some(10.0),
            max: Some(20.0),
            period_ms: None,
            interval_ms: None,
            step: None,
            noise: None,
            values: vec![],
        }
    }

    #[test]
    fn waves_stay_in_range() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let t = tag(DataType::Float64);
        for kind in [GenKind::Sine, GenKind::Triangle, GenKind::Ramp, GenKind::Square, GenKind::Random, GenKind::Walk] {
            let mut prev = None;
            for i in 0..200 {
                let v = g(kind).next(&t, i as f64 / 200.0, prev.as_ref(), i, &mut rng);
                let x = v.as_f64().unwrap();
                assert!((10.0..=20.0).contains(&x), "{kind:?} gave {x}");
                prev = Some(v);
            }
        }
        // A sine starts at min and peaks at half the period.
        assert_eq!(g(GenKind::Sine).next(&t, 0.0, None, 0, &mut rng), json!(10.0));
        assert_eq!(g(GenKind::Sine).next(&t, 0.5, None, 0, &mut rng), json!(20.0));
    }

    #[test]
    fn counter_wraps_and_sequence_cycles() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(2);
        let t = tag(DataType::Uint16);
        let mut c = g(GenKind::Counter);
        c.step = Some(4.0);
        let mut prev = None;
        let mut seen = vec![];
        for i in 0..5 {
            let v = c.next(&t, 0.0, prev.as_ref(), i, &mut rng);
            seen.push(v.as_f64().unwrap());
            prev = Some(v);
        }
        assert_eq!(seen, vec![10.0, 14.0, 18.0, 10.0, 14.0]);

        let mut s = g(GenKind::Sequence);
        s.values = vec![json!("idle"), json!("run")];
        let st = tag(DataType::String);
        assert!(s.validate(&st).is_ok());
        assert_eq!(s.next(&st, 0.0, None, 3, &mut rng), json!("run"));
    }

    #[test]
    fn scaled_ints_round_to_raw_steps() {
        let mut t = tag(DataType::Int16);
        t.scale = Some(0.1);
        assert_eq!(quantize(&t, json!(13.711)), json!(13.7));
    }

    #[test]
    fn bools_and_validation() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        let b = tag(DataType::Bool);
        assert_eq!(g(GenKind::Toggle).next(&b, 0.0, None, 1, &mut rng), json!(false));
        assert_eq!(g(GenKind::Square).next(&b, 0.2, None, 0, &mut rng), json!(true));
        assert!(g(GenKind::Sine).validate(&b).is_err());
        assert!(g(GenKind::Sine).validate(&tag(DataType::String)).is_err());
        let mut bad = g(GenKind::Ramp);
        bad.min = Some(5.0);
        bad.max = Some(1.0);
        assert!(bad.validate(&tag(DataType::Int16)).is_err());
    }
}
