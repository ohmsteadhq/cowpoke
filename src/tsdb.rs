//! Storage formats for the historian.
//!
//! **Compressed day files** (`<day>.gor`) use the scheme from Facebook's Gorilla paper:
//! timestamps as delta-of-deltas and values as the XOR with the previous value, written
//! as a bit stream. Regular sampling and slowly changing values cost a few bits per point
//! instead of 16 bytes.
//!
//! **Hourly rollups** (`<day>.hour`) hold, for each hour that has points: count, min, max,
//! sum (for the average), first and last value. Long-range queries read these instead of
//! every point.

use anyhow::{Result, bail};

// ------------------------------------------------------------------ bits

struct BitWriter {
    buf: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self { buf: Vec::new(), bits: 0 }
    }

    /// Write the low `n` bits of `v`, most significant first.
    fn write(&mut self, v: u64, n: u32) {
        for i in (0..n).rev() {
            if self.bits % 8 == 0 {
                self.buf.push(0);
            }
            if (v >> i) & 1 == 1 {
                let last = self.buf.len() - 1;
                self.buf[last] |= 1 << (7 - self.bits % 8);
            }
            self.bits += 1;
        }
    }

    fn bit(&mut self, b: bool) {
        self.write(u64::from(b), 1);
    }
}

struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, n: u32) -> Result<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            let byte = *self
                .buf
                .get(self.pos / 8)
                .ok_or_else(|| anyhow::anyhow!("compressed history ends early"))?;
            v = (v << 1) | u64::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Ok(v)
    }

    fn bit(&mut self) -> Result<bool> {
        Ok(self.read(1)? == 1)
    }
}

// ------------------------------------------------------------------ points

const POINTS_MAGIC: &[u8; 4] = b"CPG1";

/// Compress points (sorted by time).
pub fn encode(points: &[(i64, f64)]) -> Vec<u8> {
    let mut w = BitWriter::new();
    let mut prev_ts = 0i64;
    let mut prev_delta = 0i64;
    let mut prev_bits = 0u64;
    // The current "meaningful bits" window: leading and trailing zeros.
    let mut window: Option<(u32, u32)> = None;

    for (i, &(ts, v)) in points.iter().enumerate() {
        let bits = v.to_bits();
        if i == 0 {
            w.write(ts as u64, 64);
            w.write(bits, 64);
        } else {
            let delta = ts.wrapping_sub(prev_ts);
            let dod = delta.wrapping_sub(prev_delta);
            match dod {
                0 => w.bit(false),
                -63..=64 => {
                    w.write(0b10, 2);
                    w.write((dod + 63) as u64, 7);
                }
                -255..=256 => {
                    w.write(0b110, 3);
                    w.write((dod + 255) as u64, 9);
                }
                -2047..=2048 => {
                    w.write(0b1110, 4);
                    w.write((dod + 2047) as u64, 12);
                }
                _ => {
                    w.write(0b1111, 4);
                    w.write(dod as u64, 64);
                }
            }
            prev_delta = delta;

            let xor = bits ^ prev_bits;
            if xor == 0 {
                w.bit(false);
            } else {
                w.bit(true);
                let lead = xor.leading_zeros();
                let trail = xor.trailing_zeros();
                match window {
                    Some((pl, pt)) if lead >= pl && trail >= pt => {
                        w.bit(false);
                        w.write(xor >> pt, 64 - pl - pt);
                    }
                    _ => {
                        let sig = 64 - lead - trail;
                        w.bit(true);
                        w.write(u64::from(lead), 6);
                        w.write(u64::from(sig - 1), 6);
                        w.write(xor >> trail, sig);
                        window = Some((lead, trail));
                    }
                }
            }
        }
        prev_ts = ts;
        prev_bits = bits;
    }

    let mut out = Vec::with_capacity(8 + w.buf.len());
    out.extend_from_slice(POINTS_MAGIC);
    out.extend_from_slice(&(points.len() as u32).to_le_bytes());
    out.extend_from_slice(&w.buf);
    out
}

pub fn decode(data: &[u8]) -> Result<Vec<(i64, f64)>> {
    if data.len() < 8 || &data[..4] != POINTS_MAGIC {
        bail!("not a compressed history file");
    }
    let n = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    let mut r = BitReader { buf: &data[8..], pos: 0 };
    let mut out = Vec::with_capacity(n);
    let (mut ts, mut delta, mut bits) = (0i64, 0i64, 0u64);
    let mut window = (0u32, 0u32);
    for i in 0..n {
        if i == 0 {
            ts = r.read(64)? as i64;
            bits = r.read(64)?;
        } else {
            let dod = if !r.bit()? {
                0
            } else if !r.bit()? {
                r.read(7)? as i64 - 63
            } else if !r.bit()? {
                r.read(9)? as i64 - 255
            } else if !r.bit()? {
                r.read(12)? as i64 - 2047
            } else {
                r.read(64)? as i64
            };
            delta = delta.wrapping_add(dod);
            ts = ts.wrapping_add(delta);

            if r.bit()? {
                let xor = if !r.bit()? {
                    let (pl, pt) = window;
                    r.read(64 - pl - pt)? << pt
                } else {
                    let lead = r.read(6)? as u32;
                    let sig = r.read(6)? as u32 + 1;
                    if lead + sig > 64 {
                        bail!("corrupt compressed history");
                    }
                    let trail = 64 - lead - sig;
                    window = (lead, trail);
                    r.read(sig)? << trail
                };
                bits ^= xor;
            }
        }
        out.push((ts, f64::from_bits(bits)));
    }
    Ok(out)
}

