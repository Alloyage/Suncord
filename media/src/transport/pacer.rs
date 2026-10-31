use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const WIRE_OVERHEAD: usize = 80;
const MAX_PAYLOAD: usize = super::rtp::MTU - 54;
const MAX_QUEUE_BYTES: usize = 2 * 1024 * 1024;
const QUEUE_AGE: Duration = Duration::from_millis(250);
const HISTORY_AGE: Duration = Duration::from_millis(500);
const REPAIR_INTERVAL: Duration = Duration::from_millis(40);

pub(super) struct Packet {
    pub timestamp: u32,
    pub marker: bool,
    pub original_sequence: Option<u16>,
    pub payload: Vec<u8>,
    /// Sent while a probe cluster runs.
    pub probe: bool,
    /// A probe copy of an already delivered packet.
    pub duplicate: bool,
}

struct Frame {
    timestamp: u32,
    deadline: Instant,
    keyframe: bool,
    payloads: VecDeque<Vec<u8>>,
}

struct History {
    sequence: u16,
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
    sent: Instant,
    repairs: u8,
    last_repair: Option<Instant>,
}

struct Probe {
    budget: Budget,
    remaining: usize,
    next: usize,
}

struct Budget {
    rate: f64,
    capacity: f64,
    tokens: f64,
    updated: Instant,
}

impl Budget {
    fn new(rate: f64, now: Instant) -> Self {
        let capacity = (rate * 0.010).max(2.0 * (MAX_PAYLOAD + WIRE_OVERHEAD + 2) as f64);
        Self {
            rate,
            capacity,
            tokens: capacity,
            updated: now,
        }
    }

    fn available(&self, now: Instant) -> f64 {
        (self.tokens + now.saturating_duration_since(self.updated).as_secs_f64() * self.rate)
            .min(self.capacity)
    }

    fn ready(&self, bytes: usize, now: Instant) -> Instant {
        let missing = (bytes as f64 - self.available(now)).max(0.0);
        now + Duration::from_secs_f64(missing / self.rate)
    }

    fn spend(&mut self, bytes: usize, now: Instant) {
        self.tokens = self.available(now) - bytes as f64;
        self.updated = now;
    }

    fn set_rate(&mut self, rate: f64, now: Instant) {
        let available = self.available(now);
        *self = Self::new(rate, now);
        self.tokens = available.min(self.capacity);
    }
}

/// Wire bytes per second for an encoder bitrate, with headroom for overhead.
fn wire_rate(bitrate: u32) -> f64 {
    f64::from(bitrate.max(1)) / 8.0 * 1.25
}

pub(super) struct Pacer {
    frames: VecDeque<Frame>,
    queued_bytes: usize,
    queued_packets: usize,
    history: VecDeque<History>,
    history_bytes: usize,
    repairs: VecDeque<u16>,
    wire: Budget,
    repair: Budget,
    probe: Option<Probe>,
    last_keyframe: Option<Instant>,
}

impl Pacer {
    pub fn set_bitrate(&mut self, bitrate: u32, now: Instant) {
        let rate = wire_rate(bitrate);
        if rate < self.wire.rate {
            let mut bytes = 0;
            for frame in &mut self.frames {
                bytes += frame.payloads.iter().map(Vec::len).sum::<usize>()
                    + frame.payloads.len() * WIRE_OVERHEAD;
                frame.deadline = frame
                    .deadline
                    .max(now + QUEUE_AGE + Duration::from_secs_f64(bytes as f64 / rate));
            }
        }
        self.wire.set_rate(rate, now);
        self.repair.set_rate(rate * 0.2, now);
    }

    pub fn new(bitrate: u32, now: Instant) -> Self {
        let rate = wire_rate(bitrate);
        Self {
            frames: VecDeque::new(),
            queued_bytes: 0,
            queued_packets: 0,
            history: VecDeque::new(),
            history_bytes: 0,
            repairs: VecDeque::new(),
            wire: Budget::new(rate, now),
            repair: Budget::new(rate * 0.2, now),
            probe: None,
            last_keyframe: None,
        }
    }

    fn clear_frames(&mut self) {
        self.frames.clear();
        self.queued_bytes = 0;
        self.queued_packets = 0;
    }

