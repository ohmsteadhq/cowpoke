//! The historian: Cowpoke's own small time-series store.
//!
//! Layout, per tag: `<data>/history/<service>/<tag>/`, one set of files per UTC day:
//!
//! - `<YYYY-MM-DD>.bin`: new points, appended once a second. Each record is 16 bytes
//!   (i64 milliseconds since the epoch, f64 value, little-endian).
//! - `<YYYY-MM-DD>.gor`: a closed day, compressed (see [`crate::tsdb`]). Days are compacted
//!   at startup and every hour once they're over; late points (an edge agent's backfill)
//!   land in a `.bin` beside it and are merged at the next compaction.
//! - `<YYYY-MM-DD>.hour`: hourly rollups (count, min, max, average, first, last) of a
//!   compacted day. Queries whose buckets are an hour or wider read these instead of points.
//!
//! Retention deletes whole days. Bools are stored as 0/1; string tags aren't historized.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use parking_lot::Mutex;
use serde::Serialize;

use crate::model::{Config, DataType, HistoryCfg, HistoryMode};
use crate::tsdb::{self, HOUR_MS, Hour};
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
    /// Held while files change or are read, so a compaction never races a write or a query.
    io: Mutex<()>,
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
            io: Mutex::new(()),
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
                    // Fires at startup too: compact days that ended while Cowpoke was stopped.
                    let h = self.clone();
                    let _ = tokio::task::spawn_blocking(move || h.maintain()).await;
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
        if batch.is_empty() {
            return;
        }
        let _io = self.io.lock();
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

    /// Apply retention, then compact every finished day.
    pub fn maintain(&self) {
        self.flush();
        let tracked = self.tracked.lock().clone();
        let today = Utc::now().date_naive();
        for ((s, t), x) in &tracked {
            let dir = self.tag_dir(s, t);
            for day in days_in(&dir) {
                if (today - day).num_days() > x.cfg.retention_days as i64 {
                    let _io = self.io.lock();
                    for ext in ["bin", "gor", "hour"] {
                        let _ = std::fs::remove_file(dir.join(format!("{day}.{ext}")));
                    }
                }
            }
        }
        // Compact every tag folder, including tags no longer historized.
        let Ok(services) = std::fs::read_dir(&self.dir) else { return };
        for svc in services.flatten() {
            let Ok(tags) = std::fs::read_dir(svc.path()) else { continue };
            for tag in tags.flatten() {
                let dir = tag.path();
                for day in days_in(&dir) {
                    if day < today && dir.join(format!("{day}.bin")).exists() {
                        if let Err(e) = self.compact(&dir, day) {
                            tracing::error!("historian: compacting {}/{day}: {e:#}", dir.display());
                        }
                    }
                }
            }
        }
    }

    /// Merge a day's appended points into its compressed file and rebuild its rollups.
    fn compact(&self, dir: &Path, day: NaiveDate) -> anyhow::Result<()> {
        let _io = self.io.lock();
        let mut pts = read_day(dir, day);
        pts.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        pts.dedup_by(|a, b| a.0 == b.0 && a.1.to_bits() == b.1.to_bits());
        let gor = dir.join(format!("{day}.gor"));
        let tmp = dir.join(format!("{day}.gor.tmp"));
        std::fs::write(&tmp, tsdb::encode(&pts))?;
        std::fs::rename(&tmp, &gor)?;
        let hour = dir.join(format!("{day}.hour"));
        let htmp = dir.join(format!("{day}.hour.tmp"));
        std::fs::write(&htmp, tsdb::encode_hours(&tsdb::rollup(&pts)))?;
        std::fs::rename(&htmp, &hour)?;
        std::fs::remove_file(dir.join(format!("{day}.bin")))?;
        Ok(())
    }

    /// Points between `from` and `to`, at most about `max_points` of them: raw points when
    /// they fit, otherwise buckets with their average, min and max. Buckets an hour wide or
    /// wider come from the hourly rollups.
    pub fn query(
        &self,
        service: &str,
        tag: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        max_points: usize,
    ) -> Vec<Point> {
        let (f, t) = (from.timestamp_millis(), to.timestamp_millis());
        let max_points = max_points.clamp(10, 10_000);
        if (t - f) / max_points as i64 >= HOUR_MS {
            let hours = self.hours(service, tag, f, t);
            return Self::from_hours(hours, f, t, max_points);
        }
        let raw = self.raw(service, tag, f, t);
        Self::downsample(raw, f, t, max_points)
    }

    /// Every stored point between `f` and `t` (milliseconds), sorted by time.
    pub fn raw(&self, service: &str, tag: &str, f: i64, t: i64) -> Vec<(i64, f64)> {
        self.flush();
        let dir = self.tag_dir(service, tag);
        let _io = self.io.lock();
        let mut raw: Vec<(i64, f64)> = days_between(&dir, f, t)
            .into_iter()
            .flat_map(|day| read_day(&dir, day))
            .filter(|p| p.0 >= f && p.0 <= t)
            .collect();
        raw.sort_by_key(|p| p.0);
        raw
    }

    /// Hourly rollups for every hour touching `f`..`t`: stored ones for compacted days,
    /// computed on the fly for days still being written.
    pub fn hours(&self, service: &str, tag: &str, f: i64, t: i64) -> Vec<Hour> {
        self.flush();
        let dir = self.tag_dir(service, tag);
        let _io = self.io.lock();
        let (hf, ht) = (f.div_euclid(HOUR_MS) * HOUR_MS, t);
        let mut out = vec![];
        for day in days_between(&dir, f, t) {
            let stored = (!dir.join(format!("{day}.bin")).exists())
                .then(|| std::fs::read(dir.join(format!("{day}.hour"))).ok())
                .flatten()
                .and_then(|b| tsdb::decode_hours(&b).ok());
            let hours = stored.unwrap_or_else(|| {
                let mut pts = read_day(&dir, day);
                pts.sort_by_key(|p| p.0);
                tsdb::rollup(&pts)
            });
            out.extend(hours.into_iter().filter(|h| h.start >= hf && h.start <= ht));
        }
        out.sort_by_key(|h| h.start);
        out
    }

    fn from_hours(hours: Vec<Hour>, f: i64, t: i64, max_points: usize) -> Vec<Point> {
        let width = ((t - f) as f64 / max_points as f64).max(HOUR_MS as f64);
        let mut out: Vec<Point> = vec![];
        let mut cur: Option<(i64, Hour)> = None;
        let push = |out: &mut Vec<Point>, b: i64, h: Hour| {
            out.push(Point {
                t: f + (b as f64 * width + width / 2.0) as i64,
                v: h.sum / h.count.max(1) as f64,
                min: Some(h.min),
                max: Some(h.max),
            })
        };
        for h in hours {
            let b = ((h.start.max(f) - f) as f64 / width) as i64;
            match cur.as_mut() {
                Some((cb, acc)) if *cb == b => {
                    acc.count += h.count;
                    acc.sum += h.sum;
                    acc.min = acc.min.min(h.min);
                    acc.max = acc.max.max(h.max);
                    acc.last = h.last;
                }
                _ => {
                    if let Some((cb, acc)) = cur.take() {
                        push(&mut out, cb, acc);
                    }
                    cur = Some((b, h));
                }
            }
        }
        if let Some((cb, acc)) = cur {
            push(&mut out, cb, acc);
        }
        out
    }

    fn downsample(raw: Vec<(i64, f64)>, f: i64, t: i64, max_points: usize) -> Vec<Point> {
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

/// The days that have files in a tag folder.
fn days_in(dir: &Path) -> Vec<NaiveDate> {
    let mut days: Vec<NaiveDate> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(".bin")
                .or_else(|| name.strip_suffix(".gor"))?
                .to_string();
            stem.parse().ok()
        })
        .collect();
    days.sort();
    days.dedup();
    days
}

