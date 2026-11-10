use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use rtc::rtp::{codec::av1::Av1Depacketizer, packet::Packet, packetizer::Depacketizer};

const MAX_PACKETS: u16 = 1024;
const REPAIR_WINDOW: Duration = Duration::from_millis(85);
const MAX_PICTURE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Av1RtpFrames {
    next: Option<u16>,
    pending: HashMap<u16, Packet>,
    gap_since: Option<Instant>,
    frame: Option<(u32, Vec<u8>)>,
    depacketizer: Av1Depacketizer,
    picture_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Packet {
        let mut packet = Packet::default();
        packet.header.sequence_number = seq;
        packet.header.timestamp = timestamp;
        packet.header.marker = marker;
        packet.payload = payload.to_vec().into();
        packet
    }

    #[test]
    fn completes_before_the_next_frame_and_repairs_reordering_across_wrap() {
        let mut assembler = Av1RtpFrames::default();
        let now = Instant::now();
        let mut output = Vec::new();
        assembler.push(now, packet(65534, 0, true, &[0x10, 0x30, 7]), |item| {
            output.push(item);
        });
        assert_eq!(output, vec![Some((0, vec![0x32, 1, 7]))]);
        assembler.push(now, packet(0, 3000, true, &[0x90, 9]), |_| {
            panic!("the final fragment arrived before its first fragment");
        });
        assembler.push(now, packet(65535, 3000, false, &[0x50, 0x30, 8]), |item| {
            output.push(item);
        });
        assert_eq!(output[1], Some((3000, vec![0x32, 2, 8, 9])));
        assert_eq!(output.len(), 2);
    }

    #[test]
    fn reports_a_missing_packet_before_delivering_the_next_complete_picture() {
        let mut assembler = Av1RtpFrames::default();
        let now = Instant::now();
        assembler.push(now, packet(10, 0, false, &[0x50, 0x30, 7]), |_| {
            panic!("a partial picture cannot be delivered");
        });
        assembler.push(now, packet(12, 3000, true, &[0x10, 0x30, 8]), |_| {
            panic!("the missing packet is still eligible for repair");
        });
        let mut output = Vec::new();
        assembler.push(
            now + REPAIR_WINDOW,
            packet(13, 6000, true, &[0x10, 0x30, 9]),
            |item| {
                output.push(item);
            },
        );
        assert_eq!(
            output,
            vec![
                None,
                Some((3000, vec![0x32, 1, 8])),
                Some((6000, vec![0x32, 1, 9]))
            ]
        );
    }
}

impl Av1RtpFrames {
    // Zink sets the marker on the last packet of every encoded picture.
    // Complete pictures can go straight to decode without looking ahead.
    pub(super) fn push(
        &mut self,
        now: Instant,
        packet: Packet,
        mut emit: impl FnMut(Option<(u32, Vec<u8>)>),
    ) {
        let seq = packet.header.sequence_number;
        let next = *self.next.get_or_insert(seq);
        let distance = seq.wrapping_sub(next);
        if distance >= 0x8000 {
            return;
        }
        if distance >= MAX_PACKETS {
            self.pending.clear();
            self.next = Some(seq);
            self.gap_since = None;
            self.discard_frame();
            emit(None);
        }
        self.pending.entry(seq).or_insert(packet);

        loop {
            let next = self.next.expect("sequence was initialized on receipt");
            if let Some(packet) = self.pending.remove(&next) {
                self.next = Some(next.wrapping_add(1));
                self.gap_since = None;
                self.consume(packet, &mut emit);
            } else if !self.pending.is_empty()
                && now.duration_since(*self.gap_since.get_or_insert(now)) >= REPAIR_WINDOW
            {
                self.next = self
                    .pending
                    .keys()
                    .copied()
                    .min_by_key(|seq| seq.wrapping_sub(next));
                self.discard_frame();
                emit(None);
            } else {
                break;
            }
        }
    }

    fn consume(&mut self, packet: Packet, emit: &mut impl FnMut(Option<(u32, Vec<u8>)>)) {
        let timestamp = packet.header.timestamp;
        if self.frame.as_ref().is_some_and(|(ts, _)| *ts != timestamp) {
            self.discard_frame();
            emit(None);
        }
        if self.frame.is_none() && !self.depacketizer.is_partition_head(&packet.payload) {
            emit(None);
            return;
        }
        self.picture_bytes += packet.payload.len();
        if self.picture_bytes > MAX_PICTURE_BYTES {
            self.discard_frame();
            emit(None);
            return;
        }
        let Ok(payload) = self.depacketizer.depacketize(&packet.payload) else {
            self.discard_frame();
            emit(None);
            return;
        };
        let (_, frame) = self.frame.get_or_insert_with(|| (timestamp, Vec::new()));
        frame.extend_from_slice(&payload);
        if packet.header.marker && self.depacketizer.y {
            self.discard_frame();
            emit(None);
        } else if packet.header.marker
            && let Some((timestamp, frame)) = self.frame.take()
        {
            self.picture_bytes = 0;
            emit(Some((timestamp, frame)));
        }
    }

    fn discard_frame(&mut self) {
        self.frame = None;
        self.depacketizer = Av1Depacketizer::default();
        self.picture_bytes = 0;
    }
}
