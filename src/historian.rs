//! The historian: Cowpoke's own small time-series store.
//!
//! Layout: `<data>/history/<service>/<tag>/<YYYY-MM-DD>.bin`, each record 16 bytes
//! (i64 milliseconds since the epoch, f64 value, little-endian). One file per tag per UTC day
//! keeps writes append-only and makes retention a matter of deleting old files.
//! Bools are stored as 0/1; string tags aren't historized.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use parking_lot::Mutex;
use serde::Serialize;

use crate::model::{Config, DataType, HistoryCfg, HistoryMode};
use crate::tags::{Quality, TagStore, as_f64};

#[derive(Clone)]
struct Tracked {
    cfg: HistoryCfg,
    last_value: Option<f64>,
    last_record: Option<i64>,
}

pub struct Historian {
    dir: PathBuf,
    tracked: Mutex<HashMap<(String, String), Tracked>>,
    pending: Mutex<Vec<(String, String, i64, f64)>>,
}

#[derive(Serialize)]
pub struct Point {
    /// Milliseconds since the epoch.
    pub t: i64,
    pub v: f64,
    /// Bucket min and max when the series was downsampled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

/// A file-system-safe folder name (never `.` or `..`).
fn safe(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '~' })
        .collect();
    if out.starts_with('.') { format!("~{out}") } else { out }
}

