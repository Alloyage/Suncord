//! Send-side estimation from transport-wide feedback, a small GoogCC.
//! Transport feedback estimates the uplink. REMB also limits this single
//! video layer for receivers beyond Discord's forwarding server.
use super::rtp::TransportFeedback;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Discord's Go Live minimum.
const MINIMUM: u32 = 500_000;
const START: u32 = 2_500_000;
const RECEIVER_AGE: Duration = Duration::from_secs(5);
const HISTORY: Duration = Duration::from_secs(2);
/// Smoothed queueing delay that means overuse, and below which rate may grow.
const OVERUSE_MS: f64 = 50.0;
const NORMAL_MS: f64 = 15.0;

struct Sent {
    sequence: u16,
    at: Instant,
    bytes: u32,
    cluster: Option<u16>,
    arrival: Option<i64>,
    reported: bool,
}

pub(super) struct Rate {
    maximum: u32,
    /// Media Sink Wants limit.
    cap: u32,
    estimate: f64,
    reported: u32,
    receiver: Option<(u32, Instant)>,
    sent: VecDeque<Sent>,
    origin: Instant,
    reference: Option<(u32, i64)>,
    count: Option<u8>,
    floor: [i64; 2],
    floor_since: Instant,
    delay: f64,
    loss: f64,
    started: Instant,
    feedback: bool,
    increased: Instant,
    decreased: Instant,
    cluster: u16,
    probe: Option<Instant>,
    next_probe: Instant,
}

