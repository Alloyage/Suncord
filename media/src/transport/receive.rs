//! Bounded RFC 6184 assembly before DAVE authentication.
use super::h264::valid_nal;
use std::time::{Duration, Instant};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_FRAME: usize = 1024 * 1024;
const MAX_PACKETS: usize = 1024;
const MAX_AGE: Duration = Duration::from_millis(250);

pub(super) struct Frame {
    pub timestamp: u32,
    pub data: Vec<u8>,
    pub keyframe: bool,
}

struct Packet {
    sequence: u16,
    marker: bool,
    payload: Vec<u8>,
}

struct Assembly {
    timestamp: u32,
    since: Instant,
    packets: Vec<Packet>,
}

pub(super) struct Video {
    frames: Vec<Assembly>,
    next: Option<u16>,
    finished: Option<u32>,
    synchronized: bool,
    request: bool,
}

impl Video {
    pub fn new() -> Self {
        Self {
            frames: Vec::new(),
            next: None,
            finished: None,
            synchronized: false,
            request: true,
        }
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.request)
    }

    pub fn expire(&mut self, now: Instant) {
        if self
            .frames
            .iter()
            .any(|frame| now.saturating_duration_since(frame.since) >= MAX_AGE)
        {
            self.clear();
        }
    }

    pub fn push(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
        now: Instant,
    ) -> Result<Option<Frame>, String> {
        self.expire(now);
        if self
            .finished
            .is_some_and(|last| timestamp.wrapping_sub(last) as i32 <= 0)
        {
            return Ok(None);
        }
        // A random join has no trustworthy first sequence. Skip through the first
        // marker, then use that boundary to prove completeness of later frames.
        if self.next.is_none() {
            self.request = true;
            if marker {
                self.next = Some(sequence.wrapping_add(1));
                self.finished = Some(timestamp);
            }
            return Ok(None);
        }
        let next = self.next.unwrap();
        if (sequence.wrapping_sub(next) as i16) < 0 {
            return Ok(None);
        }
        if sequence.wrapping_sub(next) as usize >= MAX_PACKETS
            || payload.is_empty()
            || payload.len() > MAX_FRAME
        {
            self.clear();
            return Err("H.264 receive packet exceeded assembly limits".into());
        }
        let index = match self
            .frames
            .iter()
            .position(|frame| frame.timestamp == timestamp)
        {
            Some(index) => index,
            None => {
                if self.frames.len() == 8 {
                    self.clear();
                    return Err("Too many incomplete H.264 frames".into());
                }
                self.frames.push(Assembly {
                    timestamp,
                    since: now,
                    packets: Vec::new(),
                });
                self.frames
                    .sort_by(|a, b| (a.timestamp.wrapping_sub(b.timestamp) as i32).cmp(&0));
                self.frames
                    .iter()
                    .position(|frame| frame.timestamp == timestamp)
                    .unwrap()
            }
        };
        if let Some(packet) = self.frames[index]
            .packets
            .iter()
            .find(|packet| packet.sequence == sequence)
        {
            if packet.marker == marker && packet.payload == payload {
                return Ok(None);
            }
            self.clear();
            return Err("Conflicting duplicate H.264 packet".into());
        }
        let bytes: usize = self
            .frames
            .iter()
            .flat_map(|frame| &frame.packets)
            .map(|packet| packet.payload.len())
            .sum();
        let packets: usize = self.frames.iter().map(|frame| frame.packets.len()).sum();
        let frame_bytes: usize = self.frames[index]
            .packets
            .iter()
            .map(|packet| packet.payload.len())
            .sum();
        if bytes + payload.len() > MAX_BYTES
            || packets == MAX_PACKETS
            || frame_bytes + payload.len() > MAX_FRAME
        {
            self.clear();
            return Err("H.264 receive assembly exceeded limits".into());
        }
        self.frames[index].packets.push(Packet {
            sequence,
            marker,
            payload: payload.to_vec(),
        });
        let frame = &mut self.frames[0];
        frame
            .packets
            .sort_by_key(|packet| packet.sequence.wrapping_sub(next));
        if !frame.packets.last().is_some_and(|packet| packet.marker)
            || frame
                .packets
                .iter()
                .enumerate()
                .any(|(index, packet)| packet.sequence != next.wrapping_add(index as u16))
        {
            return Ok(None);
        }
        let frame = self.frames.remove(0);
        self.next = Some(next.wrapping_add(frame.packets.len() as u16));
        self.finished = Some(frame.timestamp);
        let decoded = assemble(frame);
        match decoded {
            Ok(frame) => {
                if frame.keyframe {
                    self.synchronized = true;
                }
                if !self.synchronized {
                    self.request = true;
                }
                Ok(self.synchronized.then_some(frame))
            }
            Err(error) => {
                self.synchronized = false;
                self.request = true;
                Err(error)
            }
        }
    }
}

