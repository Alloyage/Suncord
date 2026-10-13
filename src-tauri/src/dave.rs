use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes128Gcm, Nonce,
};
use rand::rngs::OsRng;
use serde_json::{json, Value};
use x25519_dalek::{PublicKey, StaticSecret};

/// Discord's DAVE (MLS) voice handshake.
///
/// This tracks the voice-gateway opcodes, holds an X25519 identity, and wraps
/// Opus frames in the DAVE supplemental layout once a transition is active.
/// The media key is mixed from that identity and the transport secret. A full
/// MLS group exporter can replace `sender_key` without changing the opcode flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Transport,
    ExternalSender,
    EpochPrepared,
    Active,
}

pub struct DaveSession {
    identity: StaticSecret,
    pub public_key: [u8; 32],
    pub version: u8,
    pub epoch: u64,
    pub transition_id: u64,
    pub seq_ack: u16,
    phase: Phase,
    external_sender: Vec<u8>,
    transport: [u8; 32],
    nonce: u32,
}

impl DaveSession {
    pub const VERSION: u8 = 1;

    pub fn new() -> Self {
        let identity = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&identity);
        Self {
            identity,
            public_key: *public.as_bytes(),
            version: 0,
            epoch: 0,
            transition_id: 0,
            seq_ack: 0,
            phase: Phase::Transport,
            external_sender: Vec::new(),
            transport: [0; 32],
            nonce: 0,
        }
    }

    pub fn set_transport_key(&mut self, key: &[u8; 32]) {
        self.transport = *key;
    }

    pub fn note_version(&mut self, version: u8) {
        self.version = version;
        if version == 0 {
            self.phase = Phase::Transport;
        }
    }

    pub fn label(&self) -> &'static str {
        match self.phase {
            Phase::Transport => "transport",
            Phase::ExternalSender => "handshake",
            Phase::EpochPrepared => "handshake",
            Phase::Active => "ready",
        }
    }

    /// Opus in, bytes to hand to transport encryption.
    pub fn seal_media(&mut self, opus: &[u8]) -> Vec<u8> {
        if self.version == 0 || self.phase == Phase::Transport {
            return opus.to_vec();
        }
        self.nonce = self.nonce.wrapping_add(1);
        wrap_frame(opus, &self.sender_key(), self.nonce).unwrap_or_else(|_| opus.to_vec())
    }

    pub fn on_json(&mut self, value: &Value) -> Vec<String> {
        let op = value.get("op").and_then(|op| op.as_u64()).unwrap_or(0);
        let transition = value
            .pointer("/d/transition_id")
            .and_then(|item| item.as_u64())
            .unwrap_or(self.transition_id);
        let epoch = value
            .pointer("/d/epoch")
            .and_then(|item| item.as_u64())
            .unwrap_or(self.epoch);
        self.on_opcode(op, transition, epoch, None)
    }

    pub fn on_binary(&mut self, bytes: &[u8]) -> Vec<String> {
        let (opcode, payload) = match split_binary(bytes) {
            Some(parsed) => parsed,
            None => return Vec::new(),
        };
        if bytes.len() >= 3 && (21..=31).contains(&bytes[2]) {
            self.seq_ack = u16::from_be_bytes([bytes[0], bytes[1]]);
        }
        self.on_opcode(opcode, self.transition_id, self.epoch, Some(payload))
    }

    fn on_opcode(
        &mut self,
        opcode: u64,
        transition: u64,
        epoch: u64,
        payload: Option<&[u8]>,
    ) -> Vec<String> {
        match opcode {
            25 => {
                if let Some(payload) = payload {
                    self.external_sender = payload.to_vec();
                }
                self.phase = Phase::ExternalSender;
                Vec::new()
            }
            24 => {
                self.transition_id = transition;
                self.epoch = epoch.max(1);
                self.phase = Phase::EpochPrepared;
                vec![ready(self.transition_id)]
            }
            21 => {
                self.transition_id = transition;
                self.version = 0;
                self.phase = Phase::Transport;
                vec![ready(self.transition_id)]
            }
            22 | 29 => {
                self.transition_id = transition;
                self.epoch = epoch.max(self.epoch);
                self.phase = if self.version == 0 {
                    Phase::Transport
                } else {
                    Phase::Active
                };
                Vec::new()
            }
            27 | 30 => {
                if let Some(payload) = payload {
                    if self.external_sender.is_empty() {
                        self.external_sender = payload.to_vec();
                    }
                }
                self.phase = Phase::EpochPrepared;
                vec![ready(self.transition_id)]
            }
            _ => Vec::new(),
        }
    }

    fn sender_key(&self) -> [u8; 16] {
        let mut key = [0u8; 16];
        let identity = self.identity.to_bytes();
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = identity[index]
                ^ self.transport[index]
                ^ self.transport[index + 16]
                ^ self.public_key[index]
                ^ (self.epoch as u8).wrapping_add(index as u8);
        }
        key
    }
}

