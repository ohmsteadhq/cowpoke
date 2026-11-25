//! Digital twins that never stop learning: the foreman's model of how a machine behaves
//! when it is healthy.
//!
//! A pump twin predicts, from the speed the drive is running at, what the pump should read:
//! differential pressure, motor current and DC bus voltage. Physics gives each model its
//! shape, so a few numbers describe a pump, and they are learned from the pump's own data
//! with recursive least squares, one sample at a time, for as long as Cowpoke runs:
//!
//! ```text
//!   ΔP  = a + b·r²              pump curve meets system curve (affinity laws), r = f / f_base
//!   I²  = c + d·r² + e·r⁴       magnetizing current² + torque current²
//!   Vdc = g + h·(I·Vout)/1000   bus droop with load
//! ```
//!
//! What the twin can't explain is the residual; residuals are the foreman's evidence for
//! process faults. Learning is gated so the twin learns the machine, not its faults: only
//! steady running, only samples it already explains, and not while residuals creep one way.
//! Forgetting (a half-life) lets it follow slow normal change, so a frozen copy from
//! commissioning (the baseline) is kept beside it: live vs. baseline is wear.
//! The reference implementation and its tests against simulated faults are in
//! `bench/foreman/process/twin.py`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tags::{Origin, Quality, TagEvent, TagStore, as_f64};

/// Samples are taken every TICK; the steadiness window, learning rate and EWMAs are in ticks.
const TICK: Duration = Duration::from_millis(100);
const STEADY_WINDOW: usize = 10; // 1 s
const STEADY_RANGE_HZ: f64 = 1.0; // under ~1 Hz/s: a PID trimming speed still counts as steady
const LEARN_EVERY: u64 = 10; // learn at 1 Hz
const TREND_N: f64 = 600.0; // ~60 s
const ROUGH_N: f64 = 50.0; // ~5 s
const STALE_S: i64 = 5;
/// A transmitter whose value hasn't changed at all in this long while its machine runs is stuck:
/// live analog readings always move in their last digits.
const FLAT_S: i64 = 600;
/// Hold when a model's residual has stayed this many times rougher than usual for ~60 s (a noisy
/// transmitter, surging, cavitation).
const ROUGH_GATE: f64 = 3.0;

// ------------------------------------------------------------------------------------ config

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TwinDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// What the machine is, which picks the models. Default a centrifugal pump.
    #[serde(default)]
    pub kind: TwinKind,
    /// Each signal as `service/tag`. Frequency and current are required; discharge pressure too for a pump.
    pub freq: String,
    pub current: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discharge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suction: Option<String>,
    /// Context: read and shown with the twin, not modelled yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<String>,
    /// More measurements Foreman may try as inputs when it retrains the twin's regressions (nearby
    /// temperatures, levels, weights), each `service/tag`. Kept only if they make the twin more accurate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// Twins the product reaches next (from the P&ID). Foreman groups holds that travel along these
    /// into one plant event instead of separate faults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub downstream: Vec<String>,
    /// The tag that says which product or recipe is running (`service/tag`, a number). Each recipe
    /// gets its own models, so a recipe change isn't a fault; a recipe seen before picks up what was
    /// learned for it. If Foreman finds a recipe doesn't change this machine, it keeps one model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// `ohmstead` when the twin was made from an Ohmstead export (re-matching may update it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_voltage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dc_bus: Option<String>,
    /// Motor base frequency, Hz.
    #[serde(default = "d_fbase")]
    pub f_base: f64,
    /// How long the twin remembers, in hours of steady running. Default a week.
    #[serde(default = "d_half")]
    pub half_life_h: f64,
    /// Seconds of steady running learned before the baseline is frozen. Default 10 minutes.
    #[serde(default = "d_warm")]
    pub warmup_s: u64,
    /// Don't learn a sample whose residual is beyond this many standard deviations.
    #[serde(default = "d_gate")]
    pub gate_z: f64,
    /// Don't learn while a residual's ~60 s average is beyond this (slow creep).
    #[serde(default = "d_trend")]
    pub trend_gate: f64,
}

/// What a twin models.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TwinKind {
    /// Centrifugal pump on a drive: ΔP from speed² (affinity laws), current from speed.
    #[default]
    PumpVfd,
    /// Positive-displacement pump (lobe, gear) on a drive: ΔP from speed (viscous and turbulent
    /// losses), current from the pressure it pushes against.
    PumpPdVfd,
    /// Any other motor on a drive (agitator, rotor, conveyor, a pump with no pressure): current from speed.
    DriveLoad,
}

fn d_fbase() -> f64 {
    60.0
}
fn d_half() -> f64 {
    168.0
}
fn d_warm() -> u64 {
    600
}
fn d_gate() -> f64 {
    4.0
}
fn d_trend() -> f64 {
    1.0
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct TwinsConfig {
    #[serde(default)]
    pub twins: Vec<TwinDef>,
    /// Foreman's own training data, generated from the twins' regressions (off unless set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<Synthetic>,
}

/// Synthetic training data: once a twin has run `after_days`, Foreman generates samples from its own
/// regression every night (the teacher) and mixes them into what the twin learns the next day and into
/// the nightly retrain, as `share` of the samples. Operating points are spread over the speeds the twin
/// has run at, with the inputs of real samples and noise as large as the twin's error. Held-out scoring
/// stays on real samples only.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Synthetic {
    /// Share of samples learned that are synthetic, 0 to 0.9.
    pub share: f64,
    #[serde(default = "d_after")]
    pub after_days: f64,
}

fn d_after() -> f64 {
    30.0
}

impl TwinsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(sy) = &self.synthetic {
            if !(0.0..=0.9).contains(&sy.share) || sy.after_days < 0.0 {
                return Err("synthetic: share must be 0 to 0.9 and after_days not negative".into());
            }
        }
        let mut names = std::collections::HashSet::new();
        for t in &self.twins {
            if !names.insert(&t.name) {
                return Err(format!("two twins are called `{}`", t.name));
            }
            for (role, key) in t.signals().into_iter().chain(t.recipe.as_ref().map(|k| ("recipe", k))) {
                if split(key).is_none() {
                    return Err(format!("twin `{}`: {role} `{key}` should be `service/tag`", t.name));
                }
            }
            if t.kind != TwinKind::DriveLoad && t.discharge.is_none() {
                return Err(format!("twin `{}`: a pump twin needs `discharge` (or make it kind: drive_load)", t.name));
            }
            if t.f_base <= 0.0 || t.half_life_h <= 0.0 || t.gate_z <= 0.0 || t.trend_gate <= 0.0 {
                return Err(format!("twin `{}`: f_base, half_life_h, gate_z and trend_gate must be above zero", t.name));
            }
        }
        Ok(())
    }
}

fn split(key: &str) -> Option<(String, String)> {
    let (s, t) = key.split_once('/')?;
    (!s.is_empty() && !t.is_empty()).then(|| (s.to_string(), t.to_string()))
}

impl TwinDef {
    /// The signals a sample needs, fresh.
    fn signals(&self) -> Vec<(&'static str, &String)> {
        let mut v = vec![("freq", &self.freq), ("current", &self.current)];
        for (role, k) in [("discharge", &self.discharge), ("suction", &self.suction), ("out_voltage", &self.out_voltage), ("dc_bus", &self.dc_bus)] {
            if let Some(k) = k {
                v.push((role, k));
            }
        }
        v
    }

    /// Context signals: shown with the twin, never required.
    fn context(&self) -> Vec<(&'static str, &String)> {
        [("flow", &self.flow), ("level", &self.level), ("temperature", &self.temperature)]
            .into_iter()
            .filter_map(|(r, k)| k.as_ref().map(|k| (r, k)))
            .collect()
    }

    /// Every signal routed to the twin, by role: required, context, then inputs (role = their key).
    fn routed(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self.signals().into_iter().chain(self.context()).map(|(r, k)| (r.to_string(), k.clone())).collect();
        for k in &self.inputs {
            v.push((k.clone(), k.clone()));
        }
        if let Some(k) = &self.recipe {
            v.push(("recipe".into(), k.clone()));
        }
        v
    }

    /// Measurements Foreman may try as regression inputs: the level and temperature roles and the
    /// inputs. A pump's own flow and pressures are left out: a fault in the pump moves them too, so
    /// they would explain the fault away.
    fn candidates(&self) -> Vec<String> {
        let mut v: Vec<String> = [("level", &self.level), ("temperature", &self.temperature)].into_iter().filter(|(_, k)| k.is_some()).map(|(r, _)| r.to_string()).collect();
        v.extend(self.inputs.iter().cloned());
        v
    }

    fn models(&self) -> Vec<Kind> {
        let mut m = match self.kind {
            TwinKind::PumpVfd => vec![Kind::Dp, Kind::Current],
            TwinKind::PumpPdVfd => vec![Kind::DpPd, Kind::CurrentPd],
            TwinKind::DriveLoad => vec![Kind::Current],
        };
        if self.dc_bus.is_some() && self.out_voltage.is_some() {
            m.push(Kind::Bus);
        }
        m
    }
}

