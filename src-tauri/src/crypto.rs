use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMode {
    Aes256Gcm,
    XChaCha20,
}

impl TransportMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Aes256Gcm => "aes-256-gcm",
            Self::XChaCha20 => "xchacha20-poly1305",
        }
    }

    pub fn discord_name(self) -> &'static str {
        match self {
            Self::Aes256Gcm => "aead_aes256_gcm_rtpsize",
            Self::XChaCha20 => "aead_xchacha20_poly1305_rtpsize",
        }
    }

    pub fn choose(modes: &[String]) -> Option<Self> {
        if modes.iter().any(|mode| mode == "aead_aes256_gcm_rtpsize") {
            Some(Self::Aes256Gcm)
        } else if modes
            .iter()
            .any(|mode| mode == "aead_xchacha20_poly1305_rtpsize")
        {
            Some(Self::XChaCha20)
        } else {
            None
        }
    }
}

pub fn rtp_header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[0] = 0x80;
    header[1] = 0x78;
    header[2..4].copy_from_slice(&sequence.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    header
}

/// Seal an Opus payload with Discord's RTP-size AEAD layout.
/// The 4-byte counter is the start of the nonce and is appended to the packet.
/// AES-256-GCM uses the CPU's AES-NI path when the `aes` crate detects it.
pub fn seal(
    mode: TransportMode,
    key: &[u8; 32],
    header: &[u8],
    plaintext: &[u8],
    counter: u32,
) -> Result<Vec<u8>, String> {
    let tail = counter.to_be_bytes();
    let ciphertext = encrypt(mode, key, header, plaintext, &tail)?;
    let mut packet = Vec::with_capacity(header.len() + ciphertext.len() + 4);
    packet.extend_from_slice(header);
    packet.extend_from_slice(&ciphertext);
    packet.extend_from_slice(&tail);
    Ok(packet)
}

pub fn open(mode: TransportMode, key: &[u8; 32], packet: &[u8]) -> Result<Vec<u8>, String> {
    if packet.len() < 12 + 16 + 4 {
        return Err("voice packet is too short".into());
    }
    let header = &packet[..12];
    let tail = &packet[packet.len() - 4..];
    let ciphertext = &packet[12..packet.len() - 4];
    decrypt(mode, key, header, ciphertext, tail)
}

fn encrypt(
    mode: TransportMode,
    key: &[u8; 32],
    header: &[u8],
    plaintext: &[u8],
    tail: &[u8; 4],
) -> Result<Vec<u8>, String> {
    match mode {
        TransportMode::Aes256Gcm => {
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(tail);
            let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "bad aes key")?;
            cipher
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: plaintext,
                        aad: header,
                    },
                )
                .map_err(|_| "aes-gcm encrypt failed".to_string())
        }
        TransportMode::XChaCha20 => {
            let mut nonce = [0u8; 24];
            nonce[..4].copy_from_slice(tail);
            let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| "bad xchacha key")?;
            cipher
                .encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: plaintext,
                        aad: header,
                    },
                )
                .map_err(|_| "xchacha encrypt failed".to_string())
        }
    }
}

fn decrypt(
    mode: TransportMode,
    key: &[u8; 32],
    header: &[u8],
    ciphertext: &[u8],
    tail: &[u8],
) -> Result<Vec<u8>, String> {
    if tail.len() != 4 {
        return Err("bad nonce".into());
    }
    let mut tail_bytes = [0u8; 4];
    tail_bytes.copy_from_slice(tail);
    match mode {
        TransportMode::Aes256Gcm => {
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(&tail_bytes);
            let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "bad aes key")?;
            cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: ciphertext,
                        aad: header,
                    },
                )
                .map_err(|_| "aes-gcm decrypt failed".to_string())
        }
        TransportMode::XChaCha20 => {
            let mut nonce = [0u8; 24];
            nonce[..4].copy_from_slice(&tail_bytes);
            let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| "bad xchacha key")?;
            cipher
                .decrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: ciphertext,
                        aad: header,
                    },
                )
                .map_err(|_| "xchacha decrypt failed".to_string())
        }
    }
}

pub fn discovery_packet(ssrc: u32) -> [u8; 74] {
    let mut packet = [0u8; 74];
    packet[0] = 0x00;
    packet[1] = 0x01;
    packet[2] = 0x00;
    packet[3] = 70;
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    packet
}

pub fn parse_discovery(packet: &[u8]) -> Option<(String, u16)> {
    if packet.len() < 74 {
        return None;
    }
    let address = packet[8..72]
        .split(|byte| *byte == 0)
        .next()
        .unwrap_or_default();
    let address = std::str::from_utf8(address).ok()?.trim().to_string();
    if address.is_empty() {
        return None;
    }
    let port = u16::from_be_bytes([packet[72], packet[73]]);
    Some((address, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_packet_round_trip() {
        let key = [7u8; 32];
        let header = rtp_header(1, 960, 42);
        let opus = [0xF8, 0xFF, 0xFE];
        let packet = seal(TransportMode::Aes256Gcm, &key, &header, &opus, 9).unwrap();
        let opened = open(TransportMode::Aes256Gcm, &key, &packet).unwrap();
        assert_eq!(opened, opus);
    }

    #[test]
    fn xchacha_packet_round_trip() {
        let key = [9u8; 32];
        let header = rtp_header(2, 1920, 7);
        let opus = b"frame".to_vec();
        let packet = seal(TransportMode::XChaCha20, &key, &header, &opus, 3).unwrap();
        let opened = open(TransportMode::XChaCha20, &key, &packet).unwrap();
        assert_eq!(opened, opus);
    }

    #[test]
    fn prefers_aes_when_the_server_offers_it() {
        let modes = vec![
            "aead_xchacha20_poly1305_rtpsize".into(),
            "aead_aes256_gcm_rtpsize".into(),
        ];
        assert_eq!(TransportMode::choose(&modes), Some(TransportMode::Aes256Gcm));
    }
}
