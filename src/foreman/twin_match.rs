//! Match an Ohmstead machine-twins export to the tags Cowpoke already reads.
//!
//! Ohmstead says what the plant is, by ISA tag: P-301 is a lobe pump on VFD-301, PT-301 is its
//! discharge pressure. Cowpoke has the tags, named however the plant named them (`PT-301`,
//! `Skid1.PT301_PV`, `vfd-301/OutputFreq`). This proposes which tag fills each role of each twin, with
//! how sure it is: *exact*, *likely*, *pick* (several fit) or *none*. Nothing is written until the
//! proposal is applied. Rules: ohmstead `docs/design/pid-twins.md` §2.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use super::twin::{TwinDef, TwinKind, TwinsConfig};
use crate::model::{Config, DataType, Mode};

#[derive(Serialize, Clone, Debug)]
pub struct RoleMatch {
    pub role: String,
    /// What Ohmstead calls it: an instrument's ISA tag, or the drive's tag for drive signals.
    pub wanted: String,
    /// exact | likely | pick | none
    pub status: &'static str,
    /// The chosen `service/tag`, when exact or likely.
    pub tag: Option<String>,
    /// Every tag that fits, best first.
    pub candidates: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct TwinMatch {
    pub name: String,
    pub kind: String,
    pub description: String,
    pub roles: Vec<RoleMatch>,
    /// Nearby measurements found among Cowpoke's tags (`service/tag`): candidates Foreman may train into
    /// the twin's regressions.
    pub inputs: Vec<String>,
    /// Twins the product reaches next, from the P&ID.
    pub downstream: Vec<String>,
    /// Ohmstead's notes, then the matcher's.
    pub notes: Vec<String>,
    /// Every required role is filled.
    pub ready: bool,
}

struct TagInfo {
    key: String,
    service: String,
    name: String,
    units: Option<String>,
    client: bool,
    numeric: bool,
}

/// Lowercase letters and digits only: `PT-301` and `pt_301` are `pt301`.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Tokens of a name, split at punctuation and at letter↔digit changes: `Skid1.PT301_PV` → skid,1,pt,301,pv.
fn tokens(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut last: Option<bool> = None; // digit?
    for c in s.chars() {
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            last = None;
            continue;
        }
        let d = c.is_ascii_digit();
        if last.is_some_and(|l| l != d) && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c.to_ascii_lowercase());
        last = Some(d);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Where `want` (normalized) appears as whole consecutive tokens of `name`: the token range.
fn find_tokens(name: &str, want: &str) -> Option<(usize, usize)> {
    let t = tokens(name);
    for i in 0..t.len() {
        let mut acc = String::new();
        for (j, tok) in t.iter().enumerate().skip(i) {
            acc.push_str(tok);
            if acc == want {
                return Some((i, j + 1));
            }
            if acc.len() >= want.len() {
                break;
            }
        }
    }
    None
}

const RECIPE_WORDS: &[&str] = &["recipe", "recipeno", "recipenum", "product", "productcode", "sku", "grade", "formula"];
const VALUE_WORDS: &[&str] = &["pv", "value", "val", "ao", "ai", "out", "meas", "in"];
const NOT_VALUE: &[&str] = &["sp", "setpoint", "alarm", "alm", "hi", "lo", "hh", "ll", "status", "fault", "flt", "mode", "cmd", "raw", "min", "max"];

fn role_words(role: &str) -> (&'static [&'static str], &'static [&'static str]) {
    // (name words, units)
    match role {
        "freq" => (&["freq", "frequency", "hz", "speed", "outputfreq", "outfreq"], &["hz"]),
        "current" => (&["current", "amps", "amp", "curr", "outputcurrent", "outcurrent"], &["a", "amps"]),
        "out_voltage" => (&["outvolt", "outvolts", "outputvoltage", "outputvolts", "voltage", "volts", "outv"], &["v"]),
        "dc_bus" => (&["bus", "dcbus", "dcbusvolts", "dcv", "busvolts"], &["v", "vdc"]),
        _ => (&[], &[]),
    }
}