// ------------------------------------------------------------------------------------ models

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dp,
    Current,
    Bus,
    /// A positive-displacement pump's ΔP: a + b·r + c·r² (viscous and turbulent losses at a flow ∝ speed).
    DpPd,
    /// A positive-displacement pump's current: torque follows the pressure it pushes against.
    CurrentPd,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Dp | Kind::DpPd => "dp",
            Kind::Current | Kind::CurrentPd => "current",
            Kind::Bus => "dc_bus",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Dp | Kind::DpPd => "differential pressure",
            Kind::Current | Kind::CurrentPd => "motor current",
            Kind::Bus => "DC bus",
        }
    }
    fn unit(self) -> &'static str {
        match self {
            Kind::Dp | Kind::DpPd => "bar",
            Kind::Current | Kind::CurrentPd => "A",
            Kind::Bus => "V",
        }
    }
    /// The smallest residual standard deviation believed: below this is transmitter noise.
    fn floor(self) -> f64 {
        match self {
            Kind::Dp | Kind::DpPd => 0.03,
            // Meter resolution only: the relative floor scales with the motor (a 1 hp conveyor draws 0.5 A).
            Kind::Current | Kind::CurrentPd => 0.05,
            Kind::Bus => 2.0,
        }
    }
    /// The smallest residual believed, as a share of the expected value: product and weather move a
    /// motor's load and a pump's pressure a few percent without anything being wrong.
    fn rel_floor(self) -> f64 {
        match self {
            Kind::Bus => 0.0,
            _ => 0.04,
        }
    }
    fn features(self, s: &Sample) -> Vec<f64> {
        let r = s.r;
        match self {
            Kind::Dp => vec![1.0, r * r],
            Kind::Current => vec![1.0, r * r, r.powi(4)],
            Kind::Bus => vec![1.0, s.current * s.out_voltage / 1000.0],
            Kind::DpPd => vec![1.0, r, r * r],
            Kind::CurrentPd => vec![1.0, r * r, s.dp, s.dp * s.dp],
        }
    }
    /// What the regression fits (current is fitted squared: magnetizing and torque currents add in quadrature).
    fn target(self, s: &Sample) -> f64 {
        match self {
            Kind::Dp | Kind::DpPd => s.dp,
            Kind::Current | Kind::CurrentPd => s.current * s.current,
            Kind::Bus => s.dc_bus,
        }
    }
    fn measured(self, y: f64) -> f64 {
        if matches!(self, Kind::Current | Kind::CurrentPd) { y.max(0.0).sqrt() } else { y }
    }
}

/// Recursive least squares with exponential forgetting.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Rls {
    theta: Vec<f64>,
    p: Vec<Vec<f64>>,
    lam: f64,
}

impl Rls {
    fn new(n: usize, lam: f64) -> Self {
        let p = (0..n).map(|i| (0..n).map(|j| if i == j { 1e4 } else { 0.0 }).collect()).collect();
        Self { theta: vec![0.0; n], p, lam }
    }
    fn predict_with(theta: &[f64], x: &[f64]) -> f64 {
        theta.iter().zip(x).map(|(t, v)| t * v).sum()
    }
    /// How unfamiliar this operating point is: x'Px, ~0 where the twin has seen plenty, large
    /// where it has seen nothing. Prediction uncertainty grows with it.
    fn leverage(&self, x: &[f64]) -> f64 {
        let n = x.len();
        (0..n).map(|i| x[i] * (0..n).map(|j| self.p[i][j] * x[j]).sum::<f64>()).sum::<f64>().max(0.0)
    }