    pub fn enqueue(
        &mut self,
        timestamp: u32,
        keyframe: bool,
        payloads: Vec<Vec<u8>>,
        now: Instant,
    ) -> bool {
        if self.expire(now) {
            return false;
        }
        let bytes: usize = payloads.iter().map(Vec::len).sum();
        if payloads.is_empty()
            || payloads
                .iter()
                .any(|p| p.is_empty() || p.len() > MAX_PAYLOAD)
            || bytes > 1024 * 1024
            || self.queued_bytes + bytes > MAX_QUEUE_BYTES
            || self.queued_packets + payloads.len() > 2048
        {
            self.clear_frames();
            return false;
        }
        self.queued_bytes += bytes;
        self.queued_packets += payloads.len();
        self.frames.push_back(Frame {
            timestamp,
            // Allow transmission of this frame and its backlog before the queueing allowance.
            deadline: now
                + QUEUE_AGE
                + Duration::from_secs_f64(
                    (self.queued_bytes + self.queued_packets * WIRE_OVERHEAD) as f64
                        / self.wire.rate,
                ),
            keyframe,
            payloads: payloads.into(),
        });
        true
    }

    pub fn has_keyframe(&self, now: Instant) -> bool {
        self.frames.iter().any(|frame| frame.keyframe)
            || self
                .last_keyframe
                .is_some_and(|sent| now.saturating_duration_since(sent) < HISTORY_AGE)
    }

    /// Shrink new frames while a keyframe or a rate reduction holds up the queue.
    pub fn encoder_bitrate(&self, bitrate: u32) -> u32 {
        let backlog =
            (self.queued_bytes + self.queued_packets * WIRE_OVERHEAD) as f64 / self.wire.rate;
        let allowance = QUEUE_AGE.as_secs_f64();
        ((f64::from(bitrate) * allowance / (allowance + backlog)) as u32 / 1000 * 1000).max(100_000)
    }

    pub fn can_probe(&self) -> bool {
        !self.history.is_empty()
    }

    fn trim_history(&mut self, now: Instant) {
        while self.history.front().is_some_and(|entry| {
            now.saturating_duration_since(entry.sent) >= HISTORY_AGE
                || self.history.len() > 1024
                || self.history_bytes > MAX_QUEUE_BYTES
        }) {
            self.history_bytes -= self.history.pop_front().unwrap().payload.len();
        }
        self.repairs
            .retain(|sequence| self.history.iter().any(|entry| entry.sequence == *sequence));
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        self.trim_history(now);
        if self
            .frames
            .front()
            .is_some_and(|frame| now >= frame.deadline)
        {
            self.clear_frames();
            true
        } else {
            false
        }
    }