fn assemble(frame: Assembly) -> Result<Frame, String> {
    let mut data = Vec::new();
    let mut fragment = None;
    let mut keyframe = false;
    for (index, packet) in frame.packets.iter().enumerate() {
        if packet.marker && index + 1 != frame.packets.len() {
            return Err("H.264 marker preceded access unit end".into());
        }
        let payload = &packet.payload;
        let header = payload[0];
        if header & 0x80 != 0 {
            return Err("Forbidden H.264 NAL bit".into());
        }
        match header & 31 {
            1..=23 if fragment.is_none() => {
                data.extend_from_slice(&[0, 0, 0, 1]);
                data.extend_from_slice(payload);
                keyframe |= header & 31 == 5;
            }
            24 if fragment.is_none() => {
                let mut offset = 1;
                let mut count = 0;
                while offset < payload.len() {
                    if offset + 2 > payload.len() {
                        return Err("Truncated H.264 STAP-A length".into());
                    }
                    let size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                    offset += 2;
                    if size == 0 || size > payload.len() - offset || !valid_nal(payload[offset]) {
                        return Err("Invalid H.264 STAP-A NAL".into());
                    }
                    data.extend_from_slice(&[0, 0, 0, 1]);
                    data.extend_from_slice(&payload[offset..offset + size]);
                    keyframe |= payload[offset] & 31 == 5;
                    offset += size;
                    count += 1;
                    if count > MAX_PACKETS || data.len() > MAX_FRAME {
                        return Err("H.264 STAP-A exceeded limits".into());
                    }
                }
                if count == 0 {
                    return Err("Empty H.264 STAP-A".into());
                }
            }
            28 if payload.len() > 2 => {
                let flags = payload[1];
                let reconstructed = (header & 0xe0) | (flags & 31);
                if flags & 0x20 != 0 || flags & 0xc0 == 0xc0 || !valid_nal(reconstructed) {
                    return Err("Invalid H.264 FU-A header".into());
                }
                if flags & 0x80 != 0 {
                    if fragment.is_some() {
                        return Err("Nested H.264 FU-A start".into());
                    }
                    fragment = Some(reconstructed);
                    data.extend_from_slice(&[0, 0, 0, 1, reconstructed]);
                    keyframe |= reconstructed & 31 == 5;
                } else if fragment != Some(reconstructed) {
                    return Err("H.264 FU-A continuation without matching start".into());
                }
                data.extend_from_slice(&payload[2..]);
                if flags & 0x40 != 0 {
                    fragment = None;
                }
            }
            _ => return Err("Unsupported or interrupted H.264 NAL".into()),
        }
        if data.len() > MAX_FRAME {
            return Err("H.264 access unit exceeded limit".into());
        }
    }
    if fragment.is_some() {
        return Err("Incomplete H.264 FU-A".into());
    }
    Ok(Frame {
        timestamp: frame.timestamp,
        data,
        keyframe,
    })
}
