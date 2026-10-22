//! MQTT's packets (3.1, 3.1.1 and 5) read out of what passes the capture: the app's and its
//! broker's, each side as its bytes come, cut anywhere. Its own reader rather than
//! rumqttc's: that one takes a packet only whole (up to 256 MB), refuses 3.1, and gives
//! typed structs each to be laid out field by field.

use std::sync::OnceLock;
use std::time::SystemTime;

use super::{MAX_BODY, StreamMessage};

/// One MQTT packet as it went: its type and what its header says. Its payload, a PUBLISH's
/// message, is the `StreamMessage`'s data.
#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    /// 1 CONNECT … 15 AUTH.
    pub kind: u8,
    /// Its header in a line: a PUBLISH's topic and QoS, a CONNECT's client ID.
    pub summary: String,
    /// What doesn't fit the line, a line each: a CONNECT's login and will.
    pub fields: Vec<(String, String)>,
}

pub const PUBLISH: u8 = 3;
const CONNECT: u8 = 1;
const SUBSCRIBE: u8 = 8;

const NAMES: [&str; 16] = [
    "RESERVED",
    "CONNECT",
    "CONNACK",
    "PUBLISH",
    "PUBACK",
    "PUBREC",
    "PUBREL",
    "PUBCOMP",
    "SUBSCRIBE",
    "SUBACK",
    "UNSUBSCRIBE",
    "UNSUBACK",
    "PINGREQ",
    "PINGRESP",
    "DISCONNECT",
    "AUTH",
];

impl Packet {
    pub fn name(&self) -> &'static str {
        NAMES[self.kind as usize & 0x0f]
    }
}

/// The protocol's name for a CONNECT's level.
pub fn version(level: u8) -> String {
    match level {
        3 => "MQTT 3.1".into(),
        4 => "MQTT 3.1.1".into(),
        5 => "MQTT 5".into(),
        level => format!("MQTT (level {level})"),
    }
}

/// Whether `first`, what an app sent first into a tunnel, starts MQTT's CONNECT: its type,
/// its remaining length, then the protocol's name. None until there are enough to tell.
pub fn connect_starts(first: &[u8]) -> Option<bool> {
    if *first.first()? != CONNECT << 4 {
        return Some(false);
    }
    let mut at = 1;
    loop {
        let b = *first.get(at)?;
        at += 1;
        if b & 0x80 == 0 {
            break;
        }
        if at == 5 {
            return Some(false);
        }
    }
    let got = &first[at..];
    let mut more = false;
    // "MQTT" from 3.1.1 on; 3.1 said "MQIsdp".
    for want in [&b"\x00\x04MQTT"[..], b"\x00\x06MQIsdp"] {
        let n = got.len().min(want.len());
        if got[..n] == want[..n] {
            if n == want.len() {
                return Some(true);
            }
            more = true;
        }
    }
    (!more).then_some(false)
}

/// One side's packets, read as its bytes come.
pub struct Packets {
    sent: bool,
    /// The protocol's level: the app's side learns it from its CONNECT, the broker's
    /// starts with it. MQTT 5 puts properties in most headers.
    level: Option<u8>,
    /// A fixed header so far: the type byte, then the remaining length's.
    head: Vec<u8>,
    packet: Option<Pending>,
    /// A fixed header that can't be one: where the next packet starts is unknown.
    lost: bool,
}

struct Pending {
    /// The type in the high four bits, its flags in the low.
    first: u8,
    left: u64,
    /// The remaining length: all that follows the fixed header.
    size: u64,
    /// The first `MAX_BODY` bytes of it.
    body: Vec<u8>,
}

impl Packets {
    pub fn app() -> Self {
        Self::new(true, None)
    }

    pub fn broker(level: u8) -> Self {
        Self::new(false, Some(level))
    }

    fn new(sent: bool, level: Option<u8>) -> Self {
        Self {
            sent,
            level,
            head: Vec::new(),
            packet: None,
            lost: false,
        }
    }

    /// The packets `bytes` end. A CONNECT's level goes into `level` for the broker's side.
    pub fn feed(&mut self, mut bytes: &[u8], level: &OnceLock<u8>) -> Vec<StreamMessage> {
        let mut out = Vec::new();
        while !bytes.is_empty() && !self.lost {
            let Some(p) = &mut self.packet else {
                self.head.push(bytes[0]);
                bytes = &bytes[1..];
                match fixed_header(&self.head) {
                    Err(()) => self.lost = true,
                    Ok(None) => {}
                    Ok(Some((first, size))) => {
                        self.head.clear();
                        let body = Vec::new();
                        let left = size;
                        self.packet = Some(Pending {
                            first,
                            left,
                            size,
                            body,
                        });
                        if size == 0 {
                            self.end(&mut out, level);
                        }
                    }
                }
                continue;
            };
            let n = p.left.min(bytes.len() as u64) as usize;
            let room = MAX_BODY.saturating_sub(p.body.len()).min(n);
            p.body.extend_from_slice(&bytes[..room]);
            p.left -= n as u64;
            bytes = &bytes[n..];
            if p.left == 0 {
                self.end(&mut out, level);
            }
        }
        out
    }