fn days_between(dir: &Path, f: i64, t: i64) -> Vec<NaiveDate> {
    let day_of = |ms: i64| Utc.timestamp_millis_opt(ms).single().map(|d| d.date_naive());
    let (from_day, to_day) = (day_of(f), day_of(t));
    days_in(dir)
        .into_iter()
        .filter(|d| !from_day.is_some_and(|x| *d < x) && !to_day.is_some_and(|x| *d > x))
        .collect()
}

/// A day's points: the compressed file plus anything appended since. Unsorted.
fn read_day(dir: &Path, day: NaiveDate) -> Vec<(i64, f64)> {
    let mut pts = std::fs::read(dir.join(format!("{day}.gor")))
        .ok()
        .and_then(|b| match tsdb::decode(&b) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::error!("historian: {}/{day}.gor: {e:#}", dir.display());
                None
            }
        })
        .unwrap_or_default();
    if let Ok(buf) = std::fs::read(dir.join(format!("{day}.bin"))) {
        for rec in buf.chunks_exact(16) {
            pts.push((
                i64::from_le_bytes(rec[..8].try_into().unwrap()),
                f64::from_le_bytes(rec[8..].try_into().unwrap()),
            ));
        }
    }
    pts
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

    fn one_tag(retention_days: u32) -> Config {
        Config {
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
                        deadband: 0.0,
                        interval_ms: 1000,
                        retention_days,
                    }),
                    ..Default::default()
                }],
            }],
        }
    }

    fn files(h: &Historian) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(h.tag_dir("s", "temp"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn ms(d: &str) -> i64 {
        DateTime::parse_from_rfc3339(d).unwrap().timestamp_millis()
    }

    #[test]
    fn compaction_rollups_and_late_points() {
        let dir = tempfile::tempdir().unwrap();
        let h = Historian::new(dir.path());
        h.configure(&one_tag(100_000));
        // Two finished days of 10-second samples, plus some of today.
        let d1 = ms("2026-11-01T00:00:00Z");
        let mut pts = vec![];
        for i in 0..(2 * 8640) {
            let v = (20.0 + (i as f64 / 360.0).sin()) as f32 as f64;
            pts.push(("s".to_string(), "temp".to_string(), d1 + i * 10_000, v));
        }
        let now = Utc::now().timestamp_millis();
        pts.push(("s".into(), "temp".into(), now - 1000, 99.0));
        h.insert(pts.clone());
        h.flush();
        let raw_before = h.raw("s", "temp", 0, i64::MAX);
        let bin_bytes: u64 = std::fs::read_dir(h.tag_dir("s", "temp")).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum();

        h.maintain();
        let today = Utc::now().date_naive();
        assert_eq!(
            files(&h),
            vec!["2026-11-01.gor", "2026-11-01.hour", "2026-11-02.gor", "2026-11-02.hour", &format!("{today}.bin")]
        );
        // Same points after compaction, in a fraction of the space.
        assert_eq!(h.raw("s", "temp", 0, i64::MAX), raw_before);
        let gor_bytes: u64 = ["2026-11-01.gor", "2026-11-02.gor"]
            .iter()
            .map(|f| std::fs::metadata(h.tag_dir("s", "temp").join(f)).unwrap().len())
            .sum();
        assert!(gor_bytes * 3 < bin_bytes, "{bin_bytes} → {gor_bytes} bytes");

        // A long query comes from rollups and agrees with the points.
        let (f, t) = (d1, d1 + 2 * 86_400_000 - 1);
        let q = h.query("s", "temp", Utc.timestamp_millis_opt(f).unwrap(), Utc.timestamp_millis_opt(t).unwrap(), 24);
        assert_eq!(q.len(), 24); // 2-hour buckets
        let first_bucket: Vec<f64> = raw_before.iter().filter(|p| p.0 < d1 + 2 * HOUR_MS).map(|p| p.1).collect();
        let avg = first_bucket.iter().sum::<f64>() / first_bucket.len() as f64;
        assert!((q[0].v - avg).abs() < 1e-9);
        assert_eq!(q[0].min, first_bucket.iter().copied().reduce(f64::min));
        assert_eq!(q[0].max, first_bucket.iter().copied().reduce(f64::max));

        // A late point (an agent's backfill) for a compacted day is read at once and merged later.
        let late = d1 + 3600 * 1000 + 5;
        h.insert(vec![("s".into(), "temp".into(), late, -7.0)]);
        assert!(h.raw("s", "temp", late, late).contains(&(late, -7.0)));
        assert_eq!(h.hours("s", "temp", late, late)[0].min, -7.0);
        h.maintain();
        assert!(!files(&h).contains(&"2026-11-01.bin".to_string()));
        assert!(h.raw("s", "temp", late, late).contains(&(late, -7.0)));
        assert_eq!(h.hours("s", "temp", late, late)[0].min, -7.0);
    }

    #[test]
    fn retention_removes_whole_days() {
        let dir = tempfile::tempdir().unwrap();
        let h = Historian::new(dir.path());
        h.configure(&one_tag(30));
        let old = ms("2020-01-01T12:00:00Z");
        h.insert(vec![("s".into(), "temp".into(), old, 1.0)]);
        h.maintain();
        assert!(files(&h).is_empty());
    }

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
