//! Packets to TCP byte streams: link layers, IPv4/IPv6, and a small in-order TCP
//! reassembler per direction. Good enough for industrial traffic on a mirror port:
//! out-of-order segments resync the stream instead of being buffered.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Link-layer framing of captured packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Ethernet,
    /// BSD loopback: 4-byte address family header.
    Null,
    /// Raw IP, no link header.
    Raw,
    /// Linux "cooked" capture (any interface).
    LinuxSll,
    LinuxSll2,
}

impl Link {
    #[cfg_attr(not(feature = "live-capture"), allow(dead_code))]
    pub fn from_dlt(dlt: i32) -> Option<Self> {
        Some(match dlt {
            1 => Link::Ethernet,
            0 | 108 => Link::Null,
            12 | 14 | 101 | 228 | 229 => Link::Raw,
            113 => Link::LinuxSll,
            276 => Link::LinuxSll2,
            _ => return None,
        })
    }
}

/// One TCP segment's essentials.
#[derive(Debug)]
pub struct Segment<'a> {
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub seq: u32,
    pub syn: bool,
    pub fin_or_rst: bool,
    pub payload: &'a [u8],
}

fn be16(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

/// Find the IP packet inside a frame and return (ethertype-ish version, ip bytes).
fn ip_payload(link: Link, f: &[u8]) -> Option<&[u8]> {
    match link {
        Link::Raw => Some(f),
        Link::Null => f.get(4..),
        Link::LinuxSll => {
            let et = be16(f, 14)?;
            (et == 0x0800 || et == 0x86DD).then(|| &f[16..])
        }
        Link::LinuxSll2 => {
            let et = be16(f, 0)?;
            (et == 0x0800 || et == 0x86DD).then(|| f.get(20..)).flatten()
        }
        Link::Ethernet => {
            let mut et = be16(f, 12)?;
            let mut off = 14;
            // 802.1Q / QinQ VLAN tags.
            while et == 0x8100 || et == 0x88A8 {
                et = be16(f, off + 2)?;
                off += 4;
            }
            (et == 0x0800 || et == 0x86DD).then(|| f.get(off..)).flatten()
        }
    }
}

/// The sender's MAC and IPv4 address of an Ethernet frame (IP or ARP), for
/// duplicate-address checks.
pub fn ether_sender(link: Link, f: &[u8]) -> Option<([u8; 6], Ipv4Addr)> {
    if link != Link::Ethernet {
        return None;
    }
    let mac: [u8; 6] = f.get(6..12)?.try_into().ok()?;
    let mut et = be16(f, 12)?;
    let mut off = 14;
    while et == 0x8100 || et == 0x88A8 {
        et = be16(f, off + 2)?;
        off += 4;
    }
    let p = f.get(off..)?;
    let ip = match et {
        0x0800 if p.first()? >> 4 == 4 => <[u8; 4]>::try_from(p.get(12..16)?).ok()?,
        // ARP for IPv4 over Ethernet: sender protocol address at 14.
        0x0806 if be16(p, 2)? == 0x0800 => <[u8; 4]>::try_from(p.get(14..18)?).ok()?,
        _ => return None,
    };
    let ip = Ipv4Addr::from(ip);
    (!ip.is_unspecified()).then_some((mac, ip))
}

/// Parse a captured frame down to a TCP segment.
pub fn tcp_segment(link: Link, frame: &[u8]) -> Option<Segment<'_>> {
    let ip = ip_payload(link, frame)?;
    let (src, dst, proto, l4) = match ip.first()? >> 4 {
        4 => {
            let ihl = ((ip[0] & 0x0F) as usize) * 4;
            let total = (be16(ip, 2)? as usize).min(ip.len());
            // Fragments other than the first carry no TCP header.
            let frag = be16(ip, 6)? & 0x1FFF;
            if frag != 0 || ihl < 20 || total < ihl {
                return None;
            }
            let s = IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(ip.get(12..16)?).ok()?));
            let d = IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(ip.get(16..20)?).ok()?));
            (s, d, ip[9], &ip[ihl..total])
        }
        6 => {
            let plen = be16(ip, 4)? as usize;
            let s = IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(ip.get(8..24)?).ok()?));
            let d = IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(ip.get(24..40)?).ok()?));
            // Extension headers aren't followed: industrial TCP doesn't use them.
            (s, d, ip[6], ip.get(40..(40 + plen).min(ip.len()))?)
        }
        _ => return None,
    };
    if proto != 6 {
        return None;
    }
    let off = ((l4.get(12)? >> 4) as usize) * 4;
    let flags = *l4.get(13)?;
    Some(Segment {
        src: SocketAddr::new(src, be16(l4, 0)?),
        dst: SocketAddr::new(dst, be16(l4, 2)?),
        seq: u32::from_be_bytes(l4.get(4..8)?.try_into().ok()?),
        syn: flags & 0x02 != 0,
        fin_or_rst: flags & 0x05 != 0,
        payload: l4.get(off..).unwrap_or_default(),
    })
}

