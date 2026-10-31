//! RFC 7798 single NAL unit and FU packetization of an Annex B frame.
use super::h264::nals;

const FU: u8 = 49;

/// Forbidden bit clear, nonzero TemporalId, and no RTP-only types.
fn valid(nal: &[u8]) -> bool {
    nal.len() > 2 && nal[0] & 0x80 == 0 && (nal[0] >> 1) < 48 && nal[1] & 7 != 0
}

pub(super) fn payloads(frame: &[u8], limit: usize) -> Result<Vec<Vec<u8>>, String> {
    let mut payloads = Vec::new();
    for (start, end) in nals(frame, valid)? {
        let nal = &frame[start..end];
        if nal.len() <= limit {
            payloads.push(nal.to_vec());
            continue;
        }
        // The FU payload header keeps F, LayerId and TID from the NAL header.
        let header = [(nal[0] & 0x81) | (FU << 1), nal[1]];
        let kind = (nal[0] >> 1) & 63;
        let chunks = (nal.len() - 2).div_ceil(limit - 3);
        for (part, chunk) in nal[2..].chunks(limit - 3).enumerate() {
            let mut payload = header.to_vec();
            payload.push(
                kind | if part == 0 { 0x80 } else { 0 } | if part + 1 == chunks { 0x40 } else { 0 },
            );
            payload.extend_from_slice(chunk);
            payloads.push(payload);
        }
    }
    Ok(payloads)
}