// ------------------------------------------------------------------ hourly rollups

pub const HOUR_MS: i64 = 3_600_000;
const HOURS_MAGIC: &[u8; 4] = b"CPH1";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hour {
    /// Start of the hour, milliseconds since the epoch.
    pub start: i64,
    pub count: u64,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub first: f64,
    pub last: f64,
}

/// One rollup per hour that has points (points sorted by time).
pub fn rollup(points: &[(i64, f64)]) -> Vec<Hour> {
    let mut out: Vec<Hour> = vec![];
    for &(ts, v) in points {
        let start = ts.div_euclid(HOUR_MS) * HOUR_MS;
        match out.last_mut() {
            Some(h) if h.start == start => {
                h.count += 1;
                h.min = h.min.min(v);
                h.max = h.max.max(v);
                h.sum += v;
                h.last = v;
            }
            _ => out.push(Hour { start, count: 1, min: v, max: v, sum: v, first: v, last: v }),
        }
    }
    out
}

pub fn encode_hours(hours: &[Hour]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + hours.len() * 56);
    out.extend_from_slice(HOURS_MAGIC);
    for h in hours {
        out.extend_from_slice(&h.start.to_le_bytes());
        out.extend_from_slice(&h.count.to_le_bytes());
        for f in [h.min, h.max, h.sum, h.first, h.last] {
            out.extend_from_slice(&f.to_le_bytes());
        }
    }
    out
}

pub fn decode_hours(data: &[u8]) -> Result<Vec<Hour>> {
    if data.len() < 4 || &data[..4] != HOURS_MAGIC || (data.len() - 4) % 56 != 0 {
        bail!("not an hourly rollup file");
    }
    let f = |b: &[u8], i: usize| f64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    Ok(data[4..]
        .chunks_exact(56)
        .map(|b| Hour {
            start: i64::from_le_bytes(b[0..8].try_into().unwrap()),
            count: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            min: f(b, 16),
            max: f(b, 24),
            sum: f(b, 32),
            first: f(b, 40),
            last: f(b, 48),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(points: &[(i64, f64)]) -> usize {
        let enc = encode(points);
        let dec = decode(&enc).unwrap();
        assert_eq!(dec.len(), points.len());
        for (a, b) in points.iter().zip(&dec) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1.to_bits(), b.1.to_bits());
        }
        enc.len()
    }

    #[test]
    fn empty_and_single() {
        round_trip(&[]);
        round_trip(&[(1_763_700_000_000, 42.5)]);
    }

    #[test]
    fn awkward_values_round_trip() {
        let t0 = 1_763_700_000_000i64;
        let pts = vec![
            (t0, 0.0),
            (t0 + 1, -0.0),
            (t0 + 1, f64::MAX),
            (t0 + 5_000_000, f64::MIN_POSITIVE),
            (t0 + 5_000_007, f64::NAN),
            (t0 + 5_000_008, f64::INFINITY),
            (t0 + 9_000_000_000, -1e300),
            (t0 + 9_000_000_001, 1.0),
            (t0 + 9_000_000_002, 1.0),
            (t0 - 50, 3.25), // out of order still round-trips
        ];
        round_trip(&pts);
    }

    #[test]
    fn regular_sampling_compresses_well() {
        // A day of one-second samples of a slowly moving float32 sensor (stored as f64).
        let t0 = 1_763_683_200_000i64;
        let pts: Vec<(i64, f64)> = (0..86_400)
            .map(|i| {
                let jitter = [0i64, 0, 1, -1, 0][i % 5];
                let v = (21.5 + (i as f64 / 3600.0).sin() * 2.0) as f32;
                (t0 + i as i64 * 1000 + jitter, v as f64)
            })
            .collect();
        let raw = pts.len() * 16;
        let packed = round_trip(&pts);
        assert!(packed * 3 < raw, "only {raw} → {packed} bytes");

        // Steady values (a setpoint, a bool) shrink to almost nothing.
        let flat: Vec<(i64, f64)> = (0..86_400).map(|i| (t0 + i * 1000, 1.0)).collect();
        let packed = round_trip(&flat);
        assert!(packed < 25_000, "{packed} bytes");
    }

    #[test]
    fn hourly_rollups() {
        let t0 = 1_763_683_200_000i64; // an hour boundary
        let pts = vec![(t0, 1.0), (t0 + 10, 3.0), (t0 + HOUR_MS + 5, -2.0)];
        let h = rollup(&pts);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0], Hour { start: t0, count: 2, min: 1.0, max: 3.0, sum: 4.0, first: 1.0, last: 3.0 });
        assert_eq!(h[1].start, t0 + HOUR_MS);
        assert_eq!(decode_hours(&encode_hours(&h)).unwrap(), h);
    }
}
