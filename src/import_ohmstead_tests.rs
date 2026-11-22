//! Import from Ohmstead: the mapping rules per service kind, re-import by name, and the formats it reads.

use serde_json::json;

use crate::import_ohmstead::{Options, apply, from_value, parse};
use crate::model::{DataType, ModbusTable, ProcessData, ServiceDef};

/// The Pump and Tank built-in's I/O, as Ohmstead's export_io_points returns it (trimmed).
fn pump_and_tank() -> serde_json::Value {
    json!({
        "format": "ohmstead-io", "version": 1,
        "project": { "id": "p1", "title": "Pump and Tank" },
        "points": [
            { "name": "LT-101", "device": "LT-101", "device_terminal": "-", "description": "Level Transmitter 4-20mA",
              "module": "AI1", "terminal": "CH0", "kind": "analog", "direction": "in",
              "range": { "min": 0, "max": 100, "units": "%" }, "subsystem": "Field" },
            { "name": "FT-101", "device": "FT-101", "device_terminal": "-", "module": "AI1", "terminal": "CH1", "kind": "analog", "direction": "in" },
            { "name": "LSH-101", "device": "LSH-101", "device_terminal": "2", "module": "DI1", "terminal": "I0", "kind": "digital", "direction": "in" },
            { "name": "K1.14", "device": "K1", "device_terminal": "14", "module": "DI1", "terminal": "I1", "kind": "digital", "direction": "in" },
            { "name": "K1.A1", "device": "K1", "device_terminal": "A1", "module": "DO1", "terminal": "O0", "kind": "digital", "direction": "out" },
            { "name": "SV-101", "device": "SV-101", "device_terminal": "A1", "module": "DO1", "terminal": "O1", "kind": "digital", "direction": "out" }
        ]
    })
}

fn svc(yaml: &str) -> ServiceDef {
    serde_yaml::from_str(yaml).unwrap()
}

#[test]
fn modbus_tables_and_addresses() {
    let mut def = svc("{id: sim, kind: modbus, mode: server, settings: {port: 5020}}");
    let ex = from_value(pump_and_tank()).unwrap();
    let pv = apply(&mut def, &ex, &Options { start: Some(100), ..Default::default() });
    def.validate().unwrap();
    assert_eq!(pv.added.len(), 6);
    let t = |n: &str| def.tag(n).unwrap().clone();
    assert_eq!((t("LT-101").table(), t("LT-101").register, t("LT-101").datatype), (ModbusTable::InputRegister, Some(100), DataType::Float32));
    assert_eq!(t("FT-101").register, Some(102), "float32 takes two registers");
    assert_eq!((t("LSH-101").table(), t("LSH-101").register), (ModbusTable::DiscreteInput, Some(100)));
    assert_eq!(t("K1.14").register, Some(101));
    assert_eq!((t("K1.A1").table(), t("K1.A1").register), (ModbusTable::Coil, Some(100)));
    assert_eq!(t("LT-101").units.as_deref(), Some("%"));
    assert!(t("LT-101").description.unwrap().contains("LT-101 - → AI1:CH0"));
    assert!(t("LSH-101").writable, "server mode: inputs are writable (the simulator sets them)");
}

#[test]
fn reimport_updates_by_name_and_keeps_addresses() {
    let mut def = svc("{id: plc, kind: modbus, mode: client, settings: {host: 10.0.0.5}}");
    let ex = from_value(pump_and_tank()).unwrap();
    apply(&mut def, &ex, &Options::default());
    // The user moves a tag, adds their own, and sets a scale.
    let lt = def.tags.iter_mut().find(|t| t.name == "LT-101").unwrap();
    lt.register = Some(40);
    lt.scale = Some(0.5);
    def.tags.push(svc("{id: x, kind: modbus, mode: client, tags: [{name: mine, register: 7}]}").tags.remove(0));

    // Ohmstead gains a point and loses none.
    let mut v = pump_and_tank();
    v["points"].as_array_mut().unwrap().push(json!({ "name": "PT-102", "device": "PT-102", "module": "AI1", "terminal": "CH2", "kind": "analog", "direction": "in" }));
    let pv = apply(&mut def, &from_value(v).unwrap(), &Options::default());
    def.validate().unwrap();
    assert_eq!(pv.added, vec!["PT-102"]);
    assert!(pv.updated.is_empty(), "{:?}", pv.updated);
    assert_eq!(pv.unchanged.len(), 6);
    assert_eq!(pv.not_in_import, vec!["mine"]);
    assert_eq!(def.tags.len(), 8, "no duplicates");
    let lt = def.tag("LT-101").unwrap();
    assert_eq!((lt.register, lt.scale), (Some(40), Some(0.5)), "kept the user's address and scale");
    assert_eq!(def.tag("PT-102").unwrap().register, Some(42), "next free after the highest in use");
    assert!(!def.tag("LSH-101").unwrap().writable, "client mode: inputs are read-only");
    assert!(def.tag("SV-101").unwrap().writable);
}