    fn end(&mut self, out: &mut Vec<StreamMessage>, level: &OnceLock<u8>) {
        let Some(p) = self.packet.take() else {
            return;
        };
        let kind = p.first >> 4;
        let mut c = Cursor {
            bytes: &p.body,
            at: 0,
        };
        let read = match kind {
            CONNECT => connect(&mut c).map(|(at, summary, fields)| {
                if self.sent {
                    self.level = Some(at);
                    let _ = level.set(at);
                }
                (summary, fields)
            }),
            PUBLISH => publish(&mut c, p.first, self.v5()).map(|s| (s, Vec::new())),
            SUBSCRIBE => subscribe(&mut c, self.v5()).map(|s| (s, Vec::new())),
            _ => Some((String::new(), Vec::new())),
        };
        let (summary, fields) = read.unwrap_or_else(|| ("unreadable".into(), Vec::new()));
        // Only a PUBLISH carries a message: what's left once its header is read.
        let (data, size) = match kind {
            PUBLISH if c.at <= p.body.len() => (p.body[c.at..].to_vec(), p.size - c.at as u64),
            _ => (Vec::new(), 0),
        };
        out.push(StreamMessage {
            at: SystemTime::now(),
            sent: self.sent,
            // As MQTT over WebSocket rides binary messages.
            opcode: 2,
            data,
            size,
            mqtt: Some(Packet {
                kind,
                summary,
                fields,
            }),
        });
    }

    fn v5(&self) -> bool {
        self.level == Some(5)
    }
}

/// A fixed header once `head` holds all of it: the type byte, then the remaining length in
/// 1 to 4 bytes, each but the last with its top bit set.
fn fixed_header(head: &[u8]) -> Result<Option<(u8, u64)>, ()> {
    let Some((&first, length)) = head.split_first() else {
        return Ok(None);
    };
    let Some(last) = length.iter().position(|b| b & 0x80 == 0) else {
        return match length.len() {
            4.. => Err(()),
            _ => Ok(None),
        };
    };
    let size = (length[..=last].iter().rev()).fold(0u64, |n, b| n << 7 | (b & 0x7f) as u64);
    Ok(Some((first, size)))
}

/// Reads a header's fields in order; None once one runs past what was kept.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let got = self.bytes.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(got)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn varint(&mut self) -> Option<u32> {
        let mut n = 0u32;
        for i in 0..4 {
            let b = self.u8()?;
            n |= ((b & 0x7f) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Some(n);
            }
        }
        None
    }

    /// Two bytes of length, then that many.
    fn bin(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()? as usize;
        self.take(n)
    }

    fn str(&mut self) -> Option<String> {
        Some(crate::binary::lossy(self.bin()?))
    }

    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at.min(self.bytes.len())..]
    }

    /// MQTT 5's properties, passed over.
    fn properties(&mut self) -> Option<()> {
        let n = self.varint()? as usize;
        self.take(n).map(|_| ())
    }
}

/// What a header says that doesn't fit its line: a name and its value each.
type Fields = Vec<(String, String)>;

/// A CONNECT's level, its client ID, session and keep alive in a line, and its will and
/// login apart.
fn connect(c: &mut Cursor) -> Option<(u8, String, Fields)> {
    let _name = c.str()?;
    let level = c.u8()?;
    let flags = c.u8()?;
    let keep_alive = c.u16()?;
    let v5 = level == 5;
    if v5 {
        c.properties()?;
    }
    let id = c.str()?;
    let mut line = vec![match id.is_empty() {
        true => "no client ID".to_owned(),
        false => id,
    }];
    if flags & 0x02 != 0 {
        line.push(if v5 { "clean start" } else { "clean session" }.into());
    }
    line.push(format!("keep alive {keep_alive} s"));
    let mut fields = Vec::new();
    if flags & 0x04 != 0 {
        if v5 {
            c.properties()?;
        }
        let topic = c.str()?;
        let message = crate::binary::lossy(c.bin()?);
        let qos = (flags >> 3) & 3;
        let retain = if flags & 0x20 != 0 { " · retain" } else { "" };
        let will = format!("{topic} · QoS {qos}{retain} · {message}");
        fields.push(("will".into(), will));
    }
    if flags & 0x80 != 0 {
        fields.push(("username".into(), c.str()?));
    }
    if flags & 0x40 != 0 {
        fields.push(("password".into(), crate::binary::lossy(c.bin()?)));
    }
    Some((level, line.join(" · "), fields))
}

/// A PUBLISH's topic, QoS, packet ID and flags; the cursor is left at its message.
fn publish(c: &mut Cursor, first: u8, v5: bool) -> Option<String> {
    let qos = (first >> 1) & 3;
    let mut line = vec![c.str()?, format!("QoS {qos}")];
    if qos > 0 {
        line.push(format!("id {}", c.u16()?));
    }
    if v5 {
        c.properties()?;
    }
    if first & 0x01 != 0 {
        line.push("retain".into());
    }
    if first & 0x08 != 0 {
        line.push("dup".into());
    }
    Some(line.join(" · "))
}