    fn update(&mut self, x: &[f64], y: f64) {
        let n = self.theta.len();
        let px: Vec<f64> = (0..n).map(|i| (0..n).map(|j| self.p[i][j] * x[j]).sum()).collect();
        let denom = self.lam + (0..n).map(|i| x[i] * px[i]).sum::<f64>();
        let k: Vec<f64> = px.iter().map(|v| v / denom).collect();
        let err = y - Self::predict_with(&self.theta, x);
        for i in 0..n {
            self.theta[i] += k[i] * err;
        }
        for i in 0..n {
            for j in 0..n {
                self.p[i][j] = (self.p[i][j] - k[i] * px[j]) / self.lam;
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ModelState {
    rls: Rls,
    /// Residual variance, from learned samples.
    var: f64,
    /// Rolling squared error of samples before they were learned: the fidelity.
    mse: Option<f64>,
    /// ~60 s average of z, and the spread of z over the last 5 s.
    trend: f64,
    rough: f64,
    /// The spread a healthy machine has, learned with the model; roughness is reported against it.
    #[serde(default = "one")]
    rough_ref: f64,
    /// The last 5 s of z, for the spread.
    #[serde(skip)]
    recent: VecDeque<f64>,
    /// ~60 s average of the roughness (against rough_ref).
    #[serde(skip, default = "one")]
    rough_avg: f64,
    baseline: Option<Vec<f64>>,
    /// Inputs Foreman added when it retrained this model, after the physics features.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extra: Vec<Extra>,
}

/// One trained input: a measurement, centred and scaled, alone or times speed or speed².
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Extra {
    input: String,
    /// 0: x, 1: x·r, 2: x·r²
    form: u8,
    mean: f64,
    scale: f64,
}

impl Extra {
    fn value(&self, s: &Sample) -> f64 {
        let x = (s.x.get(&self.input).copied().unwrap_or(self.mean) - self.mean) / self.scale;
        x * s.r.powi(self.form as i32)
    }
    fn label(&self) -> String {
        format!("{}{}", self.input, ["", "·speed", "·speed²"][self.form.min(2) as usize])
    }
}

fn one() -> f64 {
    1.0
}

impl ModelState {
    fn new(k: Kind, lam: f64) -> Self {
        let n = k.features(&Sample::default()).len();
        Self { rls: Rls::new(n, lam), var: 4.0 * k.floor() * k.floor(), mse: None, trend: 0.0, rough: 1.0, rough_ref: 1.0, recent: VecDeque::new(), rough_avg: 1.0, baseline: None, extra: vec![] }
    }
    /// The physics features of its kind, then the trained inputs.
    fn features(&self, k: Kind, s: &Sample) -> Vec<f64> {
        let mut x = k.features(s);
        x.extend(self.extra.iter().map(|e| e.value(s)));
        x
    }
    fn sd(&self, k: Kind) -> f64 {
        self.var.sqrt().max(k.floor())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Version {
    pub ts: DateTime<Utc>,
    pub learned_s: u64,
    pub rmse: BTreeMap<String, Option<f64>>,
    pub note: String,
}

/// What persists for one twin.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Learned {
    models: BTreeMap<String, ModelState>,
    /// Samples learned (one a second of steady running) and steady samples seen.
    learned: u64,
    seen: u64,
    baseline_at: Option<DateTime<Utc>>,
    history: Vec<Version>,
    /// Accepted samples (every BUFFER_EVERY-th one learned) that Foreman retrains the regressions on.
    #[serde(default)]
    buffer: VecDeque<Sample>,
    /// The recipe the models are for, samples learned in it, and the models of the other recipes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipe: Option<String>,
    #[serde(default)]
    recipe_learned: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    recipes: BTreeMap<String, Stash>,
    /// The recipe before this one, while this one is being learned (to compare them after).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prev_recipe: Option<String>,
    /// Foreman found the recipe doesn't change this machine: one set of models for every recipe.
    #[serde(default)]
    recipe_blind: bool,
    /// Per watched signal, the longest it has read exactly the same value and then moved again, in
    /// seconds of running: a coarse transmitter on a steady tank sits still longer than a noisy one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    flat_ref: BTreeMap<String, f64>,
    /// First and latest running sample (the data's own clock: replays run faster than time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_ts: Option<DateTime<Utc>>,
    /// Synthetic data: the models frozen last night that generate it, for the recipe then running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    teacher: Option<Teacher>,
    #[serde(default)]
    synth_credit: f64,
    #[serde(default)]
    synth_learned: u64,
    #[serde(default)]
    rng: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Teacher {
    recipe: Option<String>,
    /// Per model: θ, the trained inputs, and the noise (the model's error), in measured units.
    models: BTreeMap<String, (Vec<f64>, Vec<Extra>, f64)>,
    /// The speeds the twin has run at (r).
    r_lo: f64,
    r_hi: f64,
}

/// One recipe's models while another recipe runs.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Stash {
    models: BTreeMap<String, ModelState>,
    learned: u64,
    baseline_at: Option<DateTime<Utc>>,
}

const BUFFER_EVERY: u64 = 5;
const BUFFER_MAX: usize = 6000;

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct Sample {
    r: f64,
    freq: f64,
    current: f64,
    dp: f64,
    out_voltage: f64,
    dc_bus: f64,
    /// The candidate inputs' values, by role or key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    x: BTreeMap<String, f64>,
    /// The recipe running when it was taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipe: Option<String>,
}

#[derive(Default)]
struct Live {
    latest: HashMap<String, (f64, DateTime<Utc>)>,
    /// For the stuck-transmitter check: each watched signal's value and since when it has read exactly that.
    held: HashMap<String, (f64, DateTime<Utc>)>,
    window: VecDeque<f64>,
    last: Value,
}

struct Twin {
    def: TwinDef,
    learned: Learned,
    live: Live,
    synth: Option<Synthetic>,
}

impl Twin {
    fn new(def: TwinDef, saved: Option<Learned>) -> Self {
        let lam = 0.5f64.powf(1.0 / (def.half_life_h * 3600.0).max(1.0));
        let mut learned = saved.unwrap_or_default();
        // Saved before recipes: everything learned was learned in one.
        if learned.recipe_learned == 0 && learned.baseline_at.is_some() {
            learned.recipe_learned = learned.learned;
        }
        if def.recipe.is_none() {
            learned.recipes.clear();
        }
        let fit = |models: &mut BTreeMap<String, ModelState>| {
            for k in def.models() {
                let m = models.entry(k.key().into()).or_insert_with(|| ModelState::new(k, lam));
                // A model saved for another kind (or with inputs no longer routed) starts again.
                let n = k.features(&Sample::default()).len() + m.extra.len();
                if m.rls.theta.len() != n || m.extra.iter().any(|e| !def.candidates().contains(&e.input)) {
                    *m = ModelState::new(k, lam);
                }
                m.rls.lam = lam;
            }
            models.retain(|name, _| def.models().iter().any(|k| k.key() == name));
        };
        fit(&mut learned.models);
        for s in learned.recipes.values_mut() {
            fit(&mut s.models);
        }
        Self { def, learned, live: Live::default(), synth: None }
    }

    fn sample(&self, now: DateTime<Utc>) -> Result<Option<Sample>, String> {
        let get = |role: &str| self.live.latest.get(role).copied();
        let mut stale = vec![];
        for (role, _) in self.def.signals() {
            match get(role) {
                Some((_, ts)) if (now - ts).num_seconds() <= STALE_S => {}
                _ => stale.push(role),
            }
        }
        // Values that only change when the process does (a held pressure) are refreshed by
        // every poll, so a gap means the signal is lost.
        if !stale.is_empty() {
            return Err(format!("no fresh value for {}", stale.join(", ")));
        }
        let v = |role| get(role).map(|x| x.0).unwrap_or(0.0);
        let freq = v("freq");
        let current = v("current");
        if freq < 10.0 || current <= 0.0 {
            return Ok(None);
        }
        let x = self
            .def
            .candidates()
            .into_iter()
            .filter_map(|c| get(&c).filter(|(_, ts)| (now - *ts).num_seconds() <= STALE_S * 12).map(|(v, _)| (c, v)))
            .collect();
        Ok(Some(Sample {
            r: freq / self.def.f_base,
            freq,
            current,
            dp: v("discharge") - v("suction"),
            out_voltage: v("out_voltage"),
            dc_bus: v("dc_bus"),
            x,
            recipe: self.learned.recipe.clone(),
        }))
    }

    /// The recipe the tag says is running, if it's fresh (a minute; it rarely changes).
    fn recipe_now(&self, now: DateTime<Utc>) -> Option<String> {
        self.def.recipe.as_ref()?;
        let (v, ts) = *self.live.latest.get("recipe")?;
        ((now - ts).num_seconds() <= 60).then(|| format!("{}", v.round() as i64))
    }

    /// Follow the recipe tag: put this recipe's models away and take out the next one's, or start it
    /// from this one's (learning it fresh, as at commissioning).
    fn switch_recipe(&mut self, now: DateTime<Utc>) {
        let Some(next) = self.recipe_now(now) else { return };
        let l = &mut self.learned;
        let Some(cur) = l.recipe.clone() else {
            l.recipe = Some(next);
            return;
        };
        if cur == next || l.recipe_blind {
            return;
        }
        let lam = 0.5f64.powf(1.0 / (self.def.half_life_h * 3600.0).max(1.0));
        let stash = Stash { models: l.models.clone(), learned: l.recipe_learned, baseline_at: l.baseline_at };
        l.recipes.insert(cur.clone(), stash);
        let note = match l.recipes.remove(&next) {
            Some(s) => {
                l.models = s.models;
                l.recipe_learned = s.learned;
                l.baseline_at = s.baseline_at;
                l.prev_recipe = None;
                format!("recipe {next} (from {cur}): back to what was learned for it")
            }
            None => {
                // Start from the last recipe's curves, but unsure of them, so it learns this one fast.
                for m in l.models.values_mut() {
                    m.rls.p = Rls::new(m.rls.theta.len(), lam).p;
                    m.baseline = None;
                    m.mse = None;
                    m.trend = 0.0;
                }
                l.recipe_learned = 0;
                l.baseline_at = None;
                l.prev_recipe = Some(cur.clone());
                format!("recipe {next} (from {cur}): new, learning it")
            }
        };
        l.recipe = Some(next);
        for m in l.models.values_mut() {
            m.recent.clear();
            m.rough_avg = 1.0;
        }
        self.live.window.clear();
        self.version(now, &note);
    }

    /// A new recipe has been learned: does it differ from the one before? If the old recipe's models
    /// predict this one's accepted samples about as well as its own, the recipe doesn't change this
    /// machine, and the twin keeps one set of models from now on.
    fn compare_recipes(&mut self, now: DateTime<Utc>) {
        let l = &self.learned;
        let (Some(prev), Some(cur)) = (l.prev_recipe.clone(), l.recipe.clone()) else { return };
        let Some(old) = l.recipes.get(&prev) else { return };
        let rows: Vec<&Sample> = l.buffer.iter().filter(|s| s.recipe.as_deref() == Some(cur.as_str())).collect();
        if (rows.len() as u64) < (self.def.warmup_s / BUFFER_EVERY / 2).max(10) {
            return;
        }
        let mut same = true;
        let mut worst = String::new();
        for k in self.def.models() {
            let (Some(new_m), Some(old_m)) = (l.models.get(k.key()), old.models.get(k.key())) else { continue };
            let rmse = |m: &ModelState| {
                let se: f64 = rows.iter().map(|s| (k.measured(k.target(s)) - k.measured(Rls::predict_with(&m.rls.theta, &m.features(k, s)))).powi(2)).sum();
                (se / rows.len() as f64).sqrt()
            };
            let (a, b) = (rmse(new_m), rmse(old_m));
            let typical = rows.iter().map(|s| k.measured(k.target(s)).abs()).sum::<f64>() / rows.len() as f64;
            if b > 1.5 * a + k.floor() + 0.5 * k.rel_floor() * typical {
                same = false;
                worst = format!("{} {:.3} → {:.3} {}", k.label(), b, a, k.unit());
            }
        }
        if same {
            let back = self.learned.recipes.remove(&prev).expect("stash of the previous recipe");
            self.learned.models = back.models;
            self.learned.recipe_learned = back.learned;
            self.learned.baseline_at = back.baseline_at;
            self.learned.recipes.clear();
            self.learned.recipe_blind = true;
            self.learned.prev_recipe = None;
            self.version(now, &format!("Foreman found recipe {cur} runs this machine like recipe {prev}: one model for every recipe from now on"));
        } else {
            self.learned.prev_recipe = None;
            self.version(now, &format!("Foreman found recipe {cur} changes this machine ({worst} with its own model): kept apart"));
        }
    }

    /// Watched transmitters (pressures and trained inputs) that have read exactly the same value for
    /// FLAT_S of running, and three times longer than they ever did before moving again.
    fn stuck(&mut self, now: DateTime<Utc>) -> Vec<String> {
        let mut watch: Vec<String> = ["discharge", "suction"].iter().filter(|r| self.live.latest.contains_key(**r)).map(|r| r.to_string()).collect();
        for m in self.learned.models.values() {
            for e in &m.extra {
                if !watch.contains(&e.input) {
                    watch.push(e.input.clone());
                }
            }
        }
        let mut out = vec![];
        for role in watch {
            let Some(&(v, ts)) = self.live.latest.get(&role) else { continue };
            let limit = (FLAT_S as f64).max(3.0 * self.learned.flat_ref.get(&role).copied().unwrap_or(0.0));
            let h = self.live.held.entry(role.clone()).or_insert((v, ts));
            let flat = (now - h.1).num_milliseconds() as f64 / 1000.0;
            if h.0 != v {
                // A still stretch that ended on its own, short of a stuck one, is how this signal is.
                if flat < limit {
                    let r = self.learned.flat_ref.entry(role.clone()).or_insert(0.0);
                    *r = r.max(flat);
                }
                *h = (v, ts);
            } else if flat >= limit {
                let key = match role.as_str() {
                    "discharge" => self.def.discharge.clone(),
                    "suction" => self.def.suction.clone(),
                    "level" => self.def.level.clone(),
                    "temperature" => self.def.temperature.clone(),
                    _ => Some(role.clone()),
                }
                .unwrap_or(role);
                out.push(format!("{key} stuck at {} for {} min", round(v, 3), (now - h.1).num_minutes()));
            }
        }
        out
    }

    fn tick(&mut self, now: DateTime<Utc>) {
        self.switch_recipe(now);
        let s = match self.sample(now) {
            Err(why) => {
                self.live.window.clear();
                self.live.held.clear();
                self.live.last = json!({"status": "no data", "detail": why});
                return;
            }
            Ok(None) => {
                self.live.window.clear();
                self.live.held.clear();
                self.live.last = json!({"status": "stopped"});
                return;
            }
            Ok(Some(s)) => s,
        };
        self.learned.started_at.get_or_insert(now);
        self.learned.last_ts = Some(now);
        let stuck = self.stuck(now);
        let w = &mut self.live.window;
        w.push_back(s.freq);
        if w.len() > STEADY_WINDOW {
            w.pop_front();
        }
        let (lo, hi) = w.iter().fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
        if w.len() < STEADY_WINDOW || hi - lo >= STEADY_RANGE_HZ {
            for m in self.learned.models.values_mut() {
                m.recent.clear();
            }
            self.live.last = json!({"status": "changing speed", "freq_hz": round(s.freq, 2)});
            return;
        }

        let kinds = self.def.models();
        let mut out = serde_json::Map::new();
        let mut zs = vec![];
        for k in &kinds {
            let m = self.learned.models.get_mut(k.key()).expect("model per kind");
            let x = m.features(*k, &s);
            let pred = k.measured(Rls::predict_with(&m.rls.theta, &x));
            let meas = k.measured(k.target(&s));
            let res = meas - pred;
            // Judge against what the twin knows here: at a speed it has rarely seen, it is unsure.
            let sd = m.sd(*k).max(k.rel_floor() * pred.abs());
            let z = res / (sd * (1.0 + m.rls.leverage(&x)).sqrt());
            m.trend += (z - m.trend) / TREND_N;
            // Roughness: the spread of z over the last 5 s. Surging and noise count; a clean
            // step only for those 5 s.
            m.recent.push_back(z);
            if m.recent.len() > ROUGH_N as usize {
                m.recent.pop_front();
            }
            let mu = m.recent.iter().sum::<f64>() / m.recent.len() as f64;
            m.rough = m.recent.iter().map(|v| (v - mu).powi(2)).sum::<f64>() / m.recent.len() as f64;
            let rough = (m.rough / m.rough_ref.max(1e-6)).sqrt();
            if m.recent.len() >= ROUGH_N as usize / 2 {
                m.rough_avg += (rough - m.rough_avg) / TREND_N;
            }
            let drift = m.baseline.as_ref().map(|b| {
                let base = k.measured(Rls::predict_with(b, &x));
                if base.abs() > 1e-9 { (pred - base) / base } else { 0.0 }
            });
            zs.push((*k, z, m.trend, m.rough_avg));
            out.insert(k.key().into(), json!({
                "label": k.label(), "unit": k.unit(),
                "measured": round(meas, 3), "expected": round(pred, 3), "residual": round(res, 3),
                "z": round(z, 2), "trend": round(m.trend, 2), "rough": round(rough, 2),
                "rmse": m.mse.map(|v| round(v.sqrt(), 4)), "drift": drift.map(|d| round(d, 4)),
            }));
        }
        let l = &mut self.learned;
        l.seen += 1;
        let warm = l.recipe_learned < self.def.warmup_s;
        let mut off: Vec<String> = zs
            .iter()
            .filter_map(|(k, z, t, r)| {
                if z.abs() >= self.def.gate_z {
                    Some(format!("{} {:+.1}σ", k.label(), z))
                } else if t.abs() >= self.def.trend_gate {
                    Some(format!("{} creeping {:+.1}σ", k.label(), t))
                } else if *r >= ROUGH_GATE {
                    Some(format!("{} rough, {:.1}× its usual spread", k.label(), r))
                } else {
                    None
                }
            })
            .collect();
        off.extend(stuck);
        let status = if warm && l.learned > l.recipe_learned {
            format!("learning recipe {}", l.recipe.as_deref().unwrap_or("?"))
        } else if warm {
            "warming up".to_string()
        } else if off.is_empty() {
            "learning".to_string()
        } else {
            format!("holding: {}", off.join(", "))
        };
        if l.seen % LEARN_EVERY == 0 && (warm || off.is_empty()) {
            for k in &kinds {
                let m = l.models.get_mut(k.key()).expect("model per kind");
                let x = m.features(*k, &s);
                let res = out[k.key()]["residual"].as_f64().unwrap_or(0.0);
                if !warm {
                    m.mse = Some(m.mse.map_or(res * res, |v| 0.995 * v + 0.005 * res * res));
                }
                m.var = 0.98 * m.var + 0.02 * res * res;
                if m.recent.len() >= ROUGH_N as usize / 2 {
                    m.rough_ref = 0.98 * m.rough_ref + 0.02 * m.rough;
                }
                m.rls.update(&x, k.target(&s));
            }
            l.learned += 1;
            l.recipe_learned += 1;
            self.rehearse();
            let l = &mut self.learned;
            if l.learned % BUFFER_EVERY == 0 {
                l.buffer.push_back(s.clone());
                if l.buffer.len() > BUFFER_MAX {
                    l.buffer.pop_front();
                }
            }
            if l.recipe_learned == self.def.warmup_s && l.baseline_at.is_none() {
                if l.prev_recipe.is_some() {
                    let r = l.recipe.clone().unwrap_or_default();
                    self.freeze_baseline(now, &format!("recipe {r}: first steady running learned"));
                    self.compare_recipes(now);
                } else {
                    self.freeze_baseline(now, "commissioning: first steady running learned");
                }
            } else if l.learned % 3600 == 0 {
                self.version(now, "another hour of steady running learned");
            }
        }
        let mut v = json!({"status": status, "freq_hz": round(s.freq, 2)});
        v["models"] = Value::Object(out);
        self.live.last = v;
    }

    /// Retrain the regressions from the accepted samples: try each candidate input (alone, × speed,
    /// × speed²), keep the ones that cut the error on held-out data (the last 30 %, by time) by 5 % or
    /// more, up to three, and refit. A model whose inputs change gets a new baseline. Returns what
    /// happened per model.
    fn train(&mut self, now: DateTime<Utc>) -> Vec<Value> {
        let out = self.retrain(now);
        self.refresh_teacher(now);
        out
    }

    fn retrain(&mut self, now: DateTime<Utc>) -> Vec<Value> {
        let rows: Vec<Sample> = self.real_rows().into_iter().cloned().collect();
        if rows.len() < 300 {
            return vec![json!({"skipped": format!("{} accepted samples; needs 300", rows.len())})];
        }
        let cut = rows.len() * 7 / 10;
        // Foreman's own samples join the fitting rows (never the held-out ones) as their share.
        let n_synth = match (&self.synth, &self.learned.teacher) {
            (Some(sy), Some(t)) if self.learned.recipe_blind || t.recipe == self.learned.recipe => (cut as f64 * sy.share / (1.0 - sy.share)).round() as usize,
            _ => 0,
        };
        let synth_rows: Vec<Sample> = (0..n_synth).filter_map(|_| self.synth_sample()).collect();
        let fit: Vec<&Sample> = rows[..cut].iter().chain(synth_rows.iter()).collect();
        let all: Vec<&Sample> = rows.iter().chain(synth_rows.iter()).collect();
        let candidates: Vec<String> = self
            .def
            .candidates()
            .into_iter()
            .filter(|c| rows.iter().filter(|s| s.x.contains_key(c)).count() * 10 >= rows.len() * 8)
            .collect();
        let stats: BTreeMap<String, (f64, f64)> = candidates
            .iter()
            .map(|c| {
                let v: Vec<f64> = rows.iter().filter_map(|s| s.x.get(c).copied()).collect();
                let mean = v.iter().sum::<f64>() / v.len() as f64;
                let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
                (c.clone(), (mean, sd.max(1e-6)))
            })
            .collect();
        let mut out = vec![];
        for k in self.def.models() {
            if k == Kind::Bus {
                continue;
            }
            let feats = |extra: &[Extra], s: &Sample| {
                let mut x = k.features(s);
                x.extend(extra.iter().map(|e| e.value(s)));
                x
            };
            // Error on the held-out rows of a model fitted on the others, in measured units.
            let score = |extra: &[Extra]| -> Option<f64> {
                let xs: Vec<Vec<f64>> = fit.iter().map(|s| feats(extra, s)).collect();
                let ys: Vec<f64> = fit.iter().map(|s| k.target(s)).collect();
                let (theta, _) = lstsq(&xs, &ys)?;
                let se: f64 = rows[cut..]
                    .iter()
                    .map(|s| (k.measured(k.target(s)) - k.measured(Rls::predict_with(&theta, &feats(extra, s)))).powi(2))
                    .sum();
                Some((se / (rows.len() - cut) as f64).sqrt())
            };
            let m = self.learned.models.get(k.key()).expect("model per kind");
            let Some(current) = score(&m.extra) else { continue };
            let mut chosen: Vec<Extra> = vec![];
            let mut best = score(&chosen).unwrap_or(f64::MAX);
            while chosen.len() < 3 {
                let mut step: Option<(f64, Extra)> = None;
                for c in &candidates {
                    for form in 0..3u8 {
                        if chosen.iter().any(|e| &e.input == c && e.form == form) {
                            continue;
                        }
                        let (mean, scale) = stats[c];
                        let e = Extra { input: c.clone(), form, mean, scale };
                        let mut trial = chosen.clone();
                        trial.push(e.clone());
                        if let Some(sc) = score(&trial) {
                            if step.as_ref().is_none_or(|(b, _)| sc < *b) {
                                step = Some((sc, e));
                            }
                        }
                    }
                }
                match step {
                    Some((sc, e)) if sc < best * 0.95 => {
                        best = sc;
                        chosen.push(e);
                    }
                    _ => break,
                }
            }
            let same = chosen.len() == m.extra.len() && chosen.iter().zip(&m.extra).all(|(a, b)| a.input == b.input && a.form == b.form);
            let labels: Vec<String> = chosen.iter().map(Extra::label).collect();
            let mut r = json!({"model": k.key(), "rmse_before": round(current, 4), "rmse_after": round(best.min(current), 4), "inputs": labels, "changed": false});
            if !same && best < current * 0.95 {
                let xs: Vec<Vec<f64>> = all.iter().map(|s| feats(&chosen, s)).collect();
                let ys: Vec<f64> = all.iter().map(|s| k.target(s)).collect();
                if let Some((theta, inv)) = lstsq(&xs, &ys) {
                    let m = self.learned.models.get_mut(k.key()).expect("model per kind");
                    m.extra = chosen;
                    m.rls.theta = theta;
                    m.rls.p = inv;
                    m.var = best * best;
                    m.mse = Some(best * best);
                    m.trend = 0.0;
                    if m.baseline.is_some() {
                        m.baseline = Some(m.rls.theta.clone());
                    }
                    r["changed"] = json!(true);
                    let note = format!(
                        "Foreman retrained {}: inputs {} (error {:.3} → {:.3} {}); baseline reset",
                        k.label(),
                        if labels.is_empty() { "speed only".into() } else { labels.join(", ") },
                        current,
                        best,
                        k.unit()
                    );
                    self.version(now, &note);
                }
            }
            out.push(r);
        }
        out
    }

    /// The accepted samples of the recipe running now.
    fn real_rows(&self) -> Vec<&Sample> {
        self.learned.buffer.iter().filter(|s| self.learned.recipe_blind || s.recipe == self.learned.recipe).collect()
    }

    fn next_u64(&mut self) -> u64 {
        let r = &mut self.learned.rng;
        if *r == 0 {
            *r = self.def.name.bytes().fold(0x9E37_79B9_7F4A_7C15u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01B3)) | 1;
        }
        *r ^= *r << 13;
        *r ^= *r >> 7;
        *r ^= *r << 17;
        *r
    }
    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (a, b) = (self.uniform().max(1e-12), self.uniform());
        (-2.0 * a.ln()).sqrt() * (2.0 * std::f64::consts::PI * b).cos()
    }

    /// One sample Foreman makes up from last night's models: a real sample's inputs at a speed drawn
    /// from the range the twin has run at, with what the models say it should read, plus their error.
    fn synth_sample(&mut self) -> Option<Sample> {
        let n = self.learned.buffer.len();
        if n == 0 || self.learned.teacher.is_none() {
            return None;
        }
        // A random accepted sample of the recipe running now (drawn until one fits: they mostly do).
        let (blind, recipe) = (self.learned.recipe_blind, self.learned.recipe.clone());
        let mut pick = None;
        for _ in 0..64 {
            let i = (self.next_u64() % n as u64) as usize;
            if blind || self.learned.buffer[i].recipe == recipe {
                pick = Some(i);
                break;
            }
        }
        let i = pick?;
        let u = self.uniform();
        let noise: Vec<f64> = (0..4).map(|_| self.gauss()).collect();
        let mut s = self.learned.buffer[i].clone();
        let t = self.learned.teacher.as_ref()?;
        let r = t.r_lo + (t.r_hi - t.r_lo) * u;
        if s.r > 0.0 {
            s.out_voltage *= r / s.r;
        }
        s.r = r;
        s.freq = r * self.def.f_base;
        for (j, k) in self.def.models().into_iter().enumerate() {
            let (theta, extra, sd) = t.models.get(k.key())?;
            let mut x = k.features(&s);
            x.extend(extra.iter().map(|e| e.value(&s)));
            let y = k.measured(Rls::predict_with(theta, &x)) + sd * noise[j];
            match k {
                Kind::Dp | Kind::DpPd => s.dp = y,
                Kind::Current | Kind::CurrentPd => s.current = y.max(0.0),
                Kind::Bus => s.dc_bus = y,
            }
        }
        Some(s)
    }

    /// After a real sample is learned, learn Foreman's own samples at their share.
    fn rehearse(&mut self) {
        let Some(sy) = self.synth.clone() else { return };
        let Some(t) = &self.learned.teacher else { return };
        if (!self.learned.recipe_blind && t.recipe != self.learned.recipe) || self.learned.recipe_learned < self.def.warmup_s {
            return;
        }
        self.learned.synth_credit += sy.share / (1.0 - sy.share);
        while self.learned.synth_credit >= 1.0 {
            self.learned.synth_credit -= 1.0;
            let Some(s) = self.synth_sample() else { return };
            for k in self.def.models() {
                let m = self.learned.models.get_mut(k.key()).expect("model per kind");
                let x = m.features(k, &s);
                m.rls.update(&x, k.target(&s));
            }
            self.learned.synth_learned += 1;
        }
    }

    /// Nightly: once the twin has run `after_days`, freeze today's models as tomorrow's teacher.
    fn refresh_teacher(&mut self, now: DateTime<Utc>) {
        let Some(sy) = &self.synth else { return };
        let l = &self.learned;
        let (Some(a), Some(b)) = (l.started_at, l.last_ts) else { return };
        if ((b - a).num_seconds() as f64) < sy.after_days * 86400.0 || l.recipe_learned < self.def.warmup_s {
            return;
        }
        let mut rs: Vec<f64> = self.real_rows().iter().map(|s| s.r).collect();
        if rs.len() < 50 {
            return;
        }
        rs.sort_by(f64::total_cmp);
        let (r_lo, r_hi) = (rs[rs.len() / 50], rs[rs.len() - 1 - rs.len() / 50]);
        let models = self.def.models().into_iter().filter_map(|k| self.learned.models.get(k.key()).map(|m| (k.key().to_string(), (m.rls.theta.clone(), m.extra.clone(), m.sd(k))))).collect();
        let first = self.learned.teacher.is_none();
        self.learned.teacher = Some(Teacher { recipe: self.learned.recipe.clone(), models, r_lo, r_hi });
        if first {
            self.version(now, &format!("Foreman started generating its own training data: {:.0} % of what this twin learns", sy.share * 100.0));
        }
    }

    fn freeze_baseline(&mut self, now: DateTime<Utc>, note: &str) {
        for m in self.learned.models.values_mut() {
            m.baseline = Some(m.rls.theta.clone());
        }
        self.learned.baseline_at = Some(now);
        self.version(now, note);
    }

    fn version(&mut self, now: DateTime<Utc>, note: &str) {
        let rmse = self.learned.models.iter().map(|(k, m)| (k.clone(), m.mse.map(|v| round(v.sqrt(), 4)))).collect();
        self.learned.history.push(Version { ts: now, learned_s: self.learned.learned, rmse, note: note.into() });
        if self.learned.history.len() > 500 {
            self.learned.history.remove(0);
        }
    }

    fn report(&self) -> Value {
        json!({
            "name": self.def.name,
            "description": self.def.description,
            "learned_s": self.learned.learned,
            "baseline_at": self.learned.baseline_at,
            "recipe": self.learned.recipe,
            "recipes_learned": self.learned.recipes.keys().collect::<Vec<_>>(),
            "recipe_blind": self.learned.recipe_blind,
            "synthetic_learned": self.learned.synth_learned,
            "now": self.live.last,
            "inputs": self.learned.models.iter().filter(|(_, m)| !m.extra.is_empty()).map(|(k, m)| (k.clone(), json!(m.extra.iter().map(Extra::label).collect::<Vec<_>>()))).collect::<serde_json::Map<_, _>>(),
            "context": self.def.context().iter().filter_map(|(r, _)| self.live.latest.get(*r).map(|(v, _)| (r.to_string(), json!(round(*v, 3))))).collect::<serde_json::Map<_, _>>(),
            "history": self.learned.history.iter().rev().take(20).collect::<Vec<_>>(),
        })
    }

    /// One paragraph of evidence for the foreman's model.
    fn evidence(&self) -> String {
        let n = &self.live.last;
        let head = format!(
            "Twin `{}` ({} of healthy steady running learned{}{})",
            self.def.name,
            human_s(self.learned.learned),
            match (&self.learned.recipe, self.learned.recipe_blind) {
                (Some(r), false) => format!(", running recipe {r}"),
                _ => String::new(),
            },
            if self.learned.recipe_learned < self.def.warmup_s { ", still warming up: treat with care" } else { "" }
        );
        let status = n["status"].as_str().unwrap_or("no data");
        let Some(models) = n["models"].as_object() else {
            return format!("{head}: {status}.");
        };
        let parts: Vec<String> = models
            .values()
            .map(|m| {
                let unit = m["unit"].as_str().unwrap_or("");
                let mut p = format!(
                    "{} {} {unit} vs {} expected ({:+}σ",
                    m["label"].as_str().unwrap_or(""),
                    m["measured"],
                    m["expected"],
                    m["z"].as_f64().unwrap_or(0.0)
                );
                if m["trend"].as_f64().is_some_and(|t| t.abs() >= 0.5) {
                    p.push_str(&format!(", 60 s average {:+}σ", m["trend"].as_f64().unwrap_or(0.0)));
                }
                match m["rough"].as_f64() {
                    Some(r) if r >= 1.8 => p.push_str(&format!(", {r:.1}× its usual roughness")),
                    Some(r) if r <= 0.4 => p.push_str(&format!(", unusually smooth ({r:.1}× its usual roughness)")),
                    _ => {}
                }
                if let Some(d) = m["drift"].as_f64().filter(|d| d.abs() >= 0.03) {
                    p.push_str(&format!(", {:+.0}% from commissioning", d * 100.0));
                }
                p.push(')');
                p
            })
            .collect();
        format!("{head}: at {} Hz, {}. Status: {status}.", n["freq_hz"], parts.join("; "))
    }
}

/// Least squares by the normal equations (a whisker of ridge for conditioning): θ and (XᵀX)⁻¹,
/// which is what recursive least squares carries as P after seeing those rows.
fn lstsq(x: &[Vec<f64>], y: &[f64]) -> Option<(Vec<f64>, Vec<Vec<f64>>)> {
    let n = x.first()?.len();
    let mut a = vec![vec![0.0; n]; n];
    let mut b = vec![0.0; n];
    for (row, yi) in x.iter().zip(y) {
        for i in 0..n {
            b[i] += row[i] * yi;
            for j in 0..n {
                a[i][j] += row[i] * row[j];
            }
        }
    }
    let tr = (0..n).map(|i| a[i][i]).sum::<f64>() / n as f64;
    for (i, row) in a.iter_mut().enumerate() {
        row[i] += 1e-9 * tr.max(1e-12);
    }
    let inv = invert(a)?;
    let theta = (0..n).map(|i| (0..n).map(|j| inv[i][j] * b[j]).sum()).collect();
    Some((theta, inv))
}

/// Gauss-Jordan inverse with partial pivoting; None when singular.
fn invert(mut a: Vec<Vec<f64>>) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let mut inv: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| if i == j { 1.0 } else { 0.0 }).collect()).collect();
    for c in 0..n {
        let p = (c..n).max_by(|&i, &j| a[i][c].abs().total_cmp(&a[j][c].abs()))?;
        if a[p][c].abs() < 1e-300 {
            return None;
        }
        a.swap(c, p);
        inv.swap(c, p);
        let d = a[c][c];
        for j in 0..n {
            a[c][j] /= d;
            inv[c][j] /= d;
        }
        for i in 0..n {
            if i != c {
                let f = a[i][c];
                if f != 0.0 {
                    for j in 0..n {
                        a[i][j] -= f * a[c][j];
                        inv[i][j] -= f * inv[c][j];
                    }
                }
            }
        }
    }
    Some(inv)
}