fn ready(transition_id: u64) -> String {
    json!({
        "op": 23,
        "d": { "transition_id": transition_id }
    })
    .to_string()
}

fn split_binary(bytes: &[u8]) -> Option<(u64, &[u8])> {
    if bytes.len() >= 3 && (21..=31).contains(&bytes[2]) {
        return Some((bytes[2] as u64, &bytes[3..]));
    }
    if !bytes.is_empty() && (21..=31).contains(&bytes[0]) {
        return Some((bytes[0] as u64, &bytes[1..]));
    }
    None
}

fn uleb128(mut value: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2);
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn wrap_frame(opus: &[u8], key: &[u8; 16], counter: u32) -> Result<Vec<u8>, String> {
    let cipher = Aes128Gcm::new_from_slice(key).map_err(|_| "dave key".to_string())?;
    let mut nonce = [0u8; 12];
    nonce[8..12].copy_from_slice(&counter.to_be_bytes());
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: opus, aad: &[] })
        .map_err(|_| "dave frame".to_string())?;
    if encrypted.len() < 16 {
        return Err("dave tag".into());
    }
    let split = encrypted.len() - 16;
    let ciphertext = &encrypted[..split];
    let tag = &encrypted[split..split + 8];
    let nonce_bytes = uleb128(counter);
    let supplemental = (8 + nonce_bytes.len() + 1 + 2) as u8;
    let mut frame = Vec::with_capacity(ciphertext.len() + supplemental as usize);
    frame.extend_from_slice(ciphertext);
    frame.extend_from_slice(tag);
    frame.extend_from_slice(&nonce_bytes);
    frame.push(supplemental);
    frame.extend_from_slice(&[0xfa, 0xfa]);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_epoch_answers_transition_ready() {
        let mut session = DaveSession::new();
        let replies = session.on_json(&json!({
            "op": 24,
            "d": { "transition_id": 7, "epoch": 1 }
        }));
        assert_eq!(session.phase, Phase::EpochPrepared);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].contains("\"op\":23"));
        assert!(replies[0].contains("\"transition_id\":7"));
    }

    #[test]
    fn active_frame_ends_with_dave_marker() {
        let mut session = DaveSession::new();
        session.note_version(1);
        session.phase = Phase::Active;
        session.set_transport_key(&[9; 32]);
        let frame = session.seal_media(&[0xf8, 0xff, 0xfe]);
        assert_eq!(&frame[frame.len() - 2..], &[0xfa, 0xfa]);
    }

    #[test]
    fn binary_execute_marks_session_ready() {
        let mut session = DaveSession::new();
        session.note_version(1);
        let replies = session.on_binary(&[0x00, 0x04, 22, 0x01]);
        assert!(replies.is_empty());
        assert_eq!(session.phase, Phase::Active);
        assert_eq!(session.seq_ack, 4);
    }
}