/// A SUBSCRIBE's packet ID and each filter with the QoS it asks for.
fn subscribe(c: &mut Cursor, v5: bool) -> Option<String> {
    let id = c.u16()?;
    if v5 {
        c.properties()?;
    }
    let mut filters = Vec::new();
    while !c.rest().is_empty() {
        let filter = c.str()?;
        let qos = c.u8()? & 3;
        filters.push(format!("{filter} QoS {qos}"));
    }
    Some(format!("id {id} · {}", filters.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each side sent, cut anywhere: a byte at a time reads as all at once.
    fn read(packets: &mut Packets, bytes: &[u8], level: &OnceLock<u8>) -> Vec<StreamMessage> {
        bytes
            .iter()
            .flat_map(|b| packets.feed(std::slice::from_ref(b), level))
            .collect()
    }

    fn connect_v311() -> Vec<u8> {
        let mut body = b"\x00\x04MQTT\x04".to_vec();
        // Username, password, will retain, will QoS 1, will, clean session.
        body.extend([0xc0 | 0x20 | 0x08 | 0x04 | 0x02, 0, 60]);
        for part in [&b"tester"[..], b"status/tester", b"offline", b"u", b"p"] {
            body.extend((part.len() as u16).to_be_bytes());
            body.extend(part);
        }
        [&[0x10, body.len() as u8][..], &body].concat()
    }

    /// The app's CONNECT tells its level, its client ID and login; the broker's side then
    /// reads MQTT 5's properties out of the way, so a PUBLISH's message is what follows.
    #[test]
    fn packets_read_however_the_bytes_come() {
        let level = OnceLock::new();
        let connect = connect_v311();
        assert_eq!(connect_starts(&connect[..3]), None);
        assert_eq!(connect_starts(&connect), Some(true));
        assert_eq!(connect_starts(b"GET / HTTP/1.1\r\n"), Some(false));
        assert_eq!(connect_starts(b"\x10\x05\x00\x04HTTP"), Some(false));
        let mut app = Packets::app();
        let got = read(&mut app, &connect, &level);
        let p = got[0].mqtt.as_ref().unwrap();
        assert_eq!(
            (p.name(), p.summary.as_str()),
            ("CONNECT", "tester · clean session · keep alive 60 s")
        );
        assert_eq!(
            p.fields,
            [
                (
                    "will".into(),
                    "status/tester · QoS 1 · retain · offline".into()
                ),
                ("username".into(), "u".into()),
                ("password".into(), "p".into()),
            ]
        );
        assert_eq!(level.get(), Some(&4));

        // QoS 1 to a/1, id 7, retained: "hi"; a/# at QoS 1 and b at 0.
        let publish = b"\x33\x09\x00\x03a/1\x00\x07hi";
        let subscribe = b"\x82\x0c\x00\x01\x00\x03a/#\x01\x00\x01b\x00";
        let all = [&publish[..], subscribe, b"\xe0\x00"].concat();
        assert_eq!(
            lines(&read(&mut app, &all, &level)),
            [
                (
                    "PUBLISH",
                    "a/1 · QoS 1 · id 7 · retain".into(),
                    b"hi".to_vec(),
                    2
                ),
                (
                    "SUBSCRIBE",
                    "id 1 · a/# QoS 1, b QoS 0".into(),
                    Vec::new(),
                    0
                ),
                ("DISCONNECT", String::new(), Vec::new(), 0),
            ]
        );

        // MQTT 5: a content type before the message.
        let mut broker = Packets::broker(5);
        let publish = b"\x30\x0c\x00\x03a/1\x04\x03\x00\x01xhi";
        assert_eq!(
            lines(&read(&mut broker, publish, &level)),
            [("PUBLISH", "a/1 · QoS 0".into(), b"hi".to_vec(), 2)]
        );
    }

    /// A message past `MAX_BODY` keeps its start and its whole size, and the packet after it
    /// is read as ever: memory stays capped and the framing isn't lost.
    #[test]
    fn a_big_message_is_cut_and_the_next_packet_still_read() {
        let level = OnceLock::new();
        let message = vec![b'x'; 300_000];
        let size = 3 + message.len();
        let mut length = Vec::new();
        let mut n = size;
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            length.push(if n > 0 { b | 0x80 } else { b });
            if n == 0 {
                break;
            }
        }
        let all = [&[0x30][..], &length, b"\x00\x01t", &message, b"\xc0\x00"].concat();
        let mut broker = Packets::broker(4);
        let got: Vec<_> = (all.chunks(7000))
            .flat_map(|c| broker.feed(c, &level))
            .collect();
        assert_eq!(
            lines(&got),
            [
                (
                    "PUBLISH",
                    "t · QoS 0".into(),
                    vec![b'x'; MAX_BODY - 3],
                    300_000
                ),
                ("PINGREQ", String::new(), Vec::new(), 0),
            ]
        );
    }

    fn lines(got: &[StreamMessage]) -> Vec<(&'static str, String, Vec<u8>, u64)> {
        (got.iter())
            .map(|m| {
                let p = m.mqtt.as_ref().unwrap();
                (p.name(), p.summary.clone(), m.data.clone(), m.size)
            })
            .collect()
    }
}