fn round(v: f64, d: i32) -> f64 {
    let m = 10f64.powi(d);
    (v * m).round() / m
}

fn human_s(s: u64) -> String {
    match s {
        s if s < 120 => format!("{s} s"),
        s if s < 7200 => format!("{} min", s / 60),
        s => format!("{:.1} h", s as f64 / 3600.0),
    }
}

// ------------------------------------------------------------------------------------ registry

pub struct Twins {
    state_path: PathBuf,
    config_path: PathBuf,
    twins: Mutex<Vec<Twin>>,
    synthetic: Mutex<Option<Synthetic>>,
    /// (service, tag) → (twin index, role), rebuilt with the config.
    routes: Mutex<HashMap<(String, String), Vec<(usize, String)>>>,
}

static TWINS: OnceLock<Arc<Twins>> = OnceLock::new();

/// The running twins, once `start` has been called.
pub fn get() -> Option<&'static Arc<Twins>> {
    TWINS.get()
}

/// Open `twins.yaml` and `twins.json` in the data folder and start learning from live tags.
pub fn start(data_dir: &std::path::Path, tags: &TagStore) -> Arc<Twins> {
    let t = Twins::open(data_dir);
    let _ = TWINS.set(t.clone());
    tokio::spawn(run(t.clone(), tags.clone()));
    t
}

