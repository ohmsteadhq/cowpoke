//! Turning tag values into Modbus registers and IO-Link process data bits, and back.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{DataType, TagDef};
use crate::tags::as_f64;

/// How multi-register values are ordered. Bytes within a register are always big-endian.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WordOrder {
    /// Most significant register first (ABCD). The Modbus spec's order.
    #[default]
    Big,
    /// Least significant register first (CDAB), common on many PLCs.
    Little,
}

fn raw_number(t: &TagDef, v: &Value) -> Result<f64, String> {
    let eng = as_f64(v)
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .ok_or_else(|| format!("tag `{}`: `{v}` is not a number", t.name))?;
    Ok(t.to_raw(eng))
}

fn int_bytes(dt: DataType, raw: f64) -> Vec<u8> {
    let r = raw.round();
    match dt {
        DataType::Int16 => (r.clamp(i16::MIN as f64, i16::MAX as f64) as i16).to_be_bytes().to_vec(),
        DataType::Uint16 => (r.clamp(0.0, u16::MAX as f64) as u16).to_be_bytes().to_vec(),
        DataType::Int32 => (r.clamp(i32::MIN as f64, i32::MAX as f64) as i32).to_be_bytes().to_vec(),
        DataType::Uint32 => (r.clamp(0.0, u32::MAX as f64) as u32).to_be_bytes().to_vec(),
        DataType::Int64 => (r as i64).to_be_bytes().to_vec(),
        DataType::Uint64 => (r.max(0.0) as u64).to_be_bytes().to_vec(),
        DataType::Float32 => (raw as f32).to_be_bytes().to_vec(),
        DataType::Float64 => raw.to_be_bytes().to_vec(),
        _ => unreachable!(),
    }
}

fn bytes_to_words(b: &[u8], order: WordOrder) -> Vec<u16> {
    let mut w: Vec<u16> = b
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
        .collect();
    if order == WordOrder::Little {
        w.reverse();
    }
    w
}

fn words_to_bytes(w: &[u16], order: WordOrder) -> Vec<u8> {
    let mut w = w.to_vec();
    if order == WordOrder::Little {
        w.reverse();
    }
    w.iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// Encode an engineering value into the tag's registers.
pub fn encode_registers(t: &TagDef, v: &Value, order: WordOrder) -> Result<Vec<u16>, String> {
    match t.datatype {
        DataType::Bool => Ok(vec![u16::from(truthy(v))]),
        DataType::String => {
            let len = t.datatype.registers(t.length) as usize;
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let mut b: Vec<u8> = s.bytes().take(len * 2).collect();
            b.resize(len * 2, 0);
            // Strings are always stored first character first.
            Ok(bytes_to_words(&b, WordOrder::Big))
        }
        dt => Ok(bytes_to_words(&int_bytes(dt, raw_number(t, v)?), order)),
    }
}

/// Decode the tag's registers into an engineering value.
pub fn decode_registers(t: &TagDef, w: &[u16], order: WordOrder) -> Value {
    let dt = t.datatype;
    if dt == DataType::Bool {
        return Value::Bool(w.first().copied().unwrap_or(0) != 0);
    }
    if dt == DataType::String {
        let b = words_to_bytes(w, WordOrder::Big);
        let s: String = b
            .into_iter()
            .take_while(|c| *c != 0)
            .map(char::from)
            .collect();
        return Value::String(s);
    }
    let b = words_to_bytes(w, order);
    let raw: f64 = match dt {
        DataType::Int16 => i16::from_be_bytes([b[0], b[1]]) as f64,
        DataType::Uint16 => u16::from_be_bytes([b[0], b[1]]) as f64,
        DataType::Int32 => i32::from_be_bytes(b[..4].try_into().unwrap()) as f64,
        DataType::Uint32 => u32::from_be_bytes(b[..4].try_into().unwrap()) as f64,
        DataType::Int64 => i64::from_be_bytes(b[..8].try_into().unwrap()) as f64,
        DataType::Uint64 => u64::from_be_bytes(b[..8].try_into().unwrap()) as f64,
        DataType::Float32 => f32::from_be_bytes(b[..4].try_into().unwrap()) as f64,
        DataType::Float64 => f64::from_be_bytes(b[..8].try_into().unwrap()),
        _ => unreachable!(),
    };
    number_value(t, raw)
}

/// A raw number as the tag's JSON value: scaled tags and floats become floats, others integers.
pub fn number_value(t: &TagDef, raw: f64) -> Value {
    let eng = t.to_eng(raw);
    if t.is_scaled() || matches!(t.datatype, DataType::Float32 | DataType::Float64) {
        if t.datatype == DataType::Float32 && !t.is_scaled() {
            // Keep f32 values readable (0.1 instead of 0.10000000149011612).
            return json!((eng as f32).to_string().parse::<f64>().unwrap_or(eng));
        }
        json!(eng)
    } else if matches!(t.datatype, DataType::Uint16 | DataType::Uint32 | DataType::Uint64) {
        json!(eng as u64)
    } else {
        json!(eng as i64)
    }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "on" | "yes"),
        _ => false,
    }
}

// ---------------------------------------------------------------- IO-Link

/// Bit `i` counted IODD-style: bit 0 is the least significant bit of the last byte.
fn bit(pd: &[u8], i: usize) -> bool {
    let n = pd.len();
    if i >= n * 8 {
        return false;
    }
    let byte = pd[n - 1 - i / 8];
    byte >> (i % 8) & 1 == 1
}