impl Historian {
    pub fn new(data_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            dir: data_dir.join("history"),
            tracked: Default::default(),
            pending: Default::default(),
        })
    }

    fn tag_dir(&self, service: &str, tag: &str) -> PathBuf {
        self.dir.join(safe(service)).join(safe(tag))
    }

    /// Pick up the tags that have `history:` set.
    pub fn configure(&self, cfg: &Config) {
        let mut tr = self.tracked.lock();
        let mut next = HashMap::new();
        for s in &cfg.services {
            for t in &s.tags {
                let Some(h) = &t.history else { continue };
                if t.datatype == DataType::String {
                    continue;
                }
                let key = (s.id.clone(), t.name.clone());
                let prev = tr.remove(&key);
                next.insert(
                    key,
                    Tracked {
                        cfg: h.clone(),
                        last_value: prev.as_ref().and_then(|p| p.last_value),
                        last_record: prev.and_then(|p| p.last_record),
                    },
                );
            }
        }
        *tr = next;
    }

    fn offer(&self, service: &str, tag: &str, t: i64, v: f64, from_interval: bool) {
        let mut tr = self.tracked.lock();
        let Some(x) = tr.get_mut(&(service.to_string(), tag.to_string())) else {
            return;
        };
        let record = match x.cfg.mode {
            HistoryMode::Deadband if from_interval => false,
            HistoryMode::Deadband => match x.last_value {
                None => true,
                Some(last) => (v - last).abs() > x.cfg.deadband || (x.cfg.deadband == 0.0 && v != last),
            },
            HistoryMode::Interval => {
                from_interval
                    && x.last_record
                        .is_none_or(|lr| t - lr >= x.cfg.interval_ms as i64 - 5)
            }
        };
        if record {
            x.last_value = Some(v);
            x.last_record = Some(t);
            self.pending
                .lock()
                .push((service.to_string(), tag.to_string(), t, v));
        }
    }

    /// Run forever: record changes, sample interval tags, flush to disk, apply retention.
    pub async fn run(self: Arc<Self>, store: TagStore) {
        let mut events = store.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut flush = tokio::time::interval(Duration::from_secs(1));
        let mut sweep = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(e) if e.state.quality == Quality::Good => {
                        if let Some(v) = as_f64(&e.state.value) {
                            self.offer(&e.service, &e.tag, e.state.ts.timestamp_millis(), v, false);
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("historian fell behind by {n} changes");
                    }
                    Err(_) => return,
                },
                _ = tick.tick() => {
                    let now = Utc::now().timestamp_millis();
                    let due: Vec<(String, String)> = self.tracked.lock().iter()
                        .filter(|(_, x)| x.cfg.mode == HistoryMode::Interval
                            && x.last_record.is_none_or(|lr| now - lr >= x.cfg.interval_ms.max(100) as i64 - 5))
                        .map(|(k, _)| k.clone())
                        .collect();
                    for (s, t) in due {
                        if let Some(st) = store.get(&s, &t) {
                            if st.quality == Quality::Good {
                                if let Some(v) = as_f64(&st.value) {
                                    self.offer(&s, &t, now, v, true);
                                }
                            }
                        }
                    }
                }
                _ = flush.tick() => {
                    let h = self.clone();
                    let _ = tokio::task::spawn_blocking(move || h.flush()).await;
                }
                _ = sweep.tick() => {
                    let h = self.clone();
                    let _ = tokio::task::spawn_blocking(move || h.apply_retention()).await;
                }
            }
        }
    }

    /// Add points recorded elsewhere (an edge agent's backfill). Untracked tags are ignored.
    pub fn insert(&self, points: Vec<(String, String, i64, f64)>) {
        let tr = self.tracked.lock();
        let mut p = self.pending.lock();
        for pt in points {
            if tr.contains_key(&(pt.0.clone(), pt.1.clone())) {
                p.push(pt);
            }
        }
    }

    /// Every historized (service, tag).
    pub fn tracked(&self) -> Vec<(String, String)> {
        self.tracked.lock().keys().cloned().collect()
    }

    pub fn flush(&self) {
        let batch = std::mem::take(&mut *self.pending.lock());
        let mut by_file: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        for (s, t, ts, v) in batch {
            let day = Utc.timestamp_millis_opt(ts).single().unwrap_or_default().date_naive();
            let path = self.tag_dir(&s, &t).join(format!("{day}.bin"));
            let buf = by_file.entry(path).or_default();
            buf.extend_from_slice(&ts.to_le_bytes());
            buf.extend_from_slice(&v.to_le_bytes());
        }
        for (path, bytes) in by_file {
            let res = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(path.parent().unwrap())?;
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
                f.write_all(&bytes)
            })();
            if let Err(e) = res {
                tracing::error!("historian: writing {}: {e}", path.display());
            }
        }
    }

    fn apply_retention(&self) {
        let tracked = self.tracked.lock().clone();
        let today = Utc::now().date_naive();
        for ((s, t), x) in tracked {
            let dir = self.tag_dir(&s, &t);
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(day) = name.strip_suffix(".bin").and_then(|d| d.parse::<NaiveDate>().ok()) else {
                    continue;
                };
                if (today - day).num_days() > x.cfg.retention_days as i64 {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    /// Points between `from` and `to`, averaged into at most `max_points` buckets.
    pub fn query(
        &self,
        service: &str,
        tag: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        max_points: usize,
    ) -> Vec<Point> {
        let (f, t) = (from.timestamp_millis(), to.timestamp_millis());
        let raw = self.raw(service, tag, f, t);
        Self::downsample(raw, f, t, max_points)
    }

    /// Every stored point between `f` and `t` (milliseconds), sorted by time.
    pub fn raw(&self, service: &str, tag: &str, f: i64, t: i64) -> Vec<(i64, f64)> {
        self.flush();
        let day_of = |ms: i64| Utc.timestamp_millis_opt(ms).single().map(|d| d.date_naive());
        let (from_day, to_day) = (day_of(f), day_of(t));
        let mut raw: Vec<(i64, f64)> = vec![];
        // Only the day files that exist, so wide ranges cost nothing.
        let Ok(rd) = std::fs::read_dir(self.tag_dir(service, tag)) else {
            return raw;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(day) = name.strip_suffix(".bin").and_then(|d| d.parse::<NaiveDate>().ok()) else {
                continue;
            };
            if from_day.is_some_and(|d| day < d) || to_day.is_some_and(|d| day > d) {
                continue;
            }
            let mut buf = vec![];
            if std::fs::File::open(e.path()).and_then(|mut f| f.read_to_end(&mut buf)).is_err() {
                continue;
            }
            for rec in buf.chunks_exact(16) {
                let ts = i64::from_le_bytes(rec[..8].try_into().unwrap());
                let v = f64::from_le_bytes(rec[8..].try_into().unwrap());
                if ts >= f && ts <= t {
                    raw.push((ts, v));
                }
            }
        }
        raw.sort_by_key(|p| p.0);
        raw
    }

    fn downsample(raw: Vec<(i64, f64)>, f: i64, t: i64, max_points: usize) -> Vec<Point> {
        let max_points = max_points.clamp(10, 10_000);
        if raw.len() <= max_points {
            return raw.into_iter().map(|(t, v)| Point { t, v, min: None, max: None }).collect();
        }
        let width = ((t - f) as f64 / max_points as f64).max(1.0);
        let mut out: Vec<Point> = vec![];
        let mut cur: Option<(i64, f64, f64, f64, usize)> = None; // bucket, sum, min, max, n
        for (ts, v) in raw {
            let b = ((ts - f) as f64 / width) as i64;
            match cur.as_mut() {
                Some(c) if c.0 == b => {
                    c.1 += v;
                    c.2 = c.2.min(v);
                    c.3 = c.3.max(v);
                    c.4 += 1;
                }
                _ => {
                    if let Some(c) = cur.take() {
                        out.push(bucket_point(f, width, c));
                    }
                    cur = Some((b, v, v, v, 1));
                }
            }
        }
        if let Some(c) = cur {
            out.push(bucket_point(f, width, c));
        }
        out
    }
}

fn bucket_point(from: i64, width: f64, c: (i64, f64, f64, f64, usize)) -> Point {
    Point {
        t: from + (c.0 as f64 * width + width / 2.0) as i64,
        v: c.1 / c.4 as f64,
        min: Some(c.2),
        max: Some(c.3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Mode, ServiceDef, TagDef};

    #[test]
    fn deadband_and_query() {
        let dir = tempfile::tempdir().unwrap();
        let h = Historian::new(dir.path());
        let cfg = Config {
            services: vec![ServiceDef {
                id: "s".into(),
                name: String::new(),
                kind: Kind::Api,
                mode: Mode::Server,
                enabled: true,
                agent: None,
                settings: serde_json::json!({}),
                tags: vec![TagDef {
                    name: "temp".into(),
                    datatype: DataType::Float64,
                    history: Some(HistoryCfg {
                        mode: HistoryMode::Deadband,
                        deadband: 0.5,
                        interval_ms: 1000,
                        retention_days: 30,
                    }),
                    ..Default::default()
                }],
            }],
        };
        h.configure(&cfg);
        let t0 = Utc::now().timestamp_millis();
        for (i, v) in [20.0, 20.2, 20.6, 20.7, 30.0].iter().enumerate() {
            h.offer("s", "temp", t0 + i as i64 * 1000, *v, false);
        }
        let pts = h.query(
            "s",
            "temp",
            Utc.timestamp_millis_opt(t0 - 1).unwrap(),
            Utc.timestamp_millis_opt(t0 + 10_000).unwrap(),
            100,
        );
        let vals: Vec<f64> = pts.iter().map(|p| p.v).collect();
        assert_eq!(vals, vec![20.0, 20.6, 30.0]);
    }
}