impl Twins {
    pub fn open(data_dir: &std::path::Path) -> Arc<Self> {
        let config_path = data_dir.join("twins.yaml");
        let state_path = data_dir.join("twins.json");
        let cfg: TwinsConfig = match std::fs::read_to_string(&config_path) {
            Ok(text) => serde_yaml::from_str(&text)
                .map_err(|e| e.to_string())
                .and_then(|c: TwinsConfig| c.validate().map(|_| c))
                .unwrap_or_else(|e| {
                    tracing::warn!("{}: {e}; no twins", config_path.display());
                    TwinsConfig::default()
                }),
            Err(_) => TwinsConfig::default(),
        };
        let t = Arc::new(Self { state_path, config_path, twins: Mutex::new(vec![]), synthetic: Mutex::new(None), routes: Mutex::new(HashMap::new()) });
        t.apply(cfg);
        t
    }

    fn apply(&self, cfg: TwinsConfig) {
        let mut saved: HashMap<String, Learned> = {
            let current = self.twins.lock();
            if current.is_empty() {
                std::fs::read(&self.state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
            } else {
                current.iter().map(|t| (t.def.name.clone(), t.learned.clone())).collect()
            }
        };
        let synth = cfg.synthetic.clone();
        let twins: Vec<Twin> = cfg.twins.into_iter().map(|d| {
            let s = saved.remove(&d.name);
            let mut t = Twin::new(d, s);
            t.synth = synth.clone();
            t
        }).collect();
        *self.synthetic.lock() = synth;
        let mut routes: HashMap<(String, String), Vec<(usize, String)>> = HashMap::new();
        for (i, t) in twins.iter().enumerate() {
            for (role, key) in t.def.routed() {
                if let Some(k) = split(&key) {
                    routes.entry(k).or_default().push((i, role));
                }
            }
        }
        *self.twins.lock() = twins;
        *self.routes.lock() = routes;
    }

    pub fn config_yaml(&self) -> String {
        let cfg = TwinsConfig { twins: self.twins.lock().iter().map(|t| t.def.clone()).collect(), synthetic: self.synthetic.lock().clone() };
        serde_yaml::to_string(&cfg).unwrap_or_default()
    }

    pub fn set_config(&self, text: &str) -> Result<(), String> {
        let cfg: TwinsConfig = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        cfg.validate()?;
        std::fs::write(&self.config_path, text).map_err(|e| e.to_string())?;
        self.apply(cfg);
        self.save();
        Ok(())
    }

    fn observe(&self, e: &TagEvent) {
        if e.state.quality != Quality::Good || e.origin == Origin::User {
            return;
        }
        let routes = self.routes.lock();
        let Some(targets) = routes.get(&(e.service.clone(), e.tag.clone())) else { return };
        let Some(v) = as_f64(&e.state.value) else { return };
        let mut twins = self.twins.lock();
        for (i, role) in targets {
            if let Some(t) = twins.get_mut(*i) {
                t.live.latest.insert(role.clone(), (v, e.state.ts));
            }
        }
    }

    fn tick(&self) {
        let now = Utc::now();
        for t in self.twins.lock().iter_mut() {
            t.tick(now);
        }
    }

    pub fn report(&self) -> Value {
        json!({"twins": self.twins.lock().iter().map(|t| t.report()).collect::<Vec<_>>()})
    }

    /// Re-freeze the baseline (after a repair, a new impeller, a changed system).
    pub fn rebaseline(&self, name: &str, note: &str) -> Result<(), String> {
        let mut twins = self.twins.lock();
        let t = twins.iter_mut().find(|t| t.def.name == name).ok_or_else(|| format!("no twin `{name}`"))?;
        t.freeze_baseline(Utc::now(), note);
        drop(twins);
        self.save();
        Ok(())
    }

    /// Retrain the regressions of one twin, or all of them (`None`), from what each has accepted.
    pub fn train(&self, name: Option<&str>) -> Result<Value, String> {
        let mut twins = self.twins.lock();
        let mut out = serde_json::Map::new();
        for t in twins.iter_mut().filter(|t| name.is_none_or(|n| t.def.name == n)) {
            out.insert(t.def.name.clone(), json!(t.train(Utc::now())));
        }
        drop(twins);
        if out.is_empty() {
            return Err(format!("no twin `{}`", name.unwrap_or("")));
        }
        self.save();
        Ok(Value::Object(out))
    }

    /// Forget everything learned (a different machine on the same tags).
    pub fn reset(&self, name: &str) -> Result<(), String> {
        let mut twins = self.twins.lock();
        let i = twins.iter().position(|t| t.def.name == name).ok_or_else(|| format!("no twin `{name}`"))?;
        let def = twins[i].def.clone();
        let history = std::mem::take(&mut twins[i].learned.history);
        let synth = twins[i].synth.clone();
        twins[i] = Twin::new(def, None);
        twins[i].synth = synth;
        twins[i].learned.history = history;
        twins[i].version(Utc::now(), "reset: learning again from here (a new normal)");
        drop(twins);
        self.save();
        Ok(())
    }

    /// Evidence lines for twins that read from any of these services.
    pub fn evidence(&self, services: &[String]) -> Vec<String> {
        self.twins
            .lock()
            .iter()
            .filter(|t| t.def.signals().iter().any(|(_, k)| split(k).is_some_and(|(s, _)| services.contains(&s))))
            .map(|t| t.evidence())
            .collect()
    }

    pub fn save(&self) {
        let all: HashMap<String, Learned> = self.twins.lock().iter().map(|t| (t.def.name.clone(), t.learned.clone())).collect();
        if let Ok(b) = serde_json::to_vec(&all) {
            let tmp = self.state_path.with_extension("tmp");
            let _ = std::fs::write(&tmp, b).and_then(|_| std::fs::rename(&tmp, &self.state_path));
        }
    }
}

/// Feed tag values in, sample every 100 ms, save every minute.
pub async fn run(t: Arc<Twins>, tags: TagStore) {
    let mut events = tags.subscribe();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut save = tokio::time::interval(Duration::from_secs(60));
    // Foreman retrains every twin's regressions once a day from what it accepted.
    let mut retrain = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(86400), Duration::from_secs(86400));
    loop {
        tokio::select! {
            e = events.recv() => match e {
                Ok(e) => t.observe(&e),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            },
            _ = tick.tick() => t.tick(),
            _ = save.tick() => t.save(),
            _ = retrain.tick() => { let _ = t.train(None); }
        }
    }
}

