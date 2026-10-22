//! MQTT's packets (3.1, 3.1.1 and 5) read out of what passes the capture: the app's and its
//! broker's, each side as its bytes come, cut anywhere. Its own reader rather than
//! rumqttc's: that one takes a packet only whole (up to 256 MB), refuses 3.1, and gives
//! typed structs each to be laid out field by field.

use std::collections::HashMap;
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
const CONNACK: u8 = 2;
const PUBACK: u8 = 4;
const PUBCOMP: u8 = 7;
const SUBSCRIBE: u8 = 8;
const SUBACK: u8 = 9;
const UNSUBSCRIBE: u8 = 10;
const UNSUBACK: u8 = 11;
const DISCONNECT: u8 = 14;
const AUTH: u8 = 15;

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
    /// MQTT 5's topic aliases this side set: a PUBLISH may name its topic by one alone.
    aliases: HashMap<u16, String>,
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

    /// Starting afresh mid-connection, at a WebSocket message that begins a packet.
    pub fn new(sent: bool, level: Option<u8>) -> Self {
        Self {
            sent,
            level,
            head: Vec::new(),
            packet: None,
            lost: false,
            aliases: HashMap::new(),
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
        let v5 = self.v5();
        let read = match kind {
            CONNECT => connect(&mut c).map(|(at, summary, fields)| {
                if self.sent {
                    self.level = Some(at);
                    let _ = level.set(at);
                }
                (summary, fields)
            }),
            CONNACK => connack(&mut c, v5),
            PUBLISH => publish(&mut c, p.first, v5, &mut self.aliases),
            PUBACK..=PUBCOMP => ack(&mut c, v5),
            SUBSCRIBE => subscribe(&mut c, v5),
            SUBACK => suback(&mut c, v5),
            UNSUBSCRIBE => unsubscribe(&mut c, v5),
            UNSUBACK => unsuback(&mut c, v5),
            DISCONNECT | AUTH if v5 => reason_only(&mut c, kind),
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

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
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

    /// MQTT 5's properties, each as its name and value (spec names, so they can be looked
    /// up); none is left out, as a broker's limits and reasons are what one comes for.
    fn properties(&mut self) -> Option<Fields> {
        let n = self.varint()? as usize;
        let mut c = Cursor {
            bytes: self.take(n)?,
            at: 0,
        };
        let mut fields = Vec::new();
        while !c.rest().is_empty() {
            let id = c.varint()?;
            let (name, kind) = PROPERTIES.iter().find(|p| p.0 == id).map(|p| (p.1, p.2))?;
            let value = match kind {
                Prop::Byte => match (id, c.u8()?) {
                    (0x01, 0) => "bytes".into(),
                    (0x01, 1) => "UTF-8".into(),
                    (_, b) => b.to_string(),
                },
                Prop::U16 => c.u16()?.to_string(),
                Prop::Secs => format!("{} s", c.u32()?),
                Prop::U32 => c.u32()?.to_string(),
                Prop::Varint => c.varint()?.to_string(),
                Prop::Str => c.str()?,
                Prop::Bin => readable(c.bin()?),
                Prop::Pair => format!("{}={}", c.str()?, c.str()?),
            };
            // Server Keep Alive is in seconds too, but two bytes.
            let value = if id == 0x13 {
                format!("{value} s")
            } else {
                value
            };
            fields.push((name.to_owned(), value));
        }
        Some(fields)
    }
}

/// What a header says that doesn't fit its line: a name and its value each.
type Fields = Vec<(String, String)>;

#[derive(Clone, Copy)]
enum Prop {
    Byte,
    U16,
    U32,
    /// Four bytes of seconds.
    Secs,
    Varint,
    Str,
    Bin,
    Pair,
}

/// MQTT 5's properties (section 2.2.2.2): identifier, name, how its value is written.
const PROPERTIES: &[(u32, &str, Prop)] = &[
    (0x01, "Payload Format Indicator", Prop::Byte),
    (0x02, "Message Expiry Interval", Prop::Secs),
    (0x03, "Content Type", Prop::Str),
    (0x08, "Response Topic", Prop::Str),
    (0x09, "Correlation Data", Prop::Bin),
    (0x0b, "Subscription Identifier", Prop::Varint),
    (0x11, "Session Expiry Interval", Prop::Secs),
    (0x12, "Assigned Client Identifier", Prop::Str),
    (0x13, "Server Keep Alive", Prop::U16),
    (0x15, "Authentication Method", Prop::Str),
    (0x16, "Authentication Data", Prop::Bin),
    (0x17, "Request Problem Information", Prop::Byte),
    (0x18, "Will Delay Interval", Prop::Secs),
    (0x19, "Request Response Information", Prop::Byte),
    (0x1a, "Response Information", Prop::Str),
    (0x1c, "Server Reference", Prop::Str),
    (0x1f, "Reason String", Prop::Str),
    (0x21, "Receive Maximum", Prop::U16),
    (0x22, "Topic Alias Maximum", Prop::U16),
    (0x23, "Topic Alias", Prop::U16),
    (0x24, "Maximum QoS", Prop::Byte),
    (0x25, "Retain Available", Prop::Byte),
    (0x26, "User Property", Prop::Pair),
    (0x27, "Maximum Packet Size", Prop::U32),
    (0x28, "Wildcard Subscription Available", Prop::Byte),
    (0x29, "Subscription Identifier Available", Prop::Byte),
    (0x2a, "Shared Subscription Available", Prop::Byte),
];

/// Text when it is (a correlation ID often is), else hex.
fn readable(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.chars().any(char::is_control) => text.to_owned(),
        _ => bytes.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

/// A reason code as the spec names it (section 2.4), for the packet `kind` it's in: 0 and
/// the granted QoS read as such, a failure with its code to search by.
fn reason(code: u8, kind: u8) -> String {
    let name = match (code, kind) {
        (0x00, DISCONNECT) => "Normal disconnection",
        (0x00, SUBACK) => "Granted QoS 0",
        (0x00, _) => "Success",
        (0x01, _) => "Granted QoS 1",
        (0x02, _) => "Granted QoS 2",
        (0x04, _) => "Disconnect with Will Message",
        (0x10, _) => "No matching subscribers",
        (0x11, _) => "No subscription existed",
        (0x18, _) => "Continue authentication",
        (0x19, _) => "Re-authenticate",
        (0x80, _) => "Unspecified error",
        (0x81, _) => "Malformed Packet",
        (0x82, _) => "Protocol Error",
        (0x83, _) => "Implementation specific error",
        (0x84, _) => "Unsupported Protocol Version",
        (0x85, _) => "Client Identifier not valid",
        (0x86, _) => "Bad User Name or Password",
        (0x87, _) => "Not authorized",
        (0x88, _) => "Server unavailable",
        (0x89, _) => "Server busy",
        (0x8a, _) => "Banned",
        (0x8b, _) => "Server shutting down",
        (0x8c, _) => "Bad authentication method",
        (0x8d, _) => "Keep Alive timeout",
        (0x8e, _) => "Session taken over",
        (0x8f, _) => "Topic Filter invalid",
        (0x90, _) => "Topic Name invalid",
        (0x91, _) => "Packet Identifier in use",
        (0x92, _) => "Packet Identifier not found",
        (0x93, _) => "Receive Maximum exceeded",
        (0x94, _) => "Topic Alias invalid",
        (0x95, _) => "Packet too large",
        (0x96, _) => "Message rate too high",
        (0x97, _) => "Quota exceeded",
        (0x98, _) => "Administrative action",
        (0x99, _) => "Payload format invalid",
        (0x9a, _) => "Retain not supported",
        (0x9b, _) => "QoS not supported",
        (0x9c, _) => "Use another server",
        (0x9d, _) => "Server moved",
        (0x9e, _) => "Shared Subscriptions not supported",
        (0x9f, _) => "Connection rate exceeded",
        (0xa0, _) => "Maximum connect time",
        (0xa1, _) => "Subscription Identifiers not supported",
        (0xa2, _) => "Wildcard Subscriptions not supported",
        _ => "",
    };
    match code {
        0x00..=0x02 => name.to_owned(),
        _ => format!("0x{code:02x} {name}").trim_end().to_owned(),
    }
}

/// A CONNECT's level, its client ID, session and keep alive in a line; its will, login and
/// properties apart, the will's own prefixed "Will".
fn connect(c: &mut Cursor) -> Option<(u8, String, Fields)> {
    let _name = c.str()?;
    let level = c.u8()?;
    let flags = c.u8()?;
    let keep_alive = c.u16()?;
    let v5 = level == 5;
    let properties = if v5 { c.properties()? } else { Vec::new() };
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
        let will_properties = if v5 { c.properties()? } else { Vec::new() };
        let topic = c.str()?;
        let message = crate::binary::lossy(c.bin()?);
        let qos = (flags >> 3) & 3;
        let retain = if flags & 0x20 != 0 { " · retain" } else { "" };
        let will = format!("{topic} · QoS {qos}{retain} · {message}");
        fields.push(("will".into(), will));
        let prefixed = will_properties
            .into_iter()
            .map(|(k, v)| (format!("Will {k}"), v));
        fields.extend(prefixed);
    }
    if flags & 0x80 != 0 {
        fields.push(("username".into(), c.str()?));
    }
    if flags & 0x40 != 0 {
        fields.push(("password".into(), crate::binary::lossy(c.bin()?)));
    }
    fields.extend(properties);
    Some((level, line.join(" · "), fields))
}

/// A CONNACK's verdict and whether a session was kept.
fn connack(c: &mut Cursor, v5: bool) -> Option<(String, Fields)> {
    let session = c.u8()? & 1 != 0;
    let code = c.u8()?;
    let mut line = vec![match v5 {
        true => reason(code, CONNACK),
        // 3.1.1's own return codes (section 3.2.2.3).
        false => match code {
            0 => "Connection Accepted".into(),
            1 => "0x01 unacceptable protocol version".into(),
            2 => "0x02 identifier rejected".into(),
            3 => "0x03 Server unavailable".into(),
            4 => "0x04 bad user name or password".into(),
            5 => "0x05 not authorized".into(),
            code => format!("0x{code:02x}"),
        },
    }];
    if session {
        line.push("session present".into());
    }
    let fields = if v5 { c.properties()? } else { Vec::new() };
    Some((line.join(" · "), fields))
}

/// A PUBLISH's topic, QoS, packet ID and flags; the cursor is left at its message. MQTT 5
/// may name the topic by an alias it set earlier on this side: shown as the topic, alias
/// noted.
fn publish(
    c: &mut Cursor,
    first: u8,
    v5: bool,
    aliases: &mut HashMap<u16, String>,
) -> Option<(String, Fields)> {
    let qos = (first >> 1) & 3;
    let mut topic = c.str()?;
    let id = if qos > 0 { Some(c.u16()?) } else { None };
    let fields = if v5 { c.properties()? } else { Vec::new() };
    let alias = (fields.iter())
        .find(|(k, _)| k == "Topic Alias")
        .and_then(|(_, v)| v.parse::<u16>().ok());
    if let Some(alias) = alias {
        match topic.is_empty() {
            true => {
                let known = aliases.get(&alias).cloned();
                topic = known.unwrap_or_else(|| "(unknown alias)".into());
            }
            false => _ = aliases.insert(alias, topic.clone()),
        }
    }
    let mut line = vec![topic, format!("QoS {qos}")];
    if let Some(id) = id {
        line.push(format!("id {id}"));
    }
    if first & 0x01 != 0 {
        line.push("retain".into());
    }
    if first & 0x08 != 0 {
        line.push("dup".into());
    }
    Some((line.join(" · "), fields))
}

/// PUBACK, PUBREC, PUBREL, PUBCOMP: the packet ID, and MQTT 5's reason when it isn't
/// success (left out, it is).
fn ack(c: &mut Cursor, v5: bool) -> Option<(String, Fields)> {
    let mut line = format!("id {}", c.u16()?);
    if !v5 || c.rest().is_empty() {
        return Some((line, Vec::new()));
    }
    let code = c.u8()?;
    if code != 0 {
        line = format!("{line} · {}", reason(code, PUBACK));
    }
    let fields = match c.rest().is_empty() {
        true => Vec::new(),
        false => c.properties()?,
    };
    Some((line, fields))
}

/// A SUBSCRIBE's packet ID and each filter with the QoS it asks for, and MQTT 5's options
/// when they aren't the defaults.
fn subscribe(c: &mut Cursor, v5: bool) -> Option<(String, Fields)> {
    let id = c.u16()?;
    let fields = if v5 { c.properties()? } else { Vec::new() };
    let mut filters = Vec::new();
    while !c.rest().is_empty() {
        let filter = c.str()?;
        let options = c.u8()?;
        let mut one = format!("{filter} QoS {}", options & 3);
        if v5 {
            if options & 0x04 != 0 {
                one += " no local";
            }
            if options & 0x08 != 0 {
                one += " retain as published";
            }
            match (options >> 4) & 3 {
                1 => one += " retained if new",
                2 => one += " no retained",
                _ => {}
            }
        }
        filters.push(one);
    }
    Some((format!("id {id} · {}", filters.join(", ")), fields))
}

/// A SUBACK's packet ID and what each filter was granted, in the SUBSCRIBE's order.
fn suback(c: &mut Cursor, v5: bool) -> Option<(String, Fields)> {
    let id = c.u16()?;
    let fields = if v5 { c.properties()? } else { Vec::new() };
    let codes: Vec<_> = (c.rest().iter())
        .map(|&code| match (v5, code) {
            (false, 0..=2) => format!("QoS {code}"),
            (false, _) => format!("0x{code:02x} Failure"),
            (true, _) => reason(code, SUBACK),
        })
        .collect();
    Some((format!("id {id} · {}", codes.join(", ")), fields))
}

/// An UNSUBSCRIBE's packet ID and filters.
fn unsubscribe(c: &mut Cursor, v5: bool) -> Option<(String, Fields)> {
    let id = c.u16()?;
    let fields = if v5 { c.properties()? } else { Vec::new() };
    let mut filters = Vec::new();
    while !c.rest().is_empty() {
        filters.push(c.str()?);
    }
    Some((format!("id {id} · {}", filters.join(", ")), fields))
}

/// An UNSUBACK's packet ID, and in MQTT 5 each filter's outcome.
fn unsuback(c: &mut Cursor, v5: bool) -> Option<(String, Fields)> {
    let id = c.u16()?;
    if !v5 {
        return Some((format!("id {id}"), Vec::new()));
    }
    let fields = c.properties()?;
    let codes: Vec<_> = (c.rest().iter())
        .map(|&code| reason(code, UNSUBACK))
        .collect();
    Some((format!("id {id} · {}", codes.join(", ")), fields))
}

/// MQTT 5's DISCONNECT and AUTH: a reason (success when left out) and properties.
fn reason_only(c: &mut Cursor, kind: u8) -> Option<(String, Fields)> {
    let code = match c.rest().is_empty() {
        true => 0,
        false => c.u8()?,
    };
    let fields = match c.rest().is_empty() {
        true => Vec::new(),
        false => c.properties()?,
    };
    Some((reason(code, kind), fields))
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

    /// MQTT 5 as a broker answers: a verdict with its limits, refusals by name with their
    /// code, a PUBLISH that names its topic by an alias set before, and why it hung up.
    #[test]
    fn mqtt_5_reasons_properties_and_aliases_read_by_name() {
        use rumqttc::v5::mqttbytes::v5::{Packet as P5, Publish, PublishProperties};
        let level = OnceLock::new();
        let mut broker = Packets::broker(5);
        let mut bytes = Vec::new();
        // Session present, Success; Topic Alias Maximum 10, Assigned Client Identifier "abc".
        bytes.extend(b"\x20\x0c\x01\x00\x09\x22\x00\x0a\x12\x00\x03abc");
        // Granted QoS 1 to the first filter, Not authorized to the second.
        bytes.extend(b"\x90\x05\x00\x01\x00\x01\x87");
        let properties = PublishProperties {
            topic_alias: Some(3),
            content_type: Some("application/json".into()),
            message_expiry_interval: Some(60),
            user_properties: vec![("unit".into(), "°C".into())],
            ..Default::default()
        };
        let named = Publish::new(
            "a/b",
            rumqttc::v5::mqttbytes::QoS::AtMostOnce,
            "{\"t\":21}",
            Some(properties),
        );
        let alias = PublishProperties {
            topic_alias: Some(3),
            ..Default::default()
        };
        let aliased = Publish::new(
            "",
            rumqttc::v5::mqttbytes::QoS::AtMostOnce,
            "22",
            Some(alias),
        );
        for p in [named, aliased] {
            let mut out = bytes::BytesMut::new();
            P5::Publish(p).write(&mut out, None).unwrap();
            bytes.extend(&out);
        }
        // PUBACK for id 7: No matching subscribers.
        bytes.extend(b"\x40\x03\x00\x07\x10");
        // Session taken over, Reason String "kicked".
        bytes.extend(b"\xe0\x0b\x8e\x09\x1f\x00\x06kicked");
        let got = read(&mut broker, &bytes, &level);
        let fields = |i: usize| got[i].mqtt.as_ref().unwrap().fields.clone();
        let pairs = |p: &[(&str, &str)]| -> Fields {
            p.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            lines(&got),
            [
                ("CONNACK", "Success · session present".into(), Vec::new(), 0),
                (
                    "SUBACK",
                    "id 1 · Granted QoS 1, 0x87 Not authorized".into(),
                    Vec::new(),
                    0
                ),
                ("PUBLISH", "a/b · QoS 0".into(), b"{\"t\":21}".to_vec(), 8),
                ("PUBLISH", "a/b · QoS 0".into(), b"22".to_vec(), 2),
                (
                    "PUBACK",
                    "id 7 · 0x10 No matching subscribers".into(),
                    Vec::new(),
                    0
                ),
                (
                    "DISCONNECT",
                    "0x8e Session taken over".into(),
                    Vec::new(),
                    0
                ),
            ]
        );
        assert_eq!(
            fields(0),
            pairs(&[
                ("Topic Alias Maximum", "10"),
                ("Assigned Client Identifier", "abc")
            ])
        );
        let mut named = fields(2);
        named.sort();
        assert_eq!(
            named,
            pairs(&[
                ("Content Type", "application/json"),
                ("Message Expiry Interval", "60 s"),
                ("Topic Alias", "3"),
                ("User Property", "unit=°C"),
            ])
        );
        assert_eq!(fields(5), pairs(&[("Reason String", "kicked")]));

        // What the app asks of a subscription past its QoS.
        let mut app = Packets::new(true, Some(5));
        let got = read(&mut app, b"\x82\x07\x00\x02\x00\x00\x01t\x2d", &level);
        assert_eq!(
            got[0].mqtt.as_ref().unwrap().summary,
            "id 2 · t QoS 1 no local retain as published no retained"
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
