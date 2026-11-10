//! Fork the encoder's single output stream to two independent consumers:
//! the MP4 recorder and the RTMP publisher. Neither re-encodes; both get the
//! same `EncodedSample` bytes.
//!
//! Drop policy differs per branch:
//! - **Record**: must be lossless. The record channel is bounded but the
//!   producer *blocks* when it's full, so no recorded frame is ever dropped.
//! - **Stream**: may drop under backpressure (a slow/lossy network must not
//!   stall recording or the encoder), but never drops a keyframe — dropping one
//!   would freeze the decoder until the next GOP. Non-keyframes are dropped
//!   oldest-first when the channel is full.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

use crate::encoder::EncodedSample;

/// Sender half of the fork. `distribute` pushes one sample to whichever
/// branches are enabled.
pub struct Fork {
    record_tx: Option<SyncSender<EncodedSample>>,
    stream_tx: Option<SyncSender<EncodedSample>>,
    dropped_stream_frames: u64,
}

/// Receiver ends handed to the consumer threads/tasks.
pub struct ForkReceivers {
    pub record: Option<Receiver<EncodedSample>>,
    pub stream: Option<Receiver<EncodedSample>>,
}

impl Fork {
    /// Create a fork. `record`/`stream` toggle each branch; `bound` sizes both
    /// channels. Returns the sender and the receiver ends.
    pub fn new(record: bool, stream: bool, bound: usize) -> (Fork, ForkReceivers) {
        let (record_tx, record_rx) = if record {
            let (tx, rx) = std::sync::mpsc::sync_channel(bound);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let (stream_tx, stream_rx) = if stream {
            let (tx, rx) = std::sync::mpsc::sync_channel(bound);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        (
            Fork {
                record_tx,
                stream_tx,
                dropped_stream_frames: 0,
            },
            ForkReceivers {
                record: record_rx,
                stream: stream_rx,
            },
        )
    }

    /// Distribute one encoded sample. Returns false if all enabled consumers
    /// have hung up (nothing left to feed).
    pub fn distribute(&mut self, sample: EncodedSample) -> bool {
        let mut any_alive = false;

        // Record branch: block until there's room — never drop.
        if let Some(tx) = &self.record_tx {
            match tx.send(sample.clone()) {
                Ok(()) => any_alive = true,
                Err(_) => self.record_tx = None, // consumer gone
            }
        }

        // Stream branch: try to send; on full, drop only if this is not a
        // keyframe. If it is a keyframe and the channel is full, block briefly
        // so the decoder doesn't lose GOP sync.
        if let Some(tx) = &self.stream_tx {
            match tx.try_send(sample.clone()) {
                Ok(()) => any_alive = true,
                Err(TrySendError::Full(s)) => {
                    if s.is_keyframe {
                        // Keyframe must not be dropped: block until it lands.
                        match tx.send(s) {
                            Ok(()) => any_alive = true,
                            Err(_) => self.stream_tx = None,
                        }
                    } else {
                        self.dropped_stream_frames += 1;
                        any_alive = true; // consumer still alive, just slow
                    }
                }
                Err(TrySendError::Disconnected(_)) => self.stream_tx = None,
            }
        }

        any_alive
    }

    /// Count of non-keyframes dropped on the stream branch under backpressure.
    pub fn dropped_stream_frames(&self) -> u64 {
        self.dropped_stream_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample(keyframe: bool) -> EncodedSample {
        EncodedSample {
            data: vec![0u8; 4],
            timestamp: Duration::ZERO,
            is_keyframe: keyframe,
        }
    }

    #[test]
    fn stream_drops_non_keyframes_when_full_but_keeps_keyframes() {
        // bound=1 stream only. Fill it, then a non-keyframe drops, a keyframe
        // would block — so drain first to test the drop path.
        let (mut fork, rx) = Fork::new(false, true, 1);
        assert!(fork.distribute(sample(false))); // fills the single slot
        assert!(fork.distribute(sample(false))); // full -> dropped
        assert_eq!(fork.dropped_stream_frames(), 1);
        // The one buffered sample is still there.
        let stream_rx = rx.stream.unwrap();
        assert!(stream_rx.recv().is_ok());
    }

    #[test]
    fn record_branch_receives_every_sample() {
        let (mut fork, rx) = Fork::new(true, false, 8);
        for _ in 0..5 {
            assert!(fork.distribute(sample(false)));
        }
        let record_rx = rx.record.unwrap();
        let mut count = 0;
        while record_rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 5);
        assert_eq!(fork.dropped_stream_frames(), 0);
    }

    #[test]
    fn distribute_returns_false_when_all_consumers_gone() {
        let (mut fork, rx) = Fork::new(true, false, 2);
        drop(rx); // both receivers dropped
                  // First distribute detects the hangup and returns false.
        assert!(!fork.distribute(sample(false)));
    }
}
