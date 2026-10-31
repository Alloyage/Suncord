//! Discord AES256-GCM rtpsize, RTP, and bounded RTCP feedback parsing.
use crate::capture::Codec;
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const MODE: &str = "aead_aes256_gcm_rtpsize";
// Leave room for UDP/IP and Discord's forwarding overhead on minimum-MTU paths.
pub(super) const MTU: usize = 1200;
pub(super) const H264_PT: u8 = 101;
pub(super) const H264_RTX_PT: u8 = 102;

/// Media and RTX payload types advertised in Select Protocol. 96 is reserved for probes.
pub(super) fn payload_types(codec: Codec) -> (u8, u8) {
    match codec {
        Codec::H265 => (103, 104),
        Codec::Av1 => (109, 110),
        _ => (H264_PT, H264_RTX_PT),
    }
}

/// RFC 8285 one-byte elements in Discord's sender-side numbering: transport
/// sequence (5) and abs-send-time (3), then screen content (7) and RID "100"
/// (11) unless repairing. Padded to whole words.
fn video_extensions(sequence: u16, repair: bool) -> Vec<u8> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // 6.18 fixed point seconds.
    let send =
        ((u64::from(now.subsec_nanos()) << 18) / 1_000_000_000) | ((now.as_secs() & 63) << 18);
    let [high, low] = sequence.to_be_bytes();
    let [_, _, _, _, _, a, b, c] = send.to_be_bytes();
    let mut extensions = vec![0x51, high, low, 0x32, a, b, c];
    if !repair {
        extensions.extend([0x70, 1, 0xb2, b'1', b'0', b'0']);
    }
    extensions.resize(extensions.len().next_multiple_of(4), 0);
    extensions
}

pub(super) struct Feedback {
    pub sender_reports: Vec<SenderReport>,
    pub keyframe: bool,
    pub nacks: Vec<u16>,
    pub transport: Vec<TransportFeedback>,
    pub receiver_bitrate: Option<u32>,
}

pub(super) struct SenderReport {
    pub ssrc: u32,
    pub ntp: u64,
    pub timestamp: u32,
}

pub(super) struct TransportFeedback {
    /// Reference time in 64 ms units, wrapping at 24 bits.
    pub reference: u32,
    pub count: u8,
    /// Transport sequence, and arrival in microseconds after the reference.
    pub packets: Vec<(u16, Option<i64>)>,
}

/// draft-holmer-rmcat-transport-wide-cc-extensions-01 feedback. Outer None is
/// malformed, inner None is too large to track.
fn transport_feedback(part: &[u8]) -> Option<Option<TransportFeedback>> {
    let base = u16::from_be_bytes(part.get(12..14)?.try_into().ok()?);
    let total = usize::from(u16::from_be_bytes(part.get(14..16)?.try_into().ok()?));
    let reference = u32::from_be_bytes([0, *part.get(16)?, part[17], part[18]]);
    let count = *part.get(19)?;
    if total > 4096 {
        return Some(None);
    }
    let mut symbols = Vec::with_capacity(total);
    let mut at = 20;
    while symbols.len() < total {
        let chunk = u16::from_be_bytes(part.get(at..at + 2)?.try_into().ok()?);
        at += 2;
        if chunk & 0x8000 == 0 {
            let run = usize::from(chunk & 0x1fff).min(total - symbols.len());
            symbols.extend(std::iter::repeat_n((chunk >> 13) & 3, run));
        } else if chunk & 0x4000 == 0 {
            symbols.extend((0..14).rev().map(|bit| (chunk >> bit) & 1));
        } else {
            symbols.extend((0..7).rev().map(|pair| (chunk >> (pair * 2)) & 3));
        }
    }
    symbols.truncate(total);
    let mut time = 0i64;
    let mut packets = Vec::with_capacity(total);
    for (index, symbol) in symbols.into_iter().enumerate() {
        let arrival = match symbol {
            0 => None,
            1 => {
                time += i64::from(*part.get(at)?) * 250;
                at += 1;
                Some(time)
            }
            2 => {
                time += i64::from(i16::from_be_bytes(part.get(at..at + 2)?.try_into().ok()?)) * 250;
                at += 2;
                Some(time)
            }
            _ => return None,
        };
        packets.push((base.wrapping_add(index as u16), arrival));
    }
    Some(Some(TransportFeedback {
        reference,
        count,
        packets,
    }))
}

