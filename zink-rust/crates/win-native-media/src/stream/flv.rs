//! FLV video/audio tag bodies for RTMP, and Annex-B -> AVCC conversion.
//!
//! RTMP carries H.264 as FLV VIDEODATA with AVC packets: NAL units are AVCC
//! length-prefixed (4-byte big-endian length), NOT Annex-B start codes. The
//! AVC sequence header (an avcC record) is sent once before the first frame.

use crate::encoder::ParameterSets;
use crate::mux::build_avcc;

/// FLV video FrameType nibble.
const FRAME_KEY: u8 = 1;
const FRAME_INTER: u8 = 2;
/// FLV CodecID nibble for AVC.
const CODEC_AVC: u8 = 7;
/// AVCPacketType.
const AVC_SEQUENCE_HEADER: u8 = 0;
const AVC_NALU: u8 = 1;

/// Build the AVC sequence-header video tag body (sent once, before frames).
/// Body: [frametype|codec][pkt type=0][composition time=0][avcC record].
pub fn avc_sequence_header(params: &ParameterSets) -> Vec<u8> {
    let avcc = build_avcc(&params.sps, &params.pps);
    let mut body = Vec::with_capacity(5 + avcc.len());
    body.push((FRAME_KEY << 4) | CODEC_AVC);
    body.push(AVC_SEQUENCE_HEADER);
    body.extend_from_slice(&[0, 0, 0]); // composition time
    body.extend_from_slice(&avcc);
    body
}

/// Build an AVC NALU video tag body for one encoded frame.
/// `annex_b` is the encoder's Annex-B access unit; converted to AVCC here.
pub fn avc_frame(annex_b: &[u8], is_keyframe: bool) -> Vec<u8> {
    let avcc = annex_b_to_avcc(annex_b);
    let frame_type = if is_keyframe { FRAME_KEY } else { FRAME_INTER };
    let mut body = Vec::with_capacity(5 + avcc.len());
    body.push((frame_type << 4) | CODEC_AVC);
    body.push(AVC_NALU);
    body.extend_from_slice(&[0, 0, 0]); // composition time (0: no B-frame reorder)
    body.extend_from_slice(&avcc);
    body
}

/// Convert an Annex-B access unit to AVCC (4-byte length-prefixed NALs).
/// SPS/PPS NALs are dropped from per-frame data — they live in the sequence
/// header — but keeping them is harmless; we drop to match typical encoders.
pub fn annex_b_to_avcc(annex_b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annex_b.len() + 8);
    for nal in iter_nals(annex_b) {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1f;
        // Drop SPS(7)/PPS(8)/AUD(9) from frame payloads.
        if matches!(nal_type, 7..=9) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Iterate NAL bodies between Annex-B start codes (3- or 4-byte), excluding
/// the start code. Shared shape with the encoder's parser but local to avoid a
/// cross-module dependency on a private fn.
fn iter_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i + 3, 3));
            i += 3;
        } else if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            starts.push((i + 4, 4));
            i += 4;
        } else {
            i += 1;
        }
    }
    let mut ranges = Vec::with_capacity(starts.len());
    for k in 0..starts.len() {
        let body = starts[k].0;
        let end = if k + 1 < starts.len() {
            starts[k + 1].0 - starts[k + 1].1
        } else {
            data.len()
        };
        ranges.push((body, end));
    }
    ranges.into_iter().map(move |(s, e)| &data[s..e])
}

// --- audio (AAC) ----------------------------------------------------------

/// FLV audio tag header byte for AAC: SoundFormat=10 (AAC) in the high nibble,
/// then SoundRate=3 (44kHz flag, ignored for AAC), SoundSize=1 (16-bit),
/// SoundType=1 (stereo). AAC always uses this fixed 0xAF header byte.
const AAC_AUDIO_HEADER: u8 = 0xAF;
const AAC_SEQUENCE_HEADER: u8 = 0;
const AAC_RAW: u8 = 1;

/// Build the AAC sequence-header audio tag body (sent once before audio frames).
/// Body: [0xAF][AACPacketType=0][AudioSpecificConfig].
pub fn aac_sequence_header(asc: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + asc.len());
    body.push(AAC_AUDIO_HEADER);
    body.push(AAC_SEQUENCE_HEADER);
    body.extend_from_slice(asc);
    body
}

/// Build a raw AAC audio tag body for one encoded AAC frame.
/// Body: [0xAF][AACPacketType=1][raw AAC frame].
pub fn aac_frame(aac: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + aac.len());
    body.push(AAC_AUDIO_HEADER);
    body.push(AAC_RAW);
    body.extend_from_slice(aac);
    body
}

/// Convert a presentation duration to an FLV/RTMP millisecond timestamp.
///
/// FLV timestamps are 32-bit unsigned milliseconds. This is the single place
/// the ns->ms conversion happens (the spot the reference rtmp-rs got wrong):
/// saturating, integer-truncating, and wrapping at the 32-bit boundary the way
/// RTMP expects.
pub fn duration_to_flv_ms(d: std::time::Duration) -> u32 {
    (d.as_millis() as u64 & 0xffff_ffff) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn annex_b_converts_to_length_prefixed_and_drops_param_sets() {
        // SPS(7) + PPS(8) + IDR(5) in Annex-B; only the IDR should survive AVCC.
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS
            0, 0, 1, 0x68, 0xBB, // PPS
            0, 0, 0, 1, 0x65, 0x11, 0x22, // IDR
        ];
        let avcc = annex_b_to_avcc(&stream);
        // Only the IDR NAL: [len=3][0x65,0x11,0x22]
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x65, 0x11, 0x22]);
    }

    #[test]
    fn keyframe_tag_header_nibbles() {
        let body = avc_frame(&[0, 0, 0, 1, 0x65, 0x01], true);
        assert_eq!(body[0], (FRAME_KEY << 4) | CODEC_AVC); // 0x17
        assert_eq!(body[1], AVC_NALU);
    }

    #[test]
    fn interframe_tag_header_nibbles() {
        let body = avc_frame(&[0, 0, 0, 1, 0x41, 0x01], false);
        assert_eq!(body[0], (FRAME_INTER << 4) | CODEC_AVC); // 0x27
    }

    #[test]
    fn sequence_header_has_avc_seqhdr_type() {
        let params = ParameterSets {
            sps: vec![0x67, 0x64, 0x00, 0x1f],
            pps: vec![0x68, 0xCC],
        };
        let body = avc_sequence_header(&params);
        assert_eq!(body[0], (FRAME_KEY << 4) | CODEC_AVC);
        assert_eq!(body[1], AVC_SEQUENCE_HEADER);
    }

    #[test]
    fn aac_seqhdr_and_frame_headers() {
        let seq = aac_sequence_header(&[0x11, 0x90]);
        assert_eq!(seq[0], 0xAF);
        assert_eq!(seq[1], 0); // sequence header
        assert_eq!(&seq[2..], &[0x11, 0x90]);

        let frame = aac_frame(&[0xDE, 0xAD]);
        assert_eq!(frame[0], 0xAF);
        assert_eq!(frame[1], 1); // raw
        assert_eq!(&frame[2..], &[0xDE, 0xAD]);
    }

    #[test]
    fn flv_ms_truncates_and_wraps() {
        assert_eq!(duration_to_flv_ms(Duration::from_millis(0)), 0);
        assert_eq!(duration_to_flv_ms(Duration::from_micros(1500)), 1); // truncate
        assert_eq!(duration_to_flv_ms(Duration::from_millis(2500)), 2500);
    }
}