    fn repair_entry(&self, sequence: u16, now: Instant) -> Option<&History> {
        self.history.iter().find(|entry| {
            entry.sequence == sequence
                && now.saturating_duration_since(entry.sent) < HISTORY_AGE
                && entry.repairs < 2
                && entry
                    .last_repair
                    .is_none_or(|last| now.saturating_duration_since(last) >= REPAIR_INTERVAL)
        })
    }

    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        let primary = self.frames.front().map(|frame| {
            self.wire
                .ready(frame.payloads.front().unwrap().len() + WIRE_OVERHEAD, now)
                .min(frame.deadline)
        });
        let repair = self
            .repairs
            .iter()
            .find_map(|sequence| self.repair_entry(*sequence, now))
            .map(|entry| {
                let bytes = entry.payload.len() + WIRE_OVERHEAD + 2;
                self.wire
                    .ready(bytes, now)
                    .max(self.repair.ready(bytes, now))
            });
        let probe = self.probe_entry().map(|(probe, entry)| {
            probe
                .budget
                .ready(entry.payload.len() + WIRE_OVERHEAD + 2, now)
        });
        [primary, repair, probe].into_iter().flatten().min()
    }

    /// Starts a cluster that adds `bitrate` of copies of recent packets for 30 ms.
    /// Receivers discard the copies, while transport feedback times them.
    pub fn probe(&mut self, bitrate: u32, now: Instant) {
        let rate = f64::from(bitrate) / 8.0;
        let size = MAX_PAYLOAD + WIRE_OVERHEAD + 2;
        self.probe = Some(Probe {
            budget: Budget::new(rate, now),
            remaining: ((rate * 0.03) as usize).clamp(6 * size, 64 * size),
            next: 0,
        });
    }

    fn probe_entry(&self) -> Option<(&Probe, &History)> {
        let probe = self.probe.as_ref()?;
        let count = self.history.len();
        let entry = self
            .history
            .get(count.checked_sub(1)? - probe.next % count)?;
        Some((probe, entry))
    }

    pub fn pop(&mut self, now: Instant) -> Option<Packet> {
        // The caller expires queued frames before popping so it can request a keyframe.
        if self
            .frames
            .front()
            .is_some_and(|frame| now >= frame.deadline)
        {
            return None;
        }
        self.trim_history(now);
        let repair = self
            .repairs
            .iter()
            .enumerate()
            .find_map(|(index, sequence)| {
                let entry = self.repair_entry(*sequence, now)?;
                let bytes = entry.payload.len() + WIRE_OVERHEAD + 2;
                (self.wire.ready(bytes, now) <= now && self.repair.ready(bytes, now) <= now)
                    .then_some((
                        index,
                        bytes,
                        Packet {
                            timestamp: entry.timestamp,
                            marker: entry.marker,
                            original_sequence: Some(*sequence),
                            payload: entry.payload.clone(),
                            probe: self.probe.is_some(),
                            duplicate: false,
                        },
                    ))
            });
        if let Some((index, bytes, packet)) = repair {
            self.repairs.remove(index);
            self.wire.spend(bytes, now);
            self.repair.spend(bytes, now);
            return Some(packet);
        }
        if let Some(frame) = self.frames.front_mut() {
            let bytes = frame.payloads.front().unwrap().len() + WIRE_OVERHEAD;
            if self.wire.ready(bytes, now) <= now {
                let payload = frame.payloads.pop_front().unwrap();
                let packet = Packet {
                    timestamp: frame.timestamp,
                    marker: frame.payloads.is_empty(),
                    original_sequence: None,
                    payload,
                    probe: self.probe.is_some(),
                    duplicate: false,
                };
                self.queued_bytes -= packet.payload.len();
                self.queued_packets -= 1;
                if packet.marker {
                    if frame.keyframe {
                        self.last_keyframe = Some(now);
                    }
                    self.frames.pop_front();
                }
                self.wire.spend(bytes, now);
                return Some(packet);
            }
        }
        let (probe, entry) = self.probe_entry()?;
        let bytes = entry.payload.len() + WIRE_OVERHEAD + 2;
        if probe.budget.ready(bytes, now) > now {
            return None;
        }
        let packet = Packet {
            timestamp: entry.timestamp,
            marker: entry.marker,
            original_sequence: Some(entry.sequence),
            payload: entry.payload.clone(),
            probe: true,
            duplicate: true,
        };
        let probe = self.probe.as_mut().unwrap();
        probe.budget.spend(bytes, now);
        probe.next += 1;
        probe.remaining = probe.remaining.saturating_sub(bytes);
        if probe.remaining == 0 {
            self.probe = None;
        }
        Some(packet)
    }

    pub fn sent(&mut self, sequence: u16, packet: Packet, now: Instant) {
        if packet.duplicate {
            return;
        }
        self.trim_history(now);
        if let Some(original) = packet.original_sequence {
            if let Some(entry) = self
                .history
                .iter_mut()
                .find(|entry| entry.sequence == original)
            {
                entry.repairs += 1;
                entry.last_repair = Some(now);
            }
            return;
        }
        if let Some(index) = self
            .history
            .iter()
            .position(|entry| entry.sequence == sequence)
        {
            self.history_bytes -= self.history.remove(index).unwrap().payload.len();
            self.repairs.retain(|queued| *queued != sequence);
        }
        self.history_bytes += packet.payload.len();
        self.history.push_back(History {
            sequence,
            timestamp: packet.timestamp,
            marker: packet.marker,
            payload: packet.payload,
            sent: now,
            repairs: 0,
            last_repair: None,
        });
        self.trim_history(now);
    }

    pub fn request(&mut self, sequences: &[u16], now: Instant) {
        self.trim_history(now);
        for &sequence in sequences.iter().take(64) {
            if self.repairs.len() >= 64 {
                break;
            }
            if !self.repairs.contains(&sequence) && self.repair_entry(sequence, now).is_some() {
                self.repairs.push_back(sequence);
            }
        }
    }

    pub fn clear(&mut self, now: Instant) {
        self.clear_frames();
        self.history.clear();
        self.history_bytes = 0;
        self.repairs.clear();
        self.probe = None;
        self.last_keyframe = None;
        self.wire = Budget::new(self.wire.rate, now);
        self.repair = Budget::new(self.repair.rate, now);
    }
}