// ------------------------------------------------------------------------------------ replay

/// One twin's day in a replay: how much it ran steady, learned and held, and each time it held.
#[derive(Default)]
struct DayStats {
    rows: u64,
    steady: u64,
    holding: u64,
    learned_before: u64,
    episodes: Vec<Value>,
    open: Option<(f64, f64, String, f64)>, // start, last, reason at peak, peak |z|
    /// Single samples far off (|z| ≥ 8) that are too short to hold: a sag, a spike.
    blips: Vec<Value>,
    /// Stretches of a minute or more with no fresh value for a signal (comms lost, a service down).
    nodata: Vec<Value>,
    nodata_open: Option<(f64, f64, String)>,
    /// When the machine began steady running after a stop (or the day's first steady sample).
    starts: Vec<f64>,
    was_steady: bool,
}

/// Feed recorded data through the twins in a data folder, file by file, as if it were live, and save
/// what they learned after each file. Each file is CSV (optionally .gz): a `t` column (Unix seconds)
/// then one column per `service/tag`. Returns, per file and twin, what happened.
pub fn replay(data_dir: &std::path::Path, files: &[PathBuf]) -> anyhow::Result<Value> {
    use std::io::{BufRead, BufReader, Read};
    let t = Twins::open(data_dir);
    let mut out = vec![];
    for file in files {
        let raw = std::fs::File::open(file).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
        let reader: Box<dyn Read> = if file.extension().is_some_and(|e| e == "gz") { Box::new(flate2::read::GzDecoder::new(raw)) } else { Box::new(raw) };
        let mut lines = BufReader::new(reader).lines();
        let header = lines.next().ok_or_else(|| anyhow::anyhow!("{}: empty", file.display()))??;
        let cols: Vec<String> = header.split(',').map(|c| c.trim().to_string()).collect();
        let routes = t.routes.lock().clone();
        let col_routes: Vec<Vec<(usize, String)>> =
            cols.iter().map(|c| split(c).and_then(|k| routes.get(&k).cloned()).unwrap_or_default()).collect();
        let mut twins = t.twins.lock();
        let mut stats: Vec<DayStats> = twins.iter().map(|tw| DayStats { learned_before: tw.learned.learned, ..Default::default() }).collect();
        let close = |s: &mut DayStats| {
            if let Some((a, b, why, z)) = s.open.take() {
                if b - a >= 30.0 {
                    s.episodes.push(json!({"from": a, "to": b, "minutes": round((b - a) / 60.0, 1), "peak_z": round(z, 1), "why": why}));
                }
            }
        };
        for line in lines {
            let line = line?;
            let mut fields = line.split(',');
            let Some(ts) = fields.next().and_then(|v| v.trim().parse::<f64>().ok()) else { continue };
            let now = DateTime::from_timestamp_millis((ts * 1000.0) as i64).unwrap_or_default();
            for (i, f) in fields.enumerate() {
                let Some(targets) = col_routes.get(i + 1) else { continue };
                if targets.is_empty() {
                    continue;
                }
                let Ok(v) = f.trim().parse::<f64>() else { continue };
                for (ti, role) in targets {
                    if let Some(tw) = twins.get_mut(*ti) {
                        tw.live.latest.insert(role.clone(), (v, now));
                    }
                }
            }
            for (tw, s) in twins.iter_mut().zip(stats.iter_mut()) {
                tw.tick(now);
                s.rows += 1;
                let st = tw.live.last["status"].as_str().unwrap_or("");
                let steady_now = tw.live.last.get("models").is_some();
                if steady_now {
                    s.steady += 1;
                    if !s.was_steady && s.starts.len() < 50 {
                        s.starts.push(ts);
                    }
                }
                s.was_steady = steady_now;
                if let Some(models) = tw.live.last.get("models").and_then(|m| m.as_object()) {
                    for (k, m) in models {
                        let z = m["z"].as_f64().unwrap_or(0.0);
                        if z.abs() >= 8.0 && s.blips.len() < 20 && s.blips.last().is_none_or(|b: &Value| ts - b["t"].as_f64().unwrap_or(0.0) > 60.0) {
                            s.blips.push(json!({"t": ts, "model": k, "z": round(z, 1)}));
                        }
                    }
                }
                if st == "no data" {
                    let why = tw.live.last["detail"].as_str().unwrap_or("").to_string();
                    match &mut s.nodata_open {
                        Some((_, last, _)) if ts - *last <= 10.0 => *last = ts,
                        _ => {
                            if let Some((a, b, w)) = s.nodata_open.take() {
                                if b - a >= 60.0 {
                                    s.nodata.push(json!({"from": a, "to": b, "why": w}));
                                }
                            }
                            s.nodata_open = Some((ts, ts, why));
                        }
                    }
                }
                if let Some(why) = st.strip_prefix("holding: ") {
                    s.holding += 1;
                    let z = tw.live.last["models"].as_object().map(|m| m.values().filter_map(|v| v["z"].as_f64()).fold(0.0f64, |a, b| a.max(b.abs()))).unwrap_or(0.0);
                    match &mut s.open {
                        Some((_, last, reason, peak)) if ts - *last <= 120.0 => {
                            *last = ts;
                            if z > *peak {
                                *peak = z;
                                *reason = why.to_string();
                            }
                        }
                        _ => {
                            close(s);
                            s.open = Some((ts, ts, why.to_string(), z));
                        }
                    }
                }
            }
        }
        let mut per = serde_json::Map::new();
        for (tw, s) in twins.iter_mut().zip(stats.iter_mut()) {
            close(s);
            if let Some((a, b, w)) = s.nodata_open.take() {
                if b - a >= 60.0 {
                    s.nodata.push(json!({"from": a, "to": b, "why": w}));
                }
            }
            let rmse: BTreeMap<String, Option<f64>> = tw.learned.models.iter().map(|(k, m)| (k.clone(), m.mse.map(|v| round(v.sqrt(), 4)))).collect();
            let drift: BTreeMap<String, Option<f64>> = match tw.live.last.get("models").and_then(|m| m.as_object()) {
                Some(m) => m.iter().map(|(k, v)| (k.clone(), v["drift"].as_f64())).collect(),
                None => BTreeMap::new(),
            };
            per.insert(tw.def.name.clone(), json!({
                "rows": s.rows, "steady": s.steady, "holding": s.holding,
                "learned": tw.learned.learned - s.learned_before, "learned_total": tw.learned.learned,
                "baseline": tw.learned.baseline_at.is_some(), "rmse": rmse, "drift": drift, "episodes": s.episodes, "blips": s.blips, "nodata": s.nodata, "starts": s.starts,
            }));
            let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if tw.learned.learned > s.learned_before {
                tw.version(Utc::now(), &format!("replayed {name}"));
            }
            // Nightly: Foreman retrains the regressions from what the twin accepted.
            let trained = tw.train(Utc::now());
            if let Some(e) = per.get_mut(&tw.def.name) {
                e["retrained"] = json!(trained.iter().filter(|r| r["changed"] == true).cloned().collect::<Vec<_>>());
                e["inputs"] = json!(tw.learned.models.iter().map(|(k, m)| (k.clone(), m.extra.iter().map(Extra::label).collect::<Vec<_>>())).collect::<BTreeMap<_, _>>());
            }
        }
        let defs: Vec<TwinDef> = twins.iter().map(|tw| tw.def.clone()).collect();
        drop(twins);
        t.save();
        let plant = plant_events(&defs, &per);
        out.push(json!({"file": file.display().to_string(), "twins": per, "plant": plant}));
    }
    Ok(Value::Array(out))
}