#[test]
fn opcua_node_ids() {
    let mut def = svc("{id: ua, kind: opcua, mode: client, settings: {endpoint: 'opc.tcp://plc:4840'}}");
    apply(&mut def, &from_value(pump_and_tank()).unwrap(), &Options { ns: Some(3), prefix: Some("Tank.".into()), ..Default::default() });
    def.validate().unwrap();
    assert_eq!(def.tag("K1.A1").unwrap().node_id.as_deref(), Some("ns=3;s=Tank.K1.A1"));
    assert_eq!(def.tag("LT-101").unwrap().datatype, DataType::Float32);

    let mut server = svc("{id: uas, kind: opcua, mode: server, settings: {port: 4840}}");
    apply(&mut server, &from_value(pump_and_tank()).unwrap(), &Options::default());
    assert!(server.tags.iter().all(|t| t.node_id.is_none()), "server tags use Cowpoke's default node ids");
}

#[test]
fn iolink_one_port_per_device() {
    let mut def = svc("{id: iol, kind: iolink, mode: server, settings: {port: 8081, ports: 4}}");
    let pv = apply(&mut def, &from_value(pump_and_tank()).unwrap(), &Options::default());
    def.validate().unwrap();
    let t = |n: &str| def.tag(n).unwrap().clone();
    assert_eq!(t("LT-101").port, Some(1));
    assert_eq!(t("LT-101").datatype, DataType::Uint16);
    assert_eq!(t("FT-101").port, Some(2));
    assert_eq!(t("LSH-101").port, Some(3));
    assert_eq!(t("K1.14").port, Some(4));
    assert_eq!((t("K1.A1").port, t("K1.A1").pd(), t("K1.A1").bit_offset), (Some(4), ProcessData::Out, Some(0)), "same device, same port");
    assert_eq!(t("K1.14").bit_offset, Some(0), "in and out process data count separately");
    assert_eq!(pv.skipped.len(), 1, "SV-101 needs a fifth port: {:?}", pv.skipped);
    assert_eq!(pv.skipped[0].name, "SV-101");
}

#[test]
fn reads_text_arrays_and_mcp_replies() {
    let text = pump_and_tank().to_string();
    assert_eq!(parse(&text).unwrap().points.len(), 6);
    assert_eq!(parse(&pump_and_tank()["points"].to_string()).unwrap().points.len(), 6, "a bare array");
    let reply = json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": text }] } });
    assert_eq!(from_value(reply).unwrap().points.len(), 6, "the MCP tools/call reply");
    let err = json!({ "result": { "isError": true, "content": [{ "type": "text", "text": "No project with id x" }] } });
    assert!(from_value(err).unwrap_err().contains("No project"));
    assert!(parse(r#"{"format":"other","points":[]}"#).is_err());
    assert!(parse(r#"{"format":"ohmstead-io","version":2,"points":[]}"#).is_err());
}

#[test]
fn other_kinds_get_plain_tags() {
    let mut def = svc("{id: api, kind: api, mode: server, settings: {routes: []}}");
    let pv = apply(&mut def, &from_value(pump_and_tank()).unwrap(), &Options::default());
    assert_eq!(pv.added.len(), 6);
    assert_eq!(def.tag("SV-101").unwrap().datatype, DataType::Bool);
    assert!(def.tag("SV-101").unwrap().register.is_none());
}