/// Parse authenticated compound or reduced-size RTCP, rejecting the whole
/// datagram if any packet is malformed (RFC 3550, 4585 and 5506).
pub(super) fn feedback(mut packet: &[u8], video_ssrc: u32) -> Option<Feedback> {
    if packet.is_empty() || packet.len() > 4096 {
        return None;
    }
    let mut feedback = Feedback {
        sender_reports: Vec::new(),
        keyframe: false,
        nacks: Vec::new(),
        transport: Vec::new(),
        receiver_bitrate: None,
    };
    while !packet.is_empty() {
        if packet.len() < 4 || packet[0] >> 6 != 2 {
            return None;
        }
        let size = (usize::from(u16::from_be_bytes([packet[2], packet[3]])) + 1) * 4;
        if size > packet.len() {
            return None;
        }
        let mut part = &packet[..size];
        if part[0] & 32 != 0 {
            let padding = usize::from(*part.last()?);
            // Discord can append NACK after a padded TWCC subpacket. Each
            // declared subpacket boundary still determines its own padding.
            if padding == 0 || padding > size - 4 {
                return None;
            }
            part = &part[..size - padding];
        }
        let count = usize::from(part[0] & 31);
        match part[1] {
            200 | 201 => {
                let start = if part[1] == 200 { 28 } else { 8 };
                let end = start + count * 24;
                // SR/RR may carry aligned profile-specific extensions.
                if part.len() < end || !(part.len() - end).is_multiple_of(4) {
                    return None;
                }
                if part[1] == 200 && feedback.sender_reports.len() < 64 {
                    feedback.sender_reports.push(SenderReport {
                        ssrc: u32::from_be_bytes(part[4..8].try_into().ok()?),
                        ntp: u64::from_be_bytes(part[8..16].try_into().ok()?),
                        timestamp: u32::from_be_bytes(part[16..20].try_into().ok()?),
                    });
                }
            }
            205 | 206 => {
                if part.len() < 12 {
                    return None;
                }
                let matches = part[8..12] == video_ssrc.to_be_bytes();
                match (part[1], count) {
                    (205, 1) => {
                        if part.len() < 16 || !(part.len() - 12).is_multiple_of(4) {
                            return None;
                        }
                        if matches {
                            for nack in part[12..].as_chunks::<4>().0 {
                                let pid = u16::from_be_bytes([nack[0], nack[1]]);
                                let mask = u16::from_be_bytes([nack[2], nack[3]]);
                                for bit in 0..=16 {
                                    if bit == 0 || mask & (1 << (bit - 1)) != 0 {
                                        let sequence = pid.wrapping_add(bit);
                                        if feedback.nacks.len() < 64
                                            && !feedback.nacks.contains(&sequence)
                                        {
                                            feedback.nacks.push(sequence);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Transport feedback covers every SSRC on this connection.
                    (205, 15) => {
                        if let Some(report) = transport_feedback(part)? {
                            if feedback.transport.len() < 16 {
                                feedback.transport.push(report);
                            }
                        }
                    }
                    (206, 15) if part.get(12..16) == Some(b"REMB") => {
                        if part.len() < 20 || part.len() != 20 + usize::from(part[16]) * 4 {
                            return None;
                        }
                        if part[20..]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .any(|ssrc| *ssrc == video_ssrc.to_be_bytes())
                        {
                            let exponent = u32::from(part[17] >> 2);
                            let mantissa = (u64::from(part[17] & 3) << 16)
                                | (u64::from(part[18]) << 8)
                                | u64::from(part[19]);
                            let bitrate = mantissa
                                .saturating_mul(1u64 << exponent)
                                .min(u64::from(u32::MAX))
                                as u32;
                            feedback.receiver_bitrate = Some(
                                feedback
                                    .receiver_bitrate
                                    .map_or(bitrate, |old| old.min(bitrate)),
                            );
                        }
                    }
                    (206, 1) => {
                        if part.len() != 12 {
                            return None;
                        }
                        feedback.keyframe |= matches;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        packet = &packet[size..];
    }
    Some(feedback)
}

pub(super) struct Cipher {
    aes: Aes256Gcm,
    // Shared by RTP and RTCP. A new key is mandatory before exhaustion.
    counter: u64,
}
impl Cipher {
    pub(super) fn new(key: &[u8]) -> Result<Self, String> {
        Ok(Self {
            aes: Aes256Gcm::new_from_slice(key).map_err(|_| "Invalid transport key")?,
            counter: 0,
        })
    }
    pub(super) fn seal(&mut self, header: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
        let count = u32::try_from(self.counter).map_err(|_| "Transport nonce exhausted")?;
        self.counter += 1;
        let mut nonce = [0; 12];
        nonce[..4].copy_from_slice(&count.to_be_bytes());
        let encrypted = self
            .aes
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: data,
                    aad: header,
                },
            )
            .map_err(|_| "Transport encryption failed")?;
        let mut packet = header.to_vec();
        packet.extend(encrypted);
        packet.extend(count.to_be_bytes());
        Ok(packet)
    }
    pub(super) fn open(&self, packet: &[u8], clear: usize) -> Option<Vec<u8>> {
        if packet.len() < clear + 20 {
            return None;
        }
        let end = packet.len() - 4;
        let mut nonce = [0; 12];
        nonce[..4].copy_from_slice(&packet[end..]);
        self.aes
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &packet[clear..end],
                    aad: &packet[..clear],
                },
            )
            .ok()
    }
}

pub(super) fn header(sequence: u16, timestamp: u32, ssrc: u32, marker: bool) -> [u8; 12] {
    let mut h = [
        0x80,
        120 | if marker { 0x80 } else { 0 },
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    h[2..4].copy_from_slice(&sequence.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

/// RTX preserves DAVE ciphertext and uses the allocated SSRC association.
/// RTX carries no RID, only the congestion control extensions.
pub(super) fn video_packet(
    sequence: u16,
    ssrc: u32,
    codec: Codec,
    transport_sequence: u16,
    packet: &super::pacer::Packet,
) -> (Vec<u8>, Vec<u8>) {
    let repair = packet.original_sequence.is_some();
    let (media, rtx) = payload_types(codec);
    let mut header = header(sequence, packet.timestamp, ssrc, packet.marker).to_vec();
    header[0] |= 0x10;
    header[1] = (header[1] & 0x80) | if repair { rtx } else { media };
    let mut payload = video_extensions(transport_sequence, repair);
    header.extend([0xbe, 0xde, 0, (payload.len() / 4) as u8]);
    if let Some(original) = packet.original_sequence {
        payload.extend(original.to_be_bytes());
    }
    payload.extend_from_slice(&packet.payload);
    (header, payload)
}

pub(super) struct Packet<'a> {
    pub ssrc: u32,
    pub sequence: u16,
    pub timestamp: u32,
    pub payload: &'a [u8],
}

pub(super) fn clear_len(packet: &[u8]) -> Option<usize> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let size = 12 + usize::from(packet[0] & 15) * 4 + if packet[0] & 16 != 0 { 4 } else { 0 };
    (packet.len() >= size).then_some(size)
}

// Extension preamble is authenticated cleartext, extension content is encrypted.
pub(super) fn opus<'a>(header: &[u8], plain: &'a [u8]) -> Option<Packet<'a>> {
    if header[1] & 127 != 120 {
        return None;
    }
    media(header, plain)
}

pub(super) fn media<'a>(header: &[u8], plain: &'a [u8]) -> Option<Packet<'a>> {
    let mut start = 0;
    if header[0] & 16 != 0 {
        start = usize::from(u16::from_be_bytes(
            header[header.len() - 2..].try_into().ok()?,
        )) * 4;
    }
    let mut end = plain.len();
    if header[0] & 32 != 0 {
        let padding = usize::from(*plain.last()?);
        if padding == 0 || padding > end {
            return None;
        }
        end -= padding;
    }
    if start >= end {
        return None;
    }
    Some(Packet {
        ssrc: u32::from_be_bytes(header[8..12].try_into().ok()?),
        sequence: u16::from_be_bytes(header[2..4].try_into().ok()?),
        timestamp: u32::from_be_bytes(header[4..8].try_into().ok()?),
        payload: &plain[start..end],
    })
}

pub(super) fn pli(sender: u32, media: u32) -> [u8; 12] {
    let mut packet = [0x81, 206, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0];
    packet[4..8].copy_from_slice(&sender.to_be_bytes());
    packet[8..].copy_from_slice(&media.to_be_bytes());
    packet
}

pub(super) fn sender_report(
    ssrc: u32,
    timestamp: u32,
    packets: u64,
    octets: u64,
) -> Result<Vec<u8>, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "Invalid system clock")?;
    let mut report = vec![0x80, 200, 0, 6];
    report.extend(ssrc.to_be_bytes());
    report.extend((now.as_secs().wrapping_add(2_208_988_800) as u32).to_be_bytes());
    report.extend(((((u64::from(now.subsec_nanos())) << 32) / 1_000_000_000) as u32).to_be_bytes());
    report.extend(timestamp.to_be_bytes());
    report.extend((packets as u32).to_be_bytes());
    report.extend((octets as u32).to_be_bytes());
    // Compound RTCP requires an SDES CNAME. This reveals no account identifier.
    let cname = b"suncord";
    report.extend([0x81, 202, 0, 4]);
    report.extend(ssrc.to_be_bytes());
    report.extend([1, cname.len() as u8]);
    report.extend(cname);
    report.extend([0, 0, 0]);
    Ok(report)
}