/// Match one export (`ohmstead-twins` 1) against the services Cowpoke has.
pub fn propose(export: &Value, cfg: &Config) -> Result<Vec<TwinMatch>, String> {
    if export["format"] != "ohmstead-twins" {
        return Err("not an Ohmstead twins export (format ohmstead-twins)".into());
    }
    let mut tags: Vec<TagInfo> = vec![];
    for s in &cfg.services {
        for t in &s.tags {
            tags.push(TagInfo {
                key: format!("{}/{}", s.id, t.name),
                service: s.id.clone(),
                name: t.name.clone(),
                units: t.units.clone().map(|u| u.trim().to_lowercase()),
                client: s.mode == Mode::Client,
                numeric: !matches!(t.datatype, DataType::Bool | DataType::String),
            });
        }
    }
    let rank = |t: &TagInfo| (if t.client { 0 } else { 1 }, if t.numeric { 0 } else { 1 });

    let instrument = |want: &str| -> (Vec<(i32, String)>, bool) {
        let w = norm(want);
        // (score, rank, key): higher score first.
        let mut c: Vec<(i32, (i32, i32), String)> = vec![];
        let mut exact = false;
        for t in tags.iter().filter(|t| t.numeric) {
            let mut score = if norm(&t.name) == w {
                exact = true;
                10
            } else if let Some((a, b)) = find_tokens(&t.name, &w) {
                let toks = tokens(&t.name);
                let rest: Vec<&String> = toks[..a].iter().chain(toks[b..].iter()).collect();
                if rest.iter().any(|r| NOT_VALUE.contains(&r.as_str())) {
                    continue;
                }
                6 - rest.iter().filter(|r| !VALUE_WORDS.contains(&r.as_str()) && r.parse::<u32>().is_err()).count() as i32
            } else if find_tokens(&t.service, &w).is_some() {
                4
            } else {
                continue;
            };
            // A 4-wire transmitter's negative terminal (`FT-301.AO-`) is not the value.
            if t.name.trim_end().ends_with('-') {
                score -= 5;
            }
            c.push((score, rank(t), t.key.clone()));
        }
        c.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let top = c.first().map(|x| (x.0, x.1));
        let n_top = c.iter().filter(|x| Some((x.0, x.1)) == top).count();
        (c.into_iter().map(|x| (x.0, x.2)).collect(), exact && n_top == 1)
    };
    let drive_signal = |drive: &str, role: &str| -> Vec<(i32, String)> {
        let d = norm(drive);
        let (words, units) = role_words(role);
        let mut c: Vec<(i32, (i32, i32), String)> = vec![];
        for t in tags.iter().filter(|t| t.numeric) {
            let in_name = find_tokens(&t.name, &d);
            let in_service = find_tokens(&t.service, &d).is_some();
            if in_name.is_none() && !in_service {
                continue;
            }
            let rest: String = match in_name {
                Some((a, b)) => {
                    let toks = tokens(&t.name);
                    toks[..a].iter().chain(toks[b..].iter()).cloned().collect::<Vec<_>>().join("")
                }
                None => norm(&t.name),
            };
            let mut score = 0;
            if words.iter().any(|w| rest.contains(w)) {
                score += 3;
            }
            // "voltage" alone is the output voltage, not the bus.
            if role == "out_voltage" && (rest.contains("bus") || rest.contains("dc")) {
                continue;
            }
            if role == "freq" && (rest.contains("cmd") || rest.contains("ref") || rest.contains("sp")) {
                score -= 2;
            }
            match &t.units {
                Some(u) if units.contains(&u.as_str()) => score += 2,
                Some(u) if !u.is_empty() && !units.is_empty() => continue,
                _ => {}
            }
            if score >= 3 {
                c.push((score, rank(t), t.key.clone()));
            }
        }
        c.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        c.into_iter().map(|x| (x.0, x.2)).collect()
    };

    // The recipe or product running: one plant-wide tag (the line's recipe number). Ohmstead doesn't
    // draw it, so it's found by name; Foreman learns which machines it matters to.
    let recipe: Vec<String> = {
        let mut c: Vec<((i32, i32), String)> = tags
            .iter()
            .filter(|t| t.numeric && tokens(&t.name).iter().any(|w| RECIPE_WORDS.contains(&w.as_str())))
            .filter(|t| !tokens(&t.name).iter().any(|w| NOT_VALUE.contains(&w.as_str()) || w == "name" || w == "req"))
            .map(|t| (rank(t), t.key.clone()))
            .collect();
        c.sort();
        c.into_iter().map(|x| x.1).collect()
    };

    let mut out = vec![];
    for tw in export["twins"].as_array().cloned().unwrap_or_default() {
        let name = tw["name"].as_str().unwrap_or("").to_string();
        let kind = tw["kind"].as_str().unwrap_or("drive_load").to_string();
        let drive = tw["drive"].as_str().unwrap_or("");
        let mut notes: Vec<String> = tw["notes"].as_array().map(|n| n.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let mut roles = vec![];
        for role in ["freq", "current", "out_voltage", "dc_bus"] {
            let c = drive_signal(drive, role);
            let status = match c.as_slice() {
                [] => "none",
                [(_, _)] => "likely",
                [(a, _), (b, _), ..] if a > b => "likely",
                _ => "pick",
            };
            let tag = (status == "likely").then(|| c[0].1.clone());
            roles.push(RoleMatch { role: role.into(), wanted: drive.into(), status, tag, candidates: c.into_iter().map(|x| x.1).collect() });
        }
        if let Some(ins) = tw["instruments"].as_object() {
            for role in ["discharge", "suction", "flow", "level", "temperature"] {
                let Some(want) = ins.get(role).and_then(|v| v.as_str()) else { continue };
                let (c, exact) = instrument(want);
                // A clear winner on score is likely; a tie is the user's pick.
                let status = match (exact, c.as_slice()) {
                    (true, _) => "exact",
                    (_, []) => "none",
                    (_, [_]) => "likely",
                    (_, [(a, _), (b, _), ..]) if a > b => "likely",
                    _ => "pick",
                };
                let tag = matches!(status, "exact" | "likely").then(|| c[0].1.clone());
                roles.push(RoleMatch { role: role.into(), wanted: want.into(), status, tag, candidates: c.into_iter().map(|x| x.1).collect() });
            }
        }
        if !recipe.is_empty() {
            let status = if recipe.len() == 1 { "likely" } else { "pick" };
            let tag = (status == "likely").then(|| recipe[0].clone());
            roles.push(RoleMatch { role: "recipe".into(), wanted: "recipe".into(), status, tag, candidates: recipe.clone() });
        }
        let mut inputs = vec![];
        let mut missing = vec![];
        for want in tw["nearby"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(String::from)) {
            let (c, exact) = instrument(&want);
            match (exact, c.as_slice()) {
                (true, _) | (_, [_]) => inputs.push(c[0].1.clone()),
                (_, [(a, _), (b, _), ..]) if a > b => inputs.push(c[0].1.clone()),
                _ => missing.push(want),
            }
        }
        if !missing.is_empty() {
            notes.push(format!("Nearby {} not found (or not one clear tag) among Cowpoke's tags; Foreman won't try them as inputs.", missing.join(", ")));
        }
        let has = |r: &str| roles.iter().any(|x| x.role == r && x.tag.is_some());
        let ready = has("freq") && has("current");
        if ready && kind != "drive_load" && !has("discharge") {
            notes.push("No tag found for the discharge pressure yet: until one is picked this twin watches the drive's load only.".into());
        }
        out.push(TwinMatch { name, kind, description: tw["description"].as_str().unwrap_or("").into(), roles, inputs, downstream: tw["downstream"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default(), notes, ready });
    }
    Ok(out)
}

/// Turn a proposal (plus the user's picks: twin → role → `service/tag`) into twin definitions, merged
/// into `current`: twins from this project are replaced by name, keeping their tuning; twins written by
/// hand are left alone. Twins missing a required role are left out and listed.
pub fn apply(
    current: &TwinsConfig,
    proposal: &[TwinMatch],
    picks: &BTreeMap<String, BTreeMap<String, String>>,
    project: Option<&str>,
) -> (TwinsConfig, Vec<String>) {
    let mut cfg = current.clone();
    let mut left_out = vec![];
    for m in proposal {
        let chosen = |role: &str| -> Option<String> {
            picks.get(&m.name).and_then(|p| p.get(role).cloned()).or_else(|| m.roles.iter().find(|r| r.role == role).and_then(|r| r.tag.clone()))
        };
        let (Some(freq), Some(current_tag)) = (chosen("freq"), chosen("current")) else {
            left_out.push(format!("{}: no tag for the drive's {}", m.name, if chosen("freq").is_none() { "frequency" } else { "current" }));
            continue;
        };
        let discharge = chosen("discharge");
        let kind = match (m.kind.as_str(), discharge.is_some()) {
            ("pump_vfd", true) => TwinKind::PumpVfd,
            ("pump_pd_vfd", true) => TwinKind::PumpPdVfd,
            _ => TwinKind::DriveLoad,
        };
        let existing = cfg.twins.iter().position(|t| t.name == m.name);
        if let Some(i) = existing {
            if cfg.twins[i].source.as_deref() != Some("ohmstead") {
                left_out.push(format!("{}: a twin with that name was written by hand; left as it is", m.name));
                continue;
            }
        }
        let base: TwinDef = existing.map(|i| cfg.twins[i].clone()).unwrap_or_else(|| {
            serde_yaml::from_str("{name: x, freq: a/b, current: a/b, kind: drive_load}").expect("default twin")
        });
        let def = TwinDef {
            name: m.name.clone(),
            description: m.description.clone(),
            kind,
            freq,
            current: current_tag,
            discharge: if kind == TwinKind::DriveLoad { None } else { discharge },
            suction: if kind == TwinKind::DriveLoad { None } else { chosen("suction") },
            flow: chosen("flow"),
            level: chosen("level"),
            temperature: chosen("temperature"),
            out_voltage: chosen("out_voltage"),
            dc_bus: chosen("dc_bus"),
            recipe: chosen("recipe"),
            inputs: m.inputs.clone(),
            downstream: m.downstream.clone(),
            source: Some("ohmstead".into()),
            project: project.map(String::from),
            ..base
        };
        match existing {
            Some(i) => cfg.twins[i] = def,
            None => cfg.twins.push(def),
        }
    }
    (cfg, left_out)
}

/// The proposal as JSON for the API and the command line.
pub fn to_json(p: &[TwinMatch]) -> Value {
    let count = |s: &str| p.iter().flat_map(|m| &m.roles).filter(|r| r.status == s).count();
    json!({
        "twins": p,
        "summary": {"twins": p.len(), "ready": p.iter().filter(|m| m.ready).count(),
                    "exact": count("exact"), "likely": count("likely"), "pick": count("pick"), "none": count("none")},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_split_names_the_way_plants_write_them() {
        assert_eq!(tokens("Skid1.PT301_PV"), vec!["skid", "1", "pt", "301", "pv"]);
        assert_eq!(find_tokens("Skid1.PT301_PV", "pt301"), Some((2, 4)));
        assert_eq!(find_tokens("PT-3010", "pt301"), None, "a longer number isn't the same instrument");
        assert_eq!(find_tokens("VFD_301_OutFreq", "vfd301"), Some((0, 2)));
    }

    #[test]
    fn matches_real_world_names() {
        let cfg: Config = serde_yaml::from_str(
            r#"
services:
  - id: cp1-plc
    kind: eip
    mode: client
    tags:
      - { name: PT-301, datatype: float32, units: bar }
      - { name: FT-301.AO+, datatype: float32 }
      - { name: FT-301.AO-, datatype: float32 }
      - { name: PT301_HiAlarm, datatype: bool }
      - { name: VFD_301_OutFreq, datatype: float32, units: Hz }
      - { name: VFD_301_FreqCmd, datatype: float32, units: Hz }
      - { name: VFD_301_OutCurrent, datatype: float32, units: A }
      - { name: VFD_301_DCBusVolts, datatype: float32, units: V }
      - { name: VFD_301_OutVolts, datatype: float32, units: V }
      - { name: Line_RecipeNo, datatype: int16 }
      - { name: Line_RecipeName, datatype: string }
  - id: vfd-401
    kind: modbus
    mode: client
    tags:
      - { name: OutputFreq, datatype: float32, units: Hz }
      - { name: OutputCurrent, datatype: float32, units: A }
      - { name: DCBusVoltage, datatype: float32, units: V }
      - { name: OutputVoltage, datatype: float32, units: V }
"#,
        )
        .unwrap();
        let export = json!({"format": "ohmstead-twins", "version": 1, "twins": [
            {"name": "P-301", "kind": "pump_pd_vfd", "drive": "VFD-301", "motor": "M-301", "description": "timing pump",
             "instruments": {"discharge": "PT-301", "flow": "FT-301"}, "notes": []},
            {"name": "AE-401", "kind": "drive_load", "drive": "VFD-401", "motor": "M-401", "description": "aerator", "instruments": {}, "notes": []},
        ]});
        let p = propose(&export, &cfg).unwrap();
        let role = |t: usize, r: &str| p[t].roles.iter().find(|x| x.role == r).unwrap().clone();
        assert_eq!(role(0, "discharge").status, "exact");
        assert_eq!(role(0, "discharge").tag.as_deref(), Some("cp1-plc/PT-301"));
        assert_eq!(role(0, "flow").tag.as_deref(), Some("cp1-plc/FT-301.AO+"), "{:?}", role(0, "flow"));
        assert_eq!(role(0, "freq").tag.as_deref(), Some("cp1-plc/VFD_301_OutFreq"), "{:?}", role(0, "freq"));
        assert_eq!(role(0, "current").tag.as_deref(), Some("cp1-plc/VFD_301_OutCurrent"));
        assert_eq!(role(0, "dc_bus").tag.as_deref(), Some("cp1-plc/VFD_301_DCBusVolts"));
        assert_eq!(role(0, "out_voltage").tag.as_deref(), Some("cp1-plc/VFD_301_OutVolts"));
        assert_eq!(role(1, "freq").tag.as_deref(), Some("vfd-401/OutputFreq"), "drive found by its service id");
        assert_eq!(role(1, "dc_bus").tag.as_deref(), Some("vfd-401/DCBusVoltage"));
        assert_eq!(role(1, "out_voltage").tag.as_deref(), Some("vfd-401/OutputVoltage"));
        assert!(p.iter().all(|m| m.ready));
        assert_eq!(role(1, "recipe").tag.as_deref(), Some("cp1-plc/Line_RecipeNo"), "the number, not the name");

        let (cfg2, left) = apply(&TwinsConfig::default(), &p, &BTreeMap::new(), Some("proj"));
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(cfg2.twins.len(), 2);
        assert_eq!(cfg2.twins[0].kind, TwinKind::PumpPdVfd);
        assert_eq!(cfg2.twins[0].discharge.as_deref(), Some("cp1-plc/PT-301"));
        assert_eq!(cfg2.twins[0].recipe.as_deref(), Some("cp1-plc/Line_RecipeNo"));
        assert_eq!(cfg2.twins[1].kind, TwinKind::DriveLoad);
        cfg2.validate().unwrap();
        // Re-applying keeps a hand-written twin of the same name alone.
        let mut hand = cfg2.clone();
        hand.twins[1].source = None;
        let (cfg3, left) = apply(&hand, &p, &BTreeMap::new(), Some("proj"));
        assert_eq!(cfg3.twins[1].source, None);
        assert_eq!(left.len(), 1);
    }
}
