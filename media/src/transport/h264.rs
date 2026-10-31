//! H.264 Annex B parsing, RFC 6184 packetization and SPS normalization.
use std::borrow::Cow;

/// A single NAL unit type without the forbidden bit.
pub(super) fn valid_nal(header: u8) -> bool {
    header & 0x80 == 0 && (1..=23).contains(&(header & 31))
}

fn valid(nal: &[u8]) -> bool {
    nal.first().is_some_and(|&header| valid_nal(header))
}

/// Annex B NAL unit ranges, each checked by `valid`.
pub(super) fn nals(frame: &[u8], valid: fn(&[u8]) -> bool) -> Result<Vec<(usize, usize)>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= frame.len() {
        if frame[i..].starts_with(&[0, 0, 1]) {
            let prefix = if i > 0 && frame[i - 1] == 0 { i - 1 } else { i };
            starts.push((prefix, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    if starts.first().is_none_or(|(prefix, _)| *prefix != 0) {
        return Err("Video frame is not Annex B".into());
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (index, &(_, start)) in starts.iter().enumerate() {
        let end = starts
            .get(index + 1)
            .map_or(frame.len(), |(prefix, _)| *prefix);
        if !valid(&frame[start..end]) {
            return Err("Invalid NAL unit".into());
        }
        nals.push((start, end));
    }
    Ok(nals)
}

/// RFC 6184 single NAL units and FU-A fragments from one Annex B frame.
pub(super) fn payloads(frame: &[u8], limit: usize) -> Result<Vec<Vec<u8>>> {
    let mut payloads = Vec::new();
    for (start, end) in nals(frame, valid)? {
        let nal = &frame[start..end];
        let (&header, body) = nal.split_first().unwrap();
        if nal.len() <= limit {
            payloads.push(nal.to_vec());
        } else {
            let chunks = body.len().div_ceil(limit - 2);
            for (part, chunk) in body.chunks(limit - 2).enumerate() {
                let mut payload = vec![
                    (header & 0xe0) | 28,
                    (header & 31)
                        | if part == 0 { 0x80 } else { 0 }
                        | if part + 1 == chunks { 0x40 } else { 0 },
                ];
                payload.extend_from_slice(chunk);
                payloads.push(payload);
            }
        }
    }
    Ok(payloads)
}

/// Keep baseline SPS bytes stable through WebRTC's receive-side VUI rewriter.
pub(super) fn normalize(frame: &[u8]) -> Result<Cow<'_, [u8]>> {
    let mut output = None;
    let mut copied = 0;
    for (start, end) in nals(frame, valid)? {
        if frame[start] & 31 != 7 {
            continue;
        }
        if let Some(sps) = rewrite_sps(&frame[start + 1..end])? {
            let out = output.get_or_insert_with(|| Vec::with_capacity(frame.len() + 16));
            out.extend_from_slice(&frame[copied..start + 1]);
            out.extend(sps);
            copied = end;
        }
    }
    Ok(match output {
        Some(mut out) => {
            out.extend_from_slice(&frame[copied..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(frame),
    })
}

const INVALID: &str = "Invalid or unsupported baseline H.264 SPS";
type Result<T> = std::result::Result<T, String>;

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<u32> {
        if count > 32 || self.pos + count > self.bytes.len() * 8 {
            return Err(INVALID.into());
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | u32::from((self.bytes[self.pos / 8] >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Ok(value)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }
    fn ue(&mut self, max: u32) -> Result<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err(INVALID.into());
            }
        }
        let value = ((1u32 << zeros) - 1) + self.read(zeros)?;
        if value > max {
            return Err(INVALID.into());
        }
        Ok(value)
    }
    fn hrd(&mut self) -> Result<()> {
        let count = self.ue(31)? + 1;
        self.read(8)?; // bit rate and CPB size scales
        for _ in 0..count {
            self.ue(u32::MAX - 1)?;
            self.ue(u32::MAX - 1)?;
            self.read(1)?;
        }
        self.read(20)?; // delay and time offset lengths
        Ok(())
    }
}

fn rewrite_sps(escaped: &[u8]) -> Result<Option<Vec<u8>>> {
    if escaped.len() > 4096 {
        return Err(INVALID.into());
    }
    let mut rbsp = Vec::with_capacity(escaped.len());
    let mut zeros = 0;
    for (i, &byte) in escaped.iter().enumerate() {
        if zeros == 2 && byte == 3 {
            if escaped.get(i + 1).is_none_or(|next| *next > 3) {
                return Err(INVALID.into());
            }
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let mut b = Bits {
        bytes: &rbsp,
        pos: 0,
    };
    if b.read(8)? != 66 {
        return Err(INVALID.into());
    }
    b.read(16)?; // constraints and level
    b.ue(31)?; // seq_parameter_set_id
    b.ue(12)?; // log2_max_frame_num_minus4
    match b.ue(2)? {
        0 => {
            b.ue(12)?;
        }
        1 => {
            b.read(1)?; // delta_pic_order_always_zero_flag
            b.ue(u32::MAX - 1)?; // signed offset_for_non_ref_pic
            b.ue(u32::MAX - 1)?; // signed offset_for_top_to_bottom_field
            for _ in 0..b.ue(255)? {
                b.ue(u32::MAX - 1)?; // signed offset_for_ref_frame
            }
        }
        2 => {}
        _ => unreachable!(),
    }
    let refs = b.ue(16)?;
    b.read(1)?; // gaps_in_frame_num_value_allowed_flag
    b.ue(65535)?; // pic_width_in_mbs_minus1
    b.ue(65535)?; // pic_height_in_map_units_minus1
    if !b.flag()? {
        b.read(1)?;
    } // mb_adaptive_frame_field_flag
    b.read(1)?; // direct_8x8_inference_flag
    if b.flag()? {
        for _ in 0..4 {
            b.ue(65535)?;
        } // frame crop offsets
    }
    let vui_pos = b.pos;
    let vui = b.flag()?;
    if vui {
        if b.flag()? && b.read(8)? == 255 {
            b.read(32)?;
        } // aspect ratio
        if b.flag()? {
            b.read(1)?;
        } // overscan
        if b.flag()? {
            b.read(4)?; // video format and full range
            if b.flag()? {
                b.read(24)?;
            } // colour description
        }
        if b.flag()? {
            b.ue(5)?;
            b.ue(5)?;
        } // chroma location
        if b.flag()? {
            b.read(32)?;
            b.read(32)?;
            b.read(1)?;
        } // timing
        let nal_hrd = b.flag()?;
        if nal_hrd {
            b.hrd()?;
        }
        let vcl_hrd = b.flag()?;
        if vcl_hrd {
            b.hrd()?;
        }
        if nal_hrd || vcl_hrd {
            b.read(1)?;
        }
        b.read(1)?; // pic_struct_present_flag
    }
    let restriction_pos = b.pos;
    let restricted = vui && b.flag()?;
    let mut accepted = false;
    if restricted {
        b.read(1)?; // motion_vectors_over_pic_boundaries_flag
        b.ue(16)?; // max_bytes_per_pic_denom
        b.ue(16)?; // max_bits_per_mb_denom
        b.ue(16)?; // log2_max_mv_length_horizontal
        b.ue(16)?; // log2_max_mv_length_vertical
        let reorder = b.ue(16)?;
        let buffering = b.ue(16)?;
        accepted = reorder == 0 && buffering <= refs;
    }
    // SPS ends here. Check rbsp_stop_one_bit and padding before any rewrite.
    if !b.flag()? {
        return Err(INVALID.into());
    }
    while b.pos < rbsp.len() * 8 {
        if b.flag()? {
            return Err(INVALID.into());
        }
    }
    if accepted {
        return Ok(None);
    }

    let mut out = Writer {
        bytes: Vec::new(),
        pos: 0,
    };
    let prefix = if vui { restriction_pos } else { vui_pos };
    for bit in 0..prefix {
        out.bit((rbsp[bit / 8] >> (7 - bit % 8)) & 1 != 0);
    }
    if !vui {
        out.bit(true); // vui_parameters_present_flag
        for _ in 0..8 {
            out.bit(false);
        } // optional VUI fields absent
    }
    // WebRTC common_video/h264/sps_vui_rewriter.cc AddBitstreamRestriction.
    out.bit(true);
    out.bit(true);
    for value in [2, 1, 16, 16, 0, refs] {
        out.ue(value);
    }
    out.bit(true); // rbsp_stop_one_bit, remaining byte bits are already zero
    let mut escaped = Vec::with_capacity(out.bytes.len() + 4);
    zeros = 0;
    for byte in out.bytes {
        if zeros == 2 && byte <= 3 {
            escaped.push(3);
            zeros = 0;
        }
        escaped.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    Ok(Some(escaped))
}

struct Writer {
    bytes: Vec<u8>,
    pos: usize,
}
impl Writer {
    fn bit(&mut self, value: bool) {
        if self.pos.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if value {
            self.bytes[self.pos / 8] |= 1 << (7 - self.pos % 8);
        }
        self.pos += 1;
    }
    fn ue(&mut self, value: u32) {
        let code = value + 1;
        let width = 32 - code.leading_zeros();
        for _ in 1..width {
            self.bit(false);
        }
        for shift in (0..width).rev() {
            self.bit(code & (1 << shift) != 0);
        }
    }
}
