//! Minimal AMF0 encoding for the RTMP command messages we send.
//!
//! We only produce the types the publish handshake needs: number, boolean,
//! string, object (ordered key/values), and null. AMF0 decoding of server
//! replies is intentionally minimal — we only need to spot the "_result" for
//! `connect`/`createStream` and read the returned stream id.

/// AMF0 type markers.
const MARKER_NUMBER: u8 = 0x00;
const MARKER_BOOLEAN: u8 = 0x01;
const MARKER_STRING: u8 = 0x02;
const MARKER_OBJECT: u8 = 0x03;
const MARKER_NULL: u8 = 0x05;
const MARKER_OBJECT_END: u8 = 0x09;

pub enum Amf0 {
    Number(f64),
    Boolean(bool),
    String(String),
    /// Ordered key/value object properties.
    Object(Vec<(String, Amf0)>),
    Null,
}

impl Amf0 {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Amf0::Number(n) => {
                out.push(MARKER_NUMBER);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Amf0::Boolean(b) => {
                out.push(MARKER_BOOLEAN);
                out.push(if *b { 1 } else { 0 });
            }
            Amf0::String(s) => {
                out.push(MARKER_STRING);
                encode_string_body(s, out);
            }
            Amf0::Object(props) => {
                out.push(MARKER_OBJECT);
                for (k, v) in props {
                    // Keys are UTF-8 with a u16 length prefix, no type marker.
                    encode_string_body(k, out);
                    v.encode(out);
                }
                // Object end: empty key + end marker.
                out.extend_from_slice(&[0x00, 0x00, MARKER_OBJECT_END]);
            }
            Amf0::Null => out.push(MARKER_NULL),
        }
    }
}

fn encode_string_body(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Encode a full command payload: a sequence of AMF0 values concatenated.
pub fn encode_values(values: &[Amf0]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        v.encode(&mut out);
    }
    out
}

/// Very small decoder: scan a command message for the top-level command name
/// (first AMF0 string) and, if present, a transaction id (first number). Enough
/// to recognize "_result" / "_error" and read createStream's returned id.
pub struct Amf0CommandReply {
    pub command: String,
    pub transaction_id: f64,
    /// The last AMF0 number in the message, used as the stream id for
    /// createStream's _result (its 4th value).
    pub last_number: Option<f64>,
}

pub fn parse_command_reply(data: &[u8]) -> Option<Amf0CommandReply> {
    let mut r = Reader { data, pos: 0 };
    let command = match r.read_value()? {
        Value::String(s) => s,
        _ => return None,
    };
    let transaction_id = match r.read_value()? {
        Value::Number(n) => n,
        _ => 0.0,
    };
    // Scan the rest for the last number seen.
    let mut last_number = None;
    while let Some(v) = r.read_value() {
        if let Value::Number(n) = v {
            last_number = Some(n);
        }
    }
    Some(Amf0CommandReply {
        command,
        transaction_id,
        last_number,
    })
}

enum Value {
    Number(f64),
    // We only need to consume booleans, not read them, in server replies.
    Boolean,
    String(String),
    Object,
    Null,
    Unsupported,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn string_body(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        let b = self.take(len)?;
        Some(String::from_utf8_lossy(b).into_owned())
    }
    fn read_value(&mut self) -> Option<Value> {
        let marker = self.u8()?;
        match marker {
            MARKER_NUMBER => {
                let b = self.take(8)?;
                let mut arr = [0u8; 8];
                arr.copy_from_slice(b);
                Some(Value::Number(f64::from_be_bytes(arr)))
            }
            MARKER_BOOLEAN => {
                self.u8()?;
                Some(Value::Boolean)
            }
            MARKER_STRING => Some(Value::String(self.string_body()?)),
            MARKER_OBJECT => {
                // Consume properties until the object-end marker.
                loop {
                    let key_len = self.u16()?;
                    if key_len == 0 {
                        // Next byte should be the end marker.
                        let end = self.u8()?;
                        if end == MARKER_OBJECT_END {
                            break;
                        }
                        // Malformed; stop.
                        return Some(Value::Object);
                    }
                    let _ = self.take(key_len as usize)?;
                    let _ = self.read_value()?;
                }
                Some(Value::Object)
            }
            MARKER_NULL => Some(Value::Null),
            _ => Some(Value::Unsupported),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_connect_command() {
        // Encode a `connect` command and parse its name + txn id back out.
        let vals = [
            Amf0::String("connect".into()),
            Amf0::Number(1.0),
            Amf0::Object(vec![
                ("app".into(), Amf0::String("live".into())),
                ("type".into(), Amf0::String("nonprivate".into())),
            ]),
        ];
        let bytes = encode_values(&vals);
        let reply = parse_command_reply(&bytes).unwrap();
        assert_eq!(reply.command, "connect");
        assert_eq!(reply.transaction_id, 1.0);
    }

    #[test]
    fn parses_create_stream_result_stream_id() {
        // _result, txn=2, null props, stream id=1.0
        let vals = [
            Amf0::String("_result".into()),
            Amf0::Number(2.0),
            Amf0::Null,
            Amf0::Number(1.0),
        ];
        let bytes = encode_values(&vals);
        let reply = parse_command_reply(&bytes).unwrap();
        assert_eq!(reply.command, "_result");
        assert_eq!(reply.transaction_id, 2.0);
        assert_eq!(reply.last_number, Some(1.0));
    }

    #[test]
    fn number_encoding_is_be_f64() {
        let mut out = Vec::new();
        Amf0::Number(1.0).encode(&mut out);
        assert_eq!(out[0], MARKER_NUMBER);
        assert_eq!(&out[1..9], &1.0f64.to_be_bytes());
    }
}
