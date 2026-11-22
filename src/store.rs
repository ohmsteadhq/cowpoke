//! `cowpoke.yaml`: the service configuration, saved whenever the UI or API changes it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::Mutex;

use crate::model::{Config, ServiceDef};

pub struct ConfigStore {
    path: PathBuf,
    cfg: Mutex<Config>,
}

impl ConfigStore {
    pub fn open(path: &Path) -> Result<Self> {
        let cfg = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            parse(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Config::default()
        };
        Ok(Self {
            path: path.to_path_buf(),
            cfg: Mutex::new(cfg),
        })
    }

    pub fn get(&self) -> Config {
        self.cfg.lock().clone()
    }

    pub fn service(&self, id: &str) -> Option<ServiceDef> {
        self.cfg.lock().services.iter().find(|s| s.id == id).cloned()
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(&*self.cfg.lock()).unwrap_or_default()
    }

    /// Insert or replace a service (matched on `replace_id`, or its own id), then save.
    pub fn upsert(&self, replace_id: Option<&str>, svc: ServiceDef) -> Result<(), String> {
        svc.validate()?;
        let mut cfg = self.cfg.lock();
        let target = replace_id.unwrap_or(&svc.id).to_string();
        if svc.id != target && cfg.services.iter().any(|s| s.id == svc.id) {
            return Err(format!("a service with id `{}` already exists", svc.id));
        }
        match cfg.services.iter_mut().find(|s| s.id == target) {
            Some(slot) => *slot = svc,
            None => cfg.services.push(svc),
        }
        self.save(&cfg).map_err(|e| e.to_string())
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        let mut cfg = self.cfg.lock();
        let before = cfg.services.len();
        cfg.services.retain(|s| s.id != id);
        let removed = cfg.services.len() != before;
        if removed {
            self.save(&cfg)?;
        }
        Ok(removed)
    }

    pub fn replace_all(&self, new: Config) -> Result<(), String> {
        let mut ids = std::collections::HashSet::new();
        for s in &new.services {
            s.validate()?;
            if !ids.insert(&s.id) {
                return Err(format!("service id `{}` appears twice", s.id));
            }
        }
        let mut cfg = self.cfg.lock();
        *cfg = new;
        self.save(&cfg).map_err(|e| e.to_string())
    }

    fn save(&self, cfg: &Config) -> Result<()> {
        let text = format!(
            "# Cowpoke configuration. Edited by the web UI and API; safe to edit by hand while Cowpoke is stopped.\n{}",
            serde_yaml::to_string(cfg)?
        );
        let tmp = self.path.with_extension("yaml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

pub fn parse(text: &str) -> Result<Config> {
    if text.trim().is_empty() {
        return Ok(Config::default());
    }
    Ok(serde_yaml::from_str(text)?)
}
