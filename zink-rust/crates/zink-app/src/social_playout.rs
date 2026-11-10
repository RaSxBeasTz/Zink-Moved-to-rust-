use std::time::{Duration, Instant};

// About one frame at 30 FPS or two at 60 FPS. Bound the wait even when
// the sender clock drifts, restarts, or a relay delivers frames in bursts.
const JITTER_BUFFER: Duration = Duration::from_millis(35);
const AV1_720P_JITTER_BUFFER: Duration = Duration::from_millis(95);
const MAX_SOURCE_GAP_MS: i64 = 200;

pub(super) struct PlayoutClock {
    previous: Option<(i64, Instant)>,
    buffer: Duration,
}

impl Default for PlayoutClock {
    fn default() -> Self {
        Self::new(false)
    }
}

impl PlayoutClock {
    pub(super) fn new(av1_720p: bool) -> Self {
        Self {
            previous: None,
            buffer: if av1_720p {
                AV1_720P_JITTER_BUFFER
            } else {
                JITTER_BUFFER
            },
        }
    }

    pub(super) fn reset(&mut self) {
        self.previous = None;
    }

    pub(super) fn schedule(
        &mut self,
        timestamp_ms: Option<i64>,
        received: Instant,
        now: Instant,
    ) -> (Instant, Duration) {
        let fallback = Duration::from_secs_f64(1.0 / 30.0);
        let Some(timestamp) = timestamp_ms.filter(|timestamp| *timestamp >= 0) else {
            self.previous = None;
            return (now, fallback);
        };
        let Some((last_timestamp, last_deadline)) = self.previous else {
            let deadline = received + self.buffer;
            self.previous = Some((timestamp, deadline));
            return (deadline, fallback);
        };
        let step = timestamp.saturating_sub(last_timestamp);
        if !(1..=MAX_SOURCE_GAP_MS).contains(&step) {
            // Some peers do not populate reliable media timestamps.
            self.previous = Some((timestamp, now));
            return (now, fallback);
        }

        let step = Duration::from_millis(step as u64);
        let deadline = (last_deadline + step).min(received + self.buffer);
        let deadline = if now.saturating_duration_since(deadline) > JITTER_BUFFER {
            // Recover immediately after a long stall; never replay a delayed
            // backlog at real-time speed while the viewer falls further behind.
            now
        } else {
            deadline
        };
        self.previous = Some((timestamp, deadline));
        (deadline, step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursty_delivery_keeps_the_source_cadence_at_both_frame_rates() {
        let start = Instant::now();
        for step in [16, 33] {
            let mut clock = PlayoutClock::default();
            let (first, _) = clock.schedule(Some(0), start, start);
            // A slow first packet is followed immediately by another packet.
            let burst = start + Duration::from_millis(step * 2 + 1);
            let (second, duration) = clock.schedule(Some(step as i64), burst, burst);
            let (third, _) = clock.schedule(Some(2 * step as i64), burst, second.max(burst));
            assert_eq!(second - first, Duration::from_millis(step));
            assert_eq!(third - second, duration);
            assert!(third <= burst + JITTER_BUFFER);
        }
    }

    #[test]
    fn long_stalls_and_old_peers_do_not_accumulate_delay() {
        let mut clock = PlayoutClock::default();
        let start = Instant::now();
        clock.schedule(Some(0), start, start);
        let late = start + Duration::from_millis(300);
        assert_eq!(clock.schedule(Some(33), late, late).0, late);
        assert_eq!(clock.schedule(Some(33), late, late).0, late);
        assert_eq!(clock.schedule(None, late, late).0, late);
    }

    #[test]
    fn av1_720p_absorbs_a_late_frame_and_its_decode_time() {
        let mut clock = PlayoutClock::new(true);
        let start = Instant::now();
        let (first, _) = clock.schedule(Some(0), start, start);
        let arrival = start + Duration::from_millis(104);
        let (second, _) = clock.schedule(Some(32), arrival, arrival);
        assert_eq!(second - first, Duration::from_millis(32));
        assert!(second >= arrival + Duration::from_millis(15));
        clock.reset();
        assert_eq!(
            clock.schedule(Some(0), arrival, arrival).0,
            arrival + AV1_720P_JITTER_BUFFER
        );
    }
}