/// Bytes of one direction of a TCP connection, in order.
#[derive(Default)]
pub struct Stream {
    next: Option<u32>,
    pub buf: Vec<u8>,
    /// True once a gap was seen and the buffer restarted mid-stream.
    pub resynced: bool,
    /// Segments whose bytes had all been seen already (TCP retransmissions).
    pub retransmits: u64,
    /// Segments with data.
    pub segments: u64,
}

const MAX_BUF: usize = 1 << 20;

impl Stream {
    /// Add a segment; returns false if it couldn't be placed (gap: the stream restarts).
    pub fn push(&mut self, seg: &Segment) -> bool {
        let mut seq = seg.seq;
        if seg.syn {
            self.next = Some(seq.wrapping_add(1));
            self.buf.clear();
            seq = seq.wrapping_add(1);
            if seg.payload.is_empty() {
                return true;
            }
        }
        let data = seg.payload;
        if data.is_empty() {
            return true;
        }
        let Some(next) = self.next else {
            // Joined mid-connection: start here.
            self.next = Some(seq.wrapping_add(data.len() as u32));
            self.buf.extend_from_slice(data);
            self.resynced = true;
            return true;
        };
        let delta = seq.wrapping_sub(next) as i32;
        if delta > 0 {
            // A gap: what we have can't be trusted to continue. Restart from here.
            self.buf.clear();
            self.buf.extend_from_slice(data);
            self.next = Some(seq.wrapping_add(data.len() as u32));
            self.resynced = true;
            return false;
        }
        let skip = (-delta) as usize;
        self.segments += 1;
        if skip >= data.len() {
            self.retransmits += 1;
        }
        if skip < data.len() {
            self.buf.extend_from_slice(&data[skip..]);
            self.next = Some(seq.wrapping_add(data.len() as u32));
        }
        if self.buf.len() > MAX_BUF {
            self.buf.clear();
            self.resynced = true;
        }
        true
    }

    pub fn consume(&mut self, n: usize) {
        self.buf.drain(..n.min(self.buf.len()));
    }
}

#[derive(Default)]
pub struct Streams {
    pub map: HashMap<(SocketAddr, SocketAddr), Stream>,
}

impl Streams {
    /// Feed a segment; returns the stream for its direction.
    pub fn push(&mut self, seg: &Segment) -> &mut Stream {
        let s = self.map.entry((seg.src, seg.dst)).or_default();
        s.push(seg);
        s
    }

    pub fn close(&mut self, seg: &Segment) {
        self.map.remove(&(seg.src, seg.dst));
    }
}

// ------------------------------------------------------------------ files

/// A captured frame from a file or interface.
pub struct Frame {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub link: Link,
    pub data: Vec<u8>,
}

fn ts(d: std::time::Duration) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos()).unwrap_or_default()
}

fn link_of(dl: pcap_file::DataLink) -> Option<Link> {
    use pcap_file::DataLink as D;
    Some(match dl {
        D::ETHERNET => Link::Ethernet,
        D::NULL | D::LOOP => Link::Null,
        D::RAW | D::IPV4 | D::IPV6 => Link::Raw,
        D::LINUX_SLL => Link::LinuxSll,
        D::LINUX_SLL2 => Link::LinuxSll2,
        _ => return None,
    })
}

