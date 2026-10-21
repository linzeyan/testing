//! How bytes that aren't plain text read in the capture: a hex dump, protobuf without its
//! schema (as `protoc --decode_raw` prints it), and gRPC's length-prefixed messages.

use std::fmt::Write;

/// `bytes` as text when they read as text: UTF-8 without control characters but
/// whitespace. A start cut mid-character still reads, up to it. A NUL shown would also
/// cut what's copied: Windows' clipboard ends text at the first.
pub fn printable(bytes: &[u8]) -> Option<&str> {
    let text = utf8(bytes)?;
    let plain = !(text.chars()).any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'));
    plain.then_some(text)
}

/// UTF-8, a character cut at the end left off.
pub fn utf8(bytes: &[u8]) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&bytes[..e.valid_up_to()]).ok(),
        Err(_) => None,
    }
}

/// Any bytes as text: what isn't (bad UTF-8, control characters) as U+FFFD.
pub fn lossy(bytes: &[u8]) -> String {
    (String::from_utf8_lossy(bytes).chars())
        .map(|c| match c {
            '\n' | '\r' | '\t' => c,
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect()
}

/// 16 bytes a line as `hexdump -C` prints them: the offset, the bytes, and the printable
/// ones.
pub fn hexdump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, line) in bytes.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", i * 16);
        for j in 0..16 {
            match line.get(j) {
                Some(b) => _ = write!(out, "{b:02x} "),
                None => out.push_str("   "),
            }
            if j == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        out.extend(
            line.iter()
                .map(|&b| match b.is_ascii_graphic() || b == b' ' {
                    true => b as char,
                    false => '.',
                }),
        );
        out.push_str("|\n");
    }
    out
}

/// One of a gRPC body's messages, after its 5-byte prefix: a flag, then the length.
#[derive(Debug, PartialEq)]
pub struct GrpcMessage {
    /// gRPC-Web's last, its trailers as HTTP/1 header lines.
    pub trailers: bool,
    pub compressed: bool,
    /// Unpacked when compressed with gzip or deflate; else as it went.
    pub data: Vec<u8>,
    /// As the prefix gives it; a cut body may not hold all of it.
    pub size: usize,
}

/// A gRPC body's messages, compressed ones unpacked by `encoding` (its grpc-encoding);
/// None for what isn't framed as gRPC. A body cut short ends at the last whole prefix.
pub fn grpc_messages(mut body: &[u8], encoding: &str) -> Option<Vec<GrpcMessage>> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let Some((head, rest)) = body.split_first_chunk::<5>() else {
            return (!out.is_empty()).then_some(out);
        };
        // Bit 0: compressed; bit 7: gRPC-Web's trailers. Anything else isn't gRPC.
        if head[0] & !0x81 != 0 {
            return None;
        }
        let size = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let data = &rest[..size.min(rest.len())];
        let compressed = head[0] & 1 != 0;
        let data = match compressed {
            true => unpack(data, encoding).unwrap_or_else(|| data.to_vec()),
            false => data.to_vec(),
        };
        out.push(GrpcMessage {
            trailers: head[0] & 0x80 != 0,
            compressed,
            data,
            size,
        });
        body = &rest[size.min(rest.len())..];
    }
    Some(out)
}

/// What a compressed gRPC message unpacks to, at most 1 MiB: a few KiB can unpack to
/// gigabytes.
fn unpack(data: &[u8], encoding: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let r: Box<dyn Read> = match encoding.trim() {
        "gzip" => Box::new(flate2::read::GzDecoder::new(data)),
        "deflate" => Box::new(flate2::read::ZlibDecoder::new(data)),
        _ => return None,
    };
    let mut out = Vec::new();
    r.take(1 << 20).read_to_end(&mut out).ok()?;
    Some(out)
}

/// Protobuf read without its schema, as `protoc --decode_raw` prints it: field numbers,
/// varints as numbers, fixed ones in hex; a length-delimited field as text when it reads
/// as text, as a message when it parses as one, else as escaped bytes. None for what
/// isn't protobuf.
pub fn protobuf(bytes: &[u8]) -> Option<String> {
    let mut out = String::new();
    fields(bytes, 0, &mut out)?;
    Some(out)
}