impl Rate {
    pub fn new(max_bitrate: u32, now: Instant) -> Self {
        let maximum = max_bitrate.max(MINIMUM) / 1000 * 1000;
        let start = START.min(maximum);
        Self {
            maximum,
            cap: maximum,
            estimate: f64::from(start),
            reported: start,
            receiver: None,
            sent: VecDeque::new(),
            origin: now,
            reference: None,
            count: None,
            floor: [i64::MAX; 2],
            floor_since: now,
            delay: 0.0,
            loss: 0.0,
            started: now,
            feedback: false,
            increased: now,
            decreased: now,
            cluster: 0,
            probe: None,
            next_probe: now,
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.reported
    }

    pub fn receiver_bitrate(&self, now: Instant) -> Option<u32> {
        self.receiver
            .filter(|(_, at)| now.saturating_duration_since(*at) < RECEIVER_AGE)
            .map(|(bitrate, _)| bitrate)
    }

    fn ceiling(&self, now: Instant) -> u32 {
        let ceiling = self.maximum.min(self.cap);
        match self.receiver_bitrate(now) {
            Some(receiver) => ceiling.min(receiver),
            _ => ceiling,
        }
    }

    /// Reports changes of at least 5 percent, and any change at the ceiling.
    fn apply(&mut self, now: Instant) -> Option<u32> {
        let ceiling = self.ceiling(now);
        // Without transport feedback, follow REMB as before.
        if !self.feedback
            && self.receiver_bitrate(now).is_some()
            && now.saturating_duration_since(self.started) >= Duration::from_secs(3)
        {
            self.estimate = f64::from(ceiling);
        }
        self.estimate = self
            .estimate
            .min(f64::from(ceiling))
            .max(f64::from(MINIMUM.min(ceiling)));
        let next = self.estimate as u32 / 1000 * 1000;
        let changed = next.abs_diff(self.reported) * 20 >= self.reported
            || (next != self.reported && next == ceiling / 1000 * 1000);
        changed.then(|| {
            self.reported = next;
            next
        })
    }

    pub fn receiver(&mut self, bitrate: u32, now: Instant) -> Option<u32> {
        self.receiver = Some((bitrate, now));
        self.apply(now)
    }

    /// Caps the rate for a Media Sink Wants request.
    pub fn set_cap(&mut self, cap: u32, now: Instant) -> Option<u32> {
        self.cap = cap.clamp(MINIMUM.min(self.maximum), self.maximum);
        self.apply(now)
    }

    pub fn sent(&mut self, sequence: u16, bytes: usize, probe: bool, now: Instant) {
        while self.sent.front().is_some_and(|entry| {
            now.saturating_duration_since(entry.at) > HISTORY || self.sent.len() >= 8192
        }) {
            self.sent.pop_front();
        }
        // Feedback lookup relies on consecutive sequence numbers.
        if self
            .sent
            .back()
            .is_some_and(|last| last.sequence.wrapping_add(1) != sequence)
        {
            self.sent.clear();
        }
        self.sent.push_back(Sent {
            sequence,
            at: now,
            bytes: bytes as u32,
            cluster: (probe && self.probe.is_some()).then_some(self.cluster),
            arrival: None,
            reported: false,
        });
    }

    fn index(&self, sequence: u16) -> Option<usize> {
        let index = usize::from(sequence.wrapping_sub(self.sent.front()?.sequence));
        (index < self.sent.len()).then_some(index)
    }

    pub fn feedback(&mut self, feedback: &TransportFeedback, now: Instant) -> Option<u32> {
        if self.count == Some(feedback.count) {
            return None;
        }
        self.count = Some(feedback.count);
        let reference = match self.reference {
            // Sign-extend the 24-bit difference.
            Some((raw, value)) => {
                value + i64::from((feedback.reference.wrapping_sub(raw) << 8) as i32 >> 8)
            }
            None => i64::from(feedback.reference),
        };
        self.reference = Some((feedback.reference, reference));
        if now.saturating_duration_since(self.floor_since) >= Duration::from_secs(5) {
            self.floor = [self.floor[1], i64::MAX];
            self.floor_since = now;
        }
        let mut delays = Vec::new();
        for &(sequence, arrival) in &feedback.packets {
            let Some(index) = self.index(sequence) else {
                continue;
            };
            let entry = &mut self.sent[index];
            entry.reported = true;
            if let Some(offset) = arrival {
                let arrival = reference * 64_000 + offset;
                entry.arrival = Some(arrival);
                let delay = arrival - entry.at.duration_since(self.origin).as_micros() as i64;
                self.floor[1] = self.floor[1].min(delay);
                delays.push(delay);
            }
        }
        if delays.is_empty() {
            return None;
        }
        self.feedback = true;
        // Queueing delay above the lowest recent one-way delay, which hides clock offset.
        let floor = self.floor[0].min(self.floor[1]);
        let queue =
            delays.iter().map(|d| (d - floor) as f64).sum::<f64>() / delays.len() as f64 / 1000.0;
        self.delay = self.delay * 0.6 + queue * 0.4;
        let (lost, total) = self
            .sent
            .iter()
            .filter(|e| e.reported && now.saturating_duration_since(e.at) < Duration::from_secs(1))
            .fold((0, 0), |(lost, total), e| {
                (lost + usize::from(e.arrival.is_none()), total + 1)
            });
        self.loss = if total >= 20 {
            lost as f64 / total as f64
        } else {
            0.0
        };
        self.evaluate_probe(now);
        let acked = self.acked();
        let estimate = self.estimate;
        if self.delay > OVERUSE_MS || self.loss > 0.1 {
            if now.saturating_duration_since(self.decreased) >= Duration::from_millis(300) {
                let delay_based = acked.unwrap_or(estimate).min(estimate) * 0.85;
                let loss_based = estimate * (1.0 - 0.5 * self.loss);
                self.estimate = if self.delay > OVERUSE_MS {
                    delay_based.min(loss_based)
                } else {
                    loss_based
                };
                self.decreased = now;
                self.next_probe = self.next_probe.max(now + Duration::from_secs(5));
            }
        } else if self.delay < NORMAL_MS && self.loss < 0.02 {
            let elapsed = now
                .saturating_duration_since(self.increased)
                .min(Duration::from_secs(1));
            let mut next = estimate * 1.08f64.powf(elapsed.as_secs_f64());
            // An encoder well below its target is no evidence of more capacity.
            if let Some(acked) = acked {
                next = next.min((acked * 1.5 + 100_000.0).max(estimate));
            }
            self.estimate = next;
        }
        self.increased = now;
        self.apply(now)
    }

    /// Received bitrate over the latest half second of arrivals.
    fn acked(&self) -> Option<f64> {
        let newest = self.sent.iter().filter_map(|e| e.arrival).max()?;
        let (bytes, oldest) = self
            .sent
            .iter()
            .filter_map(|e| {
                e.arrival
                    .filter(|a| *a >= newest - 500_000)
                    .map(|a| (e.bytes, a))
            })
            .fold((0u64, newest), |(bytes, oldest), (b, a)| {
                (bytes + u64::from(b), oldest.min(a))
            });
        let span = newest - oldest;
        (span >= 100_000).then(|| bytes as f64 * 8e6 / span as f64)
    }

    /// Requests a probe cluster, returning the extra bitrate to add briefly.
    pub fn probe(&mut self, now: Instant) -> Option<u32> {
        let ceiling = f64::from(self.ceiling(now));
        if !self.feedback
            || self.probe.is_some()
            || now < self.next_probe
            || self.delay > NORMAL_MS
            || self.loss >= 0.02
            || self.estimate * 1.2 > ceiling
        {
            return None;
        }
        self.cluster = self.cluster.wrapping_add(1);
        self.probe = Some(now);
        self.next_probe = now + Duration::from_secs(1);
        Some(((self.estimate * 2.0).min(ceiling) - self.estimate) as u32)
    }

    /// Adopts a probe's delivered rate, the minimum of its send and receive rates.
    fn evaluate_probe(&mut self, now: Instant) {
        let Some(started) = self.probe else {
            return;
        };
        let cluster = Some(self.cluster);
        let packets: Vec<_> = self.sent.iter().filter(|e| e.cluster == cluster).collect();
        let finished = self.sent.back().is_some_and(|e| e.cluster != cluster)
            && packets.iter().all(|e| e.reported);
        if !finished && now.saturating_duration_since(started) < Duration::from_secs(1) {
            return;
        }
        self.probe = None;
        let received: Vec<_> = packets.iter().filter(|e| e.arrival.is_some()).collect();
        let result = (|| {
            if received.len() < 5 || received.len() * 5 < packets.len() * 4 {
                return None;
            }
            let first = packets.first()?;
            let last = packets.last()?;
            let send_span = last.at.duration_since(first.at).as_micros() as f64;
            let send_bytes =
                packets.iter().map(|e| f64::from(e.bytes)).sum::<f64>() - f64::from(last.bytes);
            let earliest = received.iter().min_by_key(|e| e.arrival)?;
            let latest = received.iter().filter_map(|e| e.arrival).max()?;
            let receive_span = (latest - earliest.arrival?) as f64;
            let receive_bytes = received.iter().map(|e| f64::from(e.bytes)).sum::<f64>()
                - f64::from(earliest.bytes);
            if send_span < 1000.0 || receive_span < 1000.0 {
                return None;
            }
            let send = send_bytes * 8e6 / send_span;
            let receive = receive_bytes * 8e6 / receive_span;
            Some(if receive < send * 0.9 {
                receive * 0.95
            } else {
                send.min(receive)
            })
        })();
        match result {
            Some(rate) if rate > self.estimate * 1.1 => self.estimate = rate,
            _ => self.next_probe = now + Duration::from_secs(10),
        }
    }

    /// Forgets per-packet state, keeping the estimate.
    pub fn clear(&mut self) {
        self.sent.clear();
        self.probe = None;
    }
}
