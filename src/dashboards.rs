//! Shared dashboards: a 12-column grid of widgets (trend, value, gauge, lamp, alarm list),
//! saved in `dashboards.json` on the hub.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::valid_id;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WidgetType {
    Trend,
    Value,
    Gauge,
    Lamp,
    Alarms,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Series {
    pub service: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Widget {
    #[serde(rename = "type")]
    pub kind: WidgetType,
    #[serde(default)]
    pub title: String,
    /// Columns out of 12: 3, 4, 6, 8 or 12.
    #[serde(default = "default_width")]
    pub width: u8,
    /// Rows of 150 px: 1 to 4.
    #[serde(default = "one")]
    pub height: u8,
    #[serde(default)]
    pub series: Vec<Series>,
    /// Trend: the time range shown (`15m`, `1h`, `24h`, `7d`, `30d`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    /// Gauge: the scale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Lamp: show a button so operators can switch it.
    #[serde(default)]
    pub switch: bool,
    /// Alarms: only this service's alarms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Room for widget options the UI adds later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

fn default_width() -> u8 {
    6
}
fn one() -> u8 {
    1
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Dashboard {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub widgets: Vec<Widget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
}

impl Dashboard {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err("dashboard id: use 1-64 of a-z, 0-9, - and _".into());
        }
        if self.widgets.len() > 100 {
            return Err("a dashboard holds at most 100 widgets".into());
        }
        for (i, w) in self.widgets.iter().enumerate() {
            let n = i + 1;
            if !(1..=12).contains(&w.width) {
                return Err(format!("widget {n}: width is 1 to 12 columns"));
            }
            if !(1..=4).contains(&w.height) {
                return Err(format!("widget {n}: height is 1 to 4 rows"));
            }
            let need = match w.kind {
                WidgetType::Trend => (1, 8),
                WidgetType::Value | WidgetType::Gauge | WidgetType::Lamp => (1, 1),
                WidgetType::Alarms => (0, 0),
            };
            if w.series.len() < need.0 || w.series.len() > need.1 {
                return Err(format!(
                    "widget {n} ({:?}): needs {} tag{}",
                    w.kind,
                    if need.0 == need.1 { need.0.to_string() } else { format!("{} to {}", need.0, need.1) },
                    if need.1 == 1 { "" } else { "s" }
                ));
            }
        }
        Ok(())
    }
}

pub struct Dashboards {
    path: PathBuf,
    list: Mutex<Vec<Dashboard>>,
}

impl Dashboards {
    pub fn open(path: &Path) -> Result<Self> {
        let list = match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t)?,
            Err(_) => vec![],
        };
        Ok(Self { path: path.to_path_buf(), list: Mutex::new(list) })
    }

    pub fn list(&self) -> Vec<Dashboard> {
        self.list.lock().clone()
    }

    pub fn get(&self, id: &str) -> Option<Dashboard> {
        self.list.lock().iter().find(|d| d.id == id).cloned()
    }

    fn save(&self, list: &[Dashboard]) -> Result<(), String> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(list).unwrap_or_default())
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(|e| e.to_string())
    }

    /// Create (`replace` None) or replace a dashboard.
    pub fn put(&self, replace: Option<&str>, mut d: Dashboard, user: &str) -> Result<Dashboard, String> {
        d.validate()?;
        d.updated = Some(Utc::now());
        d.updated_by = Some(user.to_string());
        let mut list = self.list.lock();
        let target = replace.unwrap_or(&d.id).to_string();
        if d.id != target || replace.is_none() {
            if list.iter().any(|x| x.id == d.id) {
                return Err(format!("a dashboard with id `{}` already exists", d.id));
            }
        }
        match list.iter_mut().find(|x| x.id == target) {
            Some(slot) if replace.is_some() => *slot = d.clone(),
            None if replace.is_none() => list.push(d.clone()),
            _ => return Err("no such dashboard".into()),
        }
        self.save(&list)?;
        Ok(d)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut list = self.list.lock();
        let before = list.len();
        list.retain(|d| d.id != id);
        if list.len() == before {
            return Err("no such dashboard".into());
        }
        self.save(&list)
    }
}