fn set_bit(pd: &mut [u8], i: usize, on: bool) {
    let n = pd.len();
    if i >= n * 8 {
        return;
    }
    let b = &mut pd[n - 1 - i / 8];
    if on {
        *b |= 1 << (i % 8);
    } else {
        *b &= !(1 << (i % 8));
    }
}

fn pd_field(t: &TagDef) -> (usize, usize) {
    let len = t.bit_length.unwrap_or_else(|| t.datatype.bits()) as usize;
    (t.bit_offset.unwrap_or(0) as usize, len)
}

pub fn decode_pd(t: &TagDef, pd: &[u8]) -> Value {
    let (off, len) = pd_field(t);
    if t.datatype == DataType::String {
        let bytes: Vec<u8> = (0..len / 8)
            .rev()
            .map(|k| (0..8).fold(0u8, |acc, b| acc | (u8::from(bit(pd, off + k * 8 + b)) << b)))
            .collect();
        return Value::String(bytes.into_iter().take_while(|c| *c != 0).map(char::from).collect());
    }
    let len = len.min(64);
    let mut raw: u64 = 0;
    for b in 0..len {
        if bit(pd, off + b) {
            raw |= 1 << b;
        }
    }
    match t.datatype {
        DataType::Bool => Value::Bool(raw != 0),
        DataType::Float32 => number_value(t, f32::from_bits(raw as u32) as f64),
        DataType::Float64 => number_value(t, f64::from_bits(raw)),
        DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            let shift = 64 - len as u32;
            let signed = ((raw << shift) as i64) >> shift;
            number_value(t, signed as f64)
        }
        _ => number_value(t, raw as f64),
    }
}

pub fn encode_pd(t: &TagDef, v: &Value, pd: &mut [u8]) -> Result<(), String> {
    let (off, len) = pd_field(t);
    if t.datatype == DataType::String {
        let s = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let nbytes = len / 8;
        let mut bytes: Vec<u8> = s.bytes().take(nbytes).collect();
        bytes.resize(nbytes, 0);
        for (k, byte) in bytes.iter().rev().enumerate() {
            for b in 0..8 {
                set_bit(pd, off + k * 8 + b, byte >> b & 1 == 1);
            }
        }
        return Ok(());
    }
    let len = len.min(64);
    let raw: u64 = match t.datatype {
        DataType::Bool => u64::from(truthy(v)),
        DataType::Float32 => (raw_number(t, v)? as f32).to_bits() as u64,
        DataType::Float64 => raw_number(t, v)?.to_bits(),
        _ => raw_number(t, v)?.round() as i64 as u64,
    };
    for b in 0..len {
        set_bit(pd, off + b, raw >> b & 1 == 1);
    }
    Ok(())
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

    #[test]
    fn registers_round_trip() {
        for (dt, v) in [
            (DataType::Int16, json!(-123)),
            (DataType::Uint32, json!(4_000_000_000u64)),
            (DataType::Float32, json!(12.5)),
            (DataType::Float64, json!(-3.25)),
            (DataType::Int64, json!(-9_000_000_000i64)),
            (DataType::Bool, json!(true)),
            (DataType::String, json!("hello")),
        ] {
            for order in [WordOrder::Big, WordOrder::Little] {
                let t = tag(dt);
                let w = encode_registers(&t, &v, order).unwrap();
                assert_eq!(w.len(), dt.registers(None) as usize);
                assert_eq!(decode_registers(&t, &w, order), v, "{dt:?} {order:?}");
            }
        }
    }

    #[test]
    fn float32_big_endian_layout() {
        let w = encode_registers(&tag(DataType::Float32), &json!(1.0), WordOrder::Big).unwrap();
        assert_eq!(w, vec![0x3F80, 0x0000]);
        let w = encode_registers(&tag(DataType::Float32), &json!(1.0), WordOrder::Little).unwrap();
        assert_eq!(w, vec![0x0000, 0x3F80]);
    }

    #[test]
    fn scaling() {
        let mut t = tag(DataType::Int16);
        t.scale = Some(0.1);
        let w = encode_registers(&t, &json!(21.5), WordOrder::Big).unwrap();
        assert_eq!(w, vec![215]);
        assert_eq!(decode_registers(&t, &w, WordOrder::Big), json!(21.5));
    }

    #[test]
    fn iodd_bit_offsets() {
        // ifm TV7105-style: 4 bytes, temperature int16 at bit offset 16, two switch bits at 0 and 1.
        let pd = [0x00, 0xD7, 0x00, 0x02]; // 215 << 16 | OUT2
        let mut temp = tag(DataType::Int16);
        temp.bit_offset = Some(16);
        temp.bit_length = Some(16);
        temp.scale = Some(0.1);
        assert_eq!(decode_pd(&temp, &pd), json!(21.5));
        let mut out1 = tag(DataType::Bool);
        out1.bit_offset = Some(0);
        let mut out2 = tag(DataType::Bool);
        out2.bit_offset = Some(1);
        assert_eq!(decode_pd(&out1, &pd), json!(false));
        assert_eq!(decode_pd(&out2, &pd), json!(true));

        let mut buf = [0u8; 4];
        encode_pd(&temp, &json!(21.5), &mut buf).unwrap();
        encode_pd(&out2, &json!(true), &mut buf).unwrap();
        assert_eq!(buf, pd);
    }

    #[test]
    fn negative_pd_ints() {
        let mut t = tag(DataType::Int16);
        t.bit_offset = Some(4);
        t.bit_length = Some(12);
        let mut pd = [0u8; 2];
        encode_pd(&t, &json!(-5), &mut pd).unwrap();
        assert_eq!(decode_pd(&t, &pd), json!(-5));
    }
}