/// Read every frame of a pcap or pcapng file.
pub fn read_file(path: &std::path::Path) -> anyhow::Result<Vec<Frame>> {
    use anyhow::Context as _;
    let bytes = std::fs::read(path).with_context(|| format!("can't read {}", path.display()))?;
    let mut out = vec![];
    if bytes.get(..4) == Some(&[0x0A, 0x0D, 0x0D, 0x0A]) {
        let mut r = pcap_file::pcapng::PcapNgReader::new(&bytes[..])?;
        let mut links: Vec<Option<Link>> = vec![];
        while let Some(b) = r.next_block() {
            match b?.0 {
                pcap_file::pcapng::Block::InterfaceDescription(i) => links.push(link_of(i.linktype)),
                pcap_file::pcapng::Block::EnhancedPacket(p) => {
                    if let Some(Some(link)) = links.get(p.interface_id as usize) {
                        out.push(Frame { ts: ts(p.timestamp), link: *link, data: p.data.to_vec() });
                    }
                }
                pcap_file::pcapng::Block::SimplePacket(p) => {
                    if let Some(Some(link)) = links.first() {
                        out.push(Frame { ts: chrono::Utc::now(), link: *link, data: p.data.to_vec() });
                    }
                }
                _ => {}
            }
        }
    } else {
        let mut r = pcap_file::pcap::PcapReader::new(&bytes[..])?;
        let link = link_of(r.header().datalink)
            .ok_or_else(|| anyhow::anyhow!("unsupported link type {:?}", r.header().datalink))?;
        while let Some(p) = r.next_packet() {
            let p = p?;
            out.push(Frame { ts: ts(p.timestamp()), link, data: p.data().to_vec() });
        }
    }
    Ok(out)
}

/// Build a raw-IPv4 TCP frame (for tests and the recording proxy).
#[cfg(test)]
pub fn ipv4_tcp_frame(src: SocketAddr, dst: SocketAddr, seq: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
    let (IpAddr::V4(s), IpAddr::V4(d)) = (src.ip(), dst.ip()) else {
        panic!("ipv4 only");
    };
    let total = 20 + 20 + payload.len();
    let mut f = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 6, 0, 0];
    f[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    f.extend_from_slice(&s.octets());
    f.extend_from_slice(&d.octets());
    f.extend_from_slice(&src.port().to_be_bytes());
    f.extend_from_slice(&dst.port().to_be_bytes());
    f.extend_from_slice(&seq.to_be_bytes());
    f.extend_from_slice(&[0, 0, 0, 0, 0x50, flags, 0xFF, 0xFF, 0, 0, 0, 0]);
    f.extend_from_slice(payload);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembles_in_order_and_resyncs() {
        let a: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:502".parse().unwrap();
        let mut st = Streams::default();
        let f = ipv4_tcp_frame(a, b, 99, 0x02, &[]);
        st.push(&tcp_segment(Link::Raw, &f).unwrap());
        for (seq, d) in [(100u32, &b"abc"[..]), (103, b"def"), (101, b"bcdefg")] {
            let f = ipv4_tcp_frame(a, b, seq, 0x18, d);
            st.push(&tcp_segment(Link::Raw, &f).unwrap());
        }
        assert_eq!(st.map[&(a, b)].buf, b"abcdefg");
        // A gap restarts the stream at the new segment.
        let f = ipv4_tcp_frame(a, b, 200, 0x18, b"xyz");
        st.push(&tcp_segment(Link::Raw, &f).unwrap());
        assert_eq!(st.map[&(a, b)].buf, b"xyz");
        assert!(st.map[&(a, b)].resynced);
    }

    #[test]
    fn ethernet_vlan() {
        let a: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:502".parse().unwrap();
        let ip = ipv4_tcp_frame(a, b, 1, 0x18, b"hi");
        let mut f = vec![0u8; 12];
        f.extend_from_slice(&[0x81, 0x00, 0x00, 0x05, 0x08, 0x00]);
        f.extend_from_slice(&ip);
        let s = tcp_segment(Link::Ethernet, &f).unwrap();
        assert_eq!((s.src, s.dst, s.payload), (a, b, &b"hi"[..]));
    }
}
