//! RTMP chunk stream serialization (write side).
//!
//! We only need to *send* chunks. A message (command / audio / video) is split
//! into chunks of at most `chunk_size` bytes: the first chunk carries a full
//! type-0 header, continuation chunks carry a type-3 header (just the basic
//! header byte). Extended timestamps are emitted when the timestamp >= 0xFFFFFF.

/// RTMP message type ids we use.
pub mod msg_type {
    pub const SET_CHUNK_SIZE: u8 = 1;
    pub const ABORT: u8 = 2;
    pub const ACK: u8 = 3;
    pub const USER_CONTROL: u8 = 4;
    pub const WINDOW_ACK_SIZE: u8 = 5;
    pub const SET_PEER_BANDWIDTH: u8 = 6;
    pub const AUDIO: u8 = 8;
    pub const VIDEO: u8 = 9;
    pub const DATA_AMF0: u8 = 18;
    pub const COMMAND_AMF0: u8 = 20;
}

/// A message to be chunked and written.
pub struct Message {
    pub chunk_stream_id: u32,
    pub timestamp: u32,
    pub type_id: u8,
    pub msg_stream_id: u32,
    pub payload: Vec<u8>,
}

/// Serialize a message into one or more chunks, appending to `out`.
/// Continuation chunks reuse the chunk stream id with a type-3 header.
pub fn write_message(out: &mut Vec<u8>, msg: &Message, chunk_size: usize) {
    let ext_ts = msg.timestamp >= 0xFF_FFFF;
    let ts_field = if ext_ts { 0xFF_FFFF } else { msg.timestamp };

    // --- first chunk: type-0 header ---
    write_basic_header(out, 0, msg.chunk_stream_id);
    // message header (type 0): timestamp(3) | length(3) | type(1) | stream id(4, LE)
    out.extend_from_slice(&ts_field.to_be_bytes()[1..4]); // 3-byte BE
    let len = msg.payload.len() as u32;
    out.extend_from_slice(&len.to_be_bytes()[1..4]);
    out.push(msg.type_id);
    out.extend_from_slice(&msg.msg_stream_id.to_le_bytes()); // stream id is LE
    if ext_ts {
        out.extend_from_slice(&msg.timestamp.to_be_bytes());
    }

    let mut written = 0;
    let first = msg.payload.len().min(chunk_size);
    out.extend_from_slice(&msg.payload[..first]);
    written += first;

    // --- continuation chunks: type-3 header ---
    while written < msg.payload.len() {
        write_basic_header(out, 3, msg.chunk_stream_id);
        if ext_ts {
            // Extended timestamp is repeated on type-3 continuations.
            out.extend_from_slice(&msg.timestamp.to_be_bytes());
        }
        let take = (msg.payload.len() - written).min(chunk_size);
        out.extend_from_slice(&msg.payload[written..written + take]);
        written += take;
    }
}

/// Basic header: 2-bit fmt + chunk stream id, with 1/2/3-byte encodings.
fn write_basic_header(out: &mut Vec<u8>, fmt: u8, csid: u32) {
    let fmt_bits = (fmt & 0x03) << 6;
    if csid < 64 {
        out.push(fmt_bits | (csid as u8));
    } else if csid < 320 {
        out.push(fmt_bits); // csid = 0 signals 2-byte form
        out.push((csid - 64) as u8);
    } else {
        out.push(fmt_bits | 1); // csid = 1 signals 3-byte form
        let v = (csid - 64) as u16;
        out.push((v & 0xff) as u8);
        out.push((v >> 8) as u8);
    }
}

/// Build a Set Chunk Size control message payload (4-byte BE).
pub fn set_chunk_size_payload(size: u32) -> Vec<u8> {
    (size & 0x7fff_ffff).to_be_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_message_single_chunk() {
        let msg = Message {
            chunk_stream_id: 3,
            timestamp: 0,
            type_id: msg_type::COMMAND_AMF0,
            msg_stream_id: 0,
            payload: vec![0xAA, 0xBB, 0xCC],
        };
        let mut out = Vec::new();
        write_message(&mut out, &msg, 128);
        // basic header (1) + msg header (11) + payload (3)
        assert_eq!(out.len(), 1 + 11 + 3);
        assert_eq!(out[0], 3); // fmt 0, csid 3
        assert_eq!(out[7], msg_type::COMMAND_AMF0); // type id position
        assert_eq!(&out[12..15], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn large_message_splits_with_type3_continuation() {
        let payload = vec![0x11u8; 200];
        let msg = Message {
            chunk_stream_id: 4,
            timestamp: 0,
            type_id: msg_type::VIDEO,
            msg_stream_id: 1,
            payload,
        };
        let mut out = Vec::new();
        write_message(&mut out, &msg, 128);
        // first: 1 + 11 + 128 ; continuation: 1 + 72
        assert_eq!(out.len(), (1 + 11 + 128) + (1 + 72));
        // continuation basic header at offset 140: fmt 3, csid 4
        let cont = out[1 + 11 + 128];
        assert_eq!(cont, (3 << 6) | 4);
    }

    #[test]
    fn stream_id_is_little_endian() {
        let msg = Message {
            chunk_stream_id: 3,
            timestamp: 0,
            type_id: msg_type::VIDEO,
            msg_stream_id: 1,
            payload: vec![0x00],
        };
        let mut out = Vec::new();
        write_message(&mut out, &msg, 128);
        // stream id occupies the 4 bytes before payload: header is
        // basic(1)+ts(3)+len(3)+type(1)+streamid(4) => bytes 8..12
        assert_eq!(&out[8..12], &1u32.to_le_bytes());
    }
}