/// Holds that start within this long of each other on machines joined by the process are one event.
const CHAIN_S: f64 = 900.0;
/// DC bus blips this close together on this many drives are the supply, not the machines.
const SUPPLY_S: f64 = 2.0;
const SUPPLY_MIN: usize = 3;
/// A hold that begins this soon after the machine starts was there when it started (worn lobes, a cold
/// cooler): a condition, not an upset arriving down the line, so it isn't chained to its neighbours.
const STARTUP_S: f64 = 300.0;
/// Holds closer together than this can't say which came first: a strong residual crosses its
/// threshold sooner than a weak one that arrived earlier.
const ORDER_S: f64 = 120.0;

/// One day's holds and blips, read across the plant: the twins only see their own machine, this sees
/// that a blockage at P-401 and a dip at the aerator upstream are one event, and that a DC bus blip on
/// thirteen drives at once is the supply. Machines are joined by `downstream` (from the P&ID), either
/// way, up to two machines apart.
pub fn plant_events(defs: &[TwinDef], per: &serde_json::Map<String, Value>) -> Vec<Value> {
    let idx: BTreeMap<&str, usize> = defs.iter().enumerate().map(|(i, d)| (d.name.as_str(), i)).collect();
    let n = defs.len();
    // Directed reach (downstream, any distance) and undirected distance ≤ 2.
    let mut down = vec![vec![false; n]; n];
    let mut adj = vec![vec![]; n];
    for (i, d) in defs.iter().enumerate() {
        for m in &d.downstream {
            if let Some(&j) = idx.get(m.as_str()) {
                if i != j {
                    down[i][j] = true;
                    adj[i].push(j);
                    adj[j].push(i);
                }
            }
        }
    }
    for k in 0..n {
        for i in 0..n {
            if down[i][k] {
                for j in 0..n {
                    if down[k][j] {
                        down[i][j] = true;
                    }
                }
            }
        }
    }
    let near = |a: usize, b: usize| adj[a].contains(&b) || adj[a].iter().any(|&m| adj[m].contains(&b));
    let mut out = vec![];

    // Supply: DC bus blips at the same moment on several drives.
    let mut bus: Vec<(f64, &str)> = vec![];
    for d in defs {
        for b in per.get(&d.name).and_then(|e| e["blips"].as_array()).into_iter().flatten() {
            if b["model"] == "dc_bus" {
                bus.push((b["t"].as_f64().unwrap_or(0.0), d.name.as_str()));
            }
        }
    }
    bus.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut used_bus = vec![false; bus.len()];
    for i in 0..bus.len() {
        if used_bus[i] {
            continue;
        }
        let group: Vec<usize> = (i..bus.len()).take_while(|&j| bus[j].0 - bus[i].0 <= SUPPLY_S).collect();
        let names: std::collections::BTreeSet<&str> = group.iter().map(|&j| bus[j].1).collect();
        if names.len() >= SUPPLY_MIN {
            group.iter().for_each(|&j| used_bus[j] = true);
            out.push(json!({"t": bus[i].0, "kind": "supply", "machines": names,
                "summary": format!("DC bus dipped on {} drives at once: the supply, not the machines.", names.len())}));
        }
    }

    // Process: holds joined along the piping.
    let mut eps: Vec<(f64, f64, usize, String, f64)> = vec![];
    for (i, d) in defs.iter().enumerate() {
        for e in per.get(&d.name).and_then(|e| e["episodes"].as_array()).into_iter().flatten() {
            eps.push((e["from"].as_f64().unwrap_or(0.0), e["to"].as_f64().unwrap_or(0.0), i, e["why"].as_str().unwrap_or("").to_string(), e["peak_z"].as_f64().unwrap_or(0.0)));
        }
    }
    eps.sort_by(|a, b| a.0.total_cmp(&b.0));
    let at_start = |from: f64, i: usize| {
        per.get(&defs[i].name).and_then(|e| e["starts"].as_array()).into_iter().flatten().filter_map(|v| v.as_f64()).any(|s| (0.0..=STARTUP_S).contains(&(from - s)))
    };
    let chainable: Vec<bool> = eps.iter().map(|e| !at_start(e.0, e.2)).collect();
    let mut parent: Vec<usize> = (0..eps.len()).collect();
    fn root(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    for a in 0..eps.len() {
        for b in a + 1..eps.len() {
            if eps[b].0 - eps[a].0 > CHAIN_S {
                break;
            }
            if chainable[a] && chainable[b] && eps[a].2 != eps[b].2 && near(eps[a].2, eps[b].2) {
                let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
                parent[rb] = ra;
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..eps.len() {
        let r = root(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    for (_, g) in groups {
        let first = &eps[g[0]];
        let mut names: Vec<usize> = vec![];
        for &i in &g {
            if !names.contains(&eps[i].2) {
                names.push(eps[i].2);
            }
        }
        let machines: Vec<Value> = g.iter().map(|&i| json!({"name": defs[eps[i].2].name, "from": eps[i].0, "to": eps[i].1, "peak_z": eps[i].4, "why": eps[i].3})).collect();
        let (kind, summary) = if names.len() == 1 {
            ("machine", format!("{} alone: {}", defs[first.2].name, first.3))
        } else {
            let s = names[0];
            let rest = &names[1..];
            let label = |v: &[usize]| v.iter().map(|&i| defs[i].name.as_str()).collect::<Vec<_>>().join(", ");
            let second = g.iter().map(|&i| &eps[i]).find(|e| e.2 != s).map(|e| e.0).unwrap_or(first.0);
            if second - first.0 < ORDER_S {
                // In flow order: a machine before the ones it feeds.
                let mut flow = names.clone();
                flow.sort_by_key(|&i| std::cmp::Reverse(names.iter().filter(|&&j| down[i][j]).count()));
                let arrows = flow.iter().map(|&i| defs[i].name.as_str()).collect::<Vec<_>>().join(" → ");
                ("process", format!("{arrows} held within {:.0} s of each other: one upset along the line, too close together to tell where it started.", second - first.0))
            } else if rest.iter().all(|&j| down[s][j]) {
                ("process", format!("Started at {} and reached {} downstream: one upset travelling with the product.", defs[s].name, label(rest)))
            } else if rest.iter().all(|&j| down[j][s]) {
                ("process", format!("Started at {} and backed up to {} upstream: look at {} first.", defs[s].name, label(rest), defs[s].name))
            } else {
                ("process", format!("{} and {} held together on connected machines.", defs[s].name, label(rest)))
            }
        };
        out.push(json!({"t": first.0, "kind": kind, "machines": machines, "summary": summary}));
    }
    out.sort_by(|a, b| a["t"].as_f64().unwrap_or(0.0).total_cmp(&b["t"].as_f64().unwrap_or(0.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plant_events_join_along_the_piping() {
        let d = |name: &str, down: &[&str]| -> TwinDef {
            let mut t: TwinDef = serde_yaml::from_str(&format!("{{name: {name}, freq: a/f, current: a/i, kind: drive_load}}")).unwrap();
            t.downstream = down.iter().map(|s| s.to_string()).collect();
            t
        };
        let defs = vec![d("AE", &["P"]), d("P", &["CV"]), d("CV", &[]), d("X", &[]), d("Y", &[]), d("Z", &[])];
        let ep = |from: f64, why: &str| json!({"from": from, "to": from + 60.0, "peak_z": 9.0, "why": why});
        let blip = |t: f64| json!({"t": t, "model": "dc_bus", "z": -40.0});
        let mut per = serde_json::Map::new();
        per.insert("P".into(), json!({"episodes": [ep(1000.0, "pressure high")], "blips": [blip(50.0)]}));
        per.insert("AE".into(), json!({"episodes": [ep(1200.0, "current low")], "blips": [blip(50.5)]}));
        per.insert("CV".into(), json!({"episodes": [ep(5000.0, "current high")], "blips": [blip(51.0)]}));
        per.insert("X".into(), json!({"episodes": [ep(1100.0, "current high")], "blips": []}));
        // Y and Z are joined and both hold as they start: two conditions, not one upset.
        let mut defs = defs;
        defs[4].downstream = vec!["Z".into()];
        per.insert("Y".into(), json!({"episodes": [ep(20060.0, "worn")], "starts": [20000.0]}));
        per.insert("Z".into(), json!({"episodes": [ep(20070.0, "cold")], "starts": [20000.0]}));
        let ev = plant_events(&defs, &per);
        let kinds: Vec<&str> = ev.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["supply", "process", "machine", "machine", "machine", "machine"], "{ev:#?}");
        assert!(ev[1]["summary"].as_str().unwrap().contains("Started at P and backed up to AE"), "{}", ev[1]["summary"]);
        assert_eq!(ev[2]["machines"][0]["name"], "X", "X isn't connected: its own event");
    }

    fn def() -> TwinDef {
        serde_yaml::from_str("{name: p1, freq: d/f, current: d/i, discharge: d/pd, suction: d/ps, warmup_s: 60}").unwrap()
    }

    /// A healthy pump: ΔP = 1 + 4r², I² = 100 + 300r² + 600r⁴, a little noise.
    fn feed(t: &mut Twin, now: DateTime<Utc>, f: f64, dp_mul: f64, i_mul: f64, n: u64) -> DateTime<Utc> {
        let mut now = now;
        for k in 0..n {
            let r = f / 60.0;
            let wob = ((k % 7) as f64 - 3.0) * 0.003;
            let dp = (1.0 + 4.0 * r * r) * dp_mul + wob;
            let i = (100.0 + 300.0 * r * r + 600.0 * r.powi(4)).sqrt() * i_mul + wob * 10.0;
            for (role, v) in [("freq", f), ("current", i), ("discharge", dp + 0.3 + wob * 0.2), ("suction", 0.3 + wob * 0.2)] {
                t.live.latest.insert(role.to_string(), (v, now));
            }
            t.tick(now);
            now += chrono::TimeDelta::milliseconds(100);
        }
        now
    }

    #[test]
    fn learns_a_healthy_pump_then_holds_on_a_fault() {
        let mut t = Twin::new(def(), None);
        let mut now = Utc::now();
        for f in [40.0, 50.0, 60.0, 45.0, 55.0] {
            now = feed(&mut t, now, f, 1.0, 1.0, 1500);
        }
        assert!(t.learned.baseline_at.is_some(), "baseline frozen after warm-up");
        // Healthy at a speed it saw: small residuals, still learning.
        now = feed(&mut t, now, 52.0, 1.0, 1.0, 300);
        let m = &t.live.last["models"];
        assert!(m["dp"]["z"].as_f64().unwrap().abs() < 3.0, "{m}");
        assert_eq!(t.live.last["status"], "learning", "{}", t.live.last);
        // Deadhead: pressure up 30 %, current down 30 %. The twin flags it and stops learning.
        let before = t.learned.learned;
        feed(&mut t, now, 52.0, 1.3, 0.7, 300);
        let m = &t.live.last["models"];
        assert!(m["dp"]["z"].as_f64().unwrap() > 4.0, "{m}");
        assert!(m["current"]["z"].as_f64().unwrap() < -4.0, "{m}");
        assert!(t.live.last["status"].as_str().unwrap().starts_with("holding"), "{}", t.live.last);
        assert!(t.learned.learned - before <= 1, "didn't learn the fault");
        assert!(t.evidence().contains("motor current"), "{}", t.evidence());
    }

    #[test]
    fn foreman_trains_in_an_input_that_explains_the_load() {
        // A scraped-surface rotor: torque rises as the product leaving it gets colder.
        let mut d = def();
        d.kind = TwinKind::DriveLoad;
        d.discharge = None;
        d.suction = None;
        d.inputs = vec!["d/tt".into()];
        let mut t = Twin::new(d, None);
        let mut now = Utc::now();
        let mut k = 0u64;
        for f in [40.0, 45.0, 50.0, 42.0, 48.0, 44.0, 46.0, 41.0, 49.0, 43.0] {
            for _ in 0..3000 {
                let temp = 18.0 + 4.0 * ((k as f64) / 700.0).sin();
                let r: f64 = f / 60.0;
                let torque = 0.3 + 0.5 * r * (1.0 + 0.04 * (18.0 - temp));
                let i = 30.0 * (0.33f64.powi(2) + (0.94 * torque).powi(2)).sqrt() + (((k * 7919) % 13) as f64 - 6.0) * 0.01;
                for (role, v) in [("freq", f), ("current", i), ("d/tt", temp)] {
                    t.live.latest.insert(role.to_string(), (v, now));
                }
                t.tick(now);
                now += chrono::TimeDelta::milliseconds(100);
                k += 1;
            }
        }
        assert!(t.learned.buffer.len() >= 300, "buffer {}", t.learned.buffer.len());
        let r = t.train(now);
        let cur = r.iter().find(|x| x["model"] == "current").expect("current model");
        assert_eq!(cur["changed"], true, "{r:?}");
        assert!(cur["inputs"].as_array().unwrap().iter().any(|x| x.as_str().unwrap().starts_with("d/tt")), "{r:?}");
        assert!(cur["rmse_after"].as_f64().unwrap() < cur["rmse_before"].as_f64().unwrap() * 0.5, "{r:?}");
        assert!(t.learned.history.last().unwrap().note.contains("Foreman retrained"));
    }

    #[test]
    fn a_recipe_gets_its_own_models_and_a_stuck_transmitter_holds() {
        // A mixer whose load is 40 % higher on recipe 2, and a pump beside it that doesn't care.
        let mut mixer = def();
        mixer.kind = TwinKind::DriveLoad;
        mixer.discharge = None;
        mixer.suction = None;
        mixer.recipe = Some("plc/recipe".into());
        let mut pump = def();
        pump.recipe = Some("plc/recipe".into());
        let (mut m, mut p) = (Twin::new(mixer, None), Twin::new(pump, None));
        let mut now = Utc::now();
        let mut k = 0u64;
        let mut run = |m: &mut Twin, p: &mut Twin, now: &mut DateTime<Utc>, recipe: f64, n: u64, stuck: Option<f64>| {
            for _ in 0..n {
                let f = [40.0, 50.0, 45.0, 55.0][((k / 1500) % 4) as usize];
                let r: f64 = f / 60.0;
                let wob = (((k * 7919) % 13) as f64 - 6.0) * 0.004;
                let load = if recipe == 2.0 { 1.4 } else { 1.0 };
                let i = 30.0 * (0.1 + 0.6 * r * r * load) + wob * 10.0;
                let dp = 1.0 + 4.0 * r * r + wob;
                for (role, v) in [("freq", f), ("current", i), ("recipe", recipe)] {
                    m.live.latest.insert(role.to_string(), (v, *now));
                }
                for (role, v) in [("freq", f), ("current", (100.0 + 300.0 * r * r).sqrt() + wob * 10.0), ("discharge", stuck.unwrap_or(dp + 0.3)), ("suction", 0.3 + wob * 0.2), ("recipe", recipe)] {
                    p.live.latest.insert(role.to_string(), (v, *now));
                }
                m.tick(*now);
                p.tick(*now);
                *now += chrono::TimeDelta::milliseconds(100);
                k += 1;
            }
        };
        run(&mut m, &mut p, &mut now, 1.0, 12000, None);
        assert!(m.learned.baseline_at.is_some());
        assert_eq!(m.learned.recipe.as_deref(), Some("1"));
        // Recipe 2: the mixer learns it as new instead of holding all day; the pump is found not to care.
        run(&mut m, &mut p, &mut now, 2.0, 12000, None);
        assert_eq!(m.learned.recipe.as_deref(), Some("2"));
        assert!(m.learned.baseline_at.is_some(), "recipe 2 learned");
        assert!(!m.learned.recipe_blind, "{:?}", m.learned.history.last());
        assert!(m.learned.recipes.contains_key("1"));
        assert!(p.learned.recipe_blind, "{:?}", p.learned.history.iter().rev().take(3).collect::<Vec<_>>());
        assert_eq!(m.live.last["status"], "learning", "{}", m.live.last);
        // Back to recipe 1: what was learned for it, no hold.
        run(&mut m, &mut p, &mut now, 1.0, 3000, None);
        assert_eq!(m.live.last["status"], "learning", "{}", m.live.last);
        assert!(m.learned.history.iter().any(|v| v.note.contains("back to what was learned")));
        // The pump's discharge transmitter freezes at its last value: held once it's been ten minutes.
        let last = p.live.latest["discharge"].0;
        run(&mut m, &mut p, &mut now, 1.0, 5000, Some(last));
        assert_eq!(p.live.last["status"], "learning", "not yet: {}", p.live.last);
        run(&mut m, &mut p, &mut now, 1.0, 1500, Some(last));
        assert!(p.live.last["status"].as_str().unwrap().contains("d/pd stuck at"), "{}", p.live.last);
    }

    #[test]
    fn foreman_learns_its_own_samples_at_their_share() {
        let mut t = Twin::new(def(), None);
        t.synth = Some(Synthetic { share: 0.5, after_days: 0.0 });
        let mut now = Utc::now();
        for f in [40.0, 50.0, 60.0, 45.0, 55.0] {
            now = feed(&mut t, now, f, 1.0, 1.0, 1500);
        }
        assert!(t.learned.teacher.is_none(), "no teacher before the first night");
        t.train(now);
        let tch = t.learned.teacher.clone().expect("teacher after the night");
        assert!(tch.r_lo < 0.7 && tch.r_hi > 0.95, "{} {}", tch.r_lo, tch.r_hi);
        let real = t.learned.learned;
        now = feed(&mut t, now, 52.0, 1.0, 1.0, 3000);
        let synth = t.learned.synth_learned;
        let real = t.learned.learned - real;
        assert!(synth.abs_diff(real) <= 1, "half the samples learned are synthetic: {synth} vs {real}");
        // Made-up samples look like the pump: the model they train still explains the real one.
        assert_eq!(t.live.last["status"], "learning", "{}", t.live.last);
        assert!(t.learned.history.iter().any(|v| v.note.contains("its own training data")));
        let s = t.synth_sample().unwrap();
        let dp_true = 1.0 + 4.0 * s.r * s.r;
        assert!((s.dp - dp_true).abs() < 0.1, "{} vs {}", s.dp, dp_true);
        let _ = now;
    }

    #[test]
    fn stale_signals_say_so() {
        let mut t = Twin::new(def(), None);
        t.tick(Utc::now());
        assert_eq!(t.live.last["status"], "no data");
    }
}