fn fields(mut b: &[u8], depth: usize, out: &mut String) -> Option<()> {
    let indent = "  ".repeat(depth);
    while !b.is_empty() {
        let key = varint(&mut b)?;
        let field = key >> 3;
        if !(1..1 << 29).contains(&field) {
            return None;
        }
        let _ = match key & 7 {
            0 => match varint(&mut b)? {
                // int32/int64's negatives go as ten bytes.
                v if v > i64::MAX as u64 => writeln!(out, "{indent}{field}: {v} ({})", v as i64),
                v => writeln!(out, "{indent}{field}: {v}"),
            },
            1 => {
                let (v, rest) = b.split_first_chunk::<8>()?;
                b = rest;
                writeln!(out, "{indent}{field}: 0x{:016x}", u64::from_le_bytes(*v))
            }
            5 => {
                let (v, rest) = b.split_first_chunk::<4>()?;
                b = rest;
                writeln!(out, "{indent}{field}: 0x{:08x}", u32::from_le_bytes(*v))
            }
            2 => {
                let len = usize::try_from(varint(&mut b)?).ok()?;
                let (v, rest) = b.split_at_checked(len)?;
                b = rest;
                // Text first: a string often parses as a message too.
                let mut nested = String::new();
                if let Some(text) = printable(v) {
                    writeln!(out, "{indent}{field}: {text:?}")
                } else if depth < 32 && fields(v, depth + 1, &mut nested).is_some() {
                    let _ = writeln!(out, "{indent}{field} {{");
                    out.push_str(&nested);
                    writeln!(out, "{indent}}}")
                } else {
                    writeln!(out, "{indent}{field}: \"{}\"", escaped(v))
                }
            }
            // Groups, long gone from proto3, and what isn't protobuf at all.
            _ => return None,
        };
    }
    Some(())
}

fn varint(b: &mut &[u8]) -> Option<u64> {
    let mut v = 0;
    for i in 0..10 {
        let (&byte, rest) = b.split_first()?;
        *b = rest;
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Some(v);
        }
    }
    None
}

fn escaped(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            b'"' | b'\\' => _ = write!(out, "\\{}", b as char),
            0x20..=0x7e => out.push(b as char),
            _ => _ = write!(out, "\\x{b:02x}"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What grpcb.in's reflection was asked and the capture showed as a blank line: the
    /// NULs of the prefix and an empty string field read as UTF-8, and copying stopped at
    /// the first NUL. Framed and parsed, it says field 7 (list_services) is empty.
    #[test]
    fn a_grpc_body_reads_as_its_messages_fields() {
        let body = [0, 0, 0, 0, 2, 0x3a, 0];
        assert!(printable(&body).is_none(), "NULs aren't text");
        let messages = grpc_messages(&body, "").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(protobuf(&messages[0].data).unwrap(), "7: \"\"\n");

        // Two messages, one nested and one gzip-compressed, then gRPC-Web's trailers.
        let nested = [0x08, 0x96, 0x01, 0x12, 0x05, 0x0a, 0x03, b'a', b'b', b'c'];
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &[0x1a, 0x02, b'h', b'i']).unwrap();
        let gz = gz.finish().unwrap();
        let trailers = b"grpc-status: 0\r\n";
        let frame = |flag: u8, data: &[u8]| {
            [&[flag][..], &(data.len() as u32).to_be_bytes(), data].concat()
        };
        let body = [frame(0, &nested), frame(1, &gz), frame(0x80, trailers)].concat();
        let messages = grpc_messages(&body, "gzip").unwrap();
        assert_eq!(
            protobuf(&messages[0].data).unwrap(),
            "1: 150\n2 {\n  1: \"abc\"\n}\n"
        );
        assert!(messages[1].compressed);
        assert_eq!(protobuf(&messages[1].data).unwrap(), "3: \"hi\"\n");
        assert!(messages[2].trailers && messages[2].data == trailers);
        // Cut inside the last prefix: what came whole is kept.
        assert_eq!(
            grpc_messages(&body[..body.len() - 20], "gzip")
                .unwrap()
                .len(),
            2
        );
    }

    /// What isn't protobuf or gRPC says so, rather than showing fields made up of noise:
    /// then the capture falls back to a hex dump.
    #[test]
    fn what_isnt_protobuf_or_grpc_is_told_apart() {
        assert_eq!(grpc_messages(b"{\"a\":1}", ""), None);
        assert_eq!(protobuf(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a]), None);
        assert_eq!(protobuf(&[0x0a, 0x05, b'a']), None, "shorter than it says");
        // Fixed-width fields in hex, a negative int64 as both, bytes escaped.
        let fixed = [
            &[0x09][..],
            &1u64.to_le_bytes(),
            &[0x15, 2, 0, 0, 0],
            &[
                0x18, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
            ],
            &[0x22, 3, 0xff, b'"', 0],
        ]
        .concat();
        assert_eq!(
            protobuf(&fixed).unwrap(),
            "1: 0x0000000000000001\n2: 0x00000002\n3: 18446744073709551615 (-1)\n4: \"\\xff\\\"\\x00\"\n"
        );
    }

    /// A hex dump as `hexdump -C` prints it, a short last line padded so the text column
    /// lines up; a NUL never reaches the text.
    #[test]
    fn a_hex_dump_lines_up() {
        let dump = hexdump(b"Hello, world!\0\x01\x02xyz");
        assert_eq!(
            dump,
            "00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 00 01 02  |Hello, world!...|\n\
             00000010  78 79 7a                                          |xyz|\n"
        );
        assert_eq!(lossy(b"a\0b\xffc\n"), "a\u{fffd}b\u{fffd}c\n");
        assert_eq!(printable("中文".as_bytes()), Some("中文"));
        assert_eq!(
            printable(&"中文".as_bytes()[..4]),
            Some("中"),
            "cut mid-character"
        );
    }
}
