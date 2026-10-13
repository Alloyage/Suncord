use audiopus_sys::{
    opus_decode_float, opus_decoder_create, opus_decoder_destroy, opus_encode_float,
    opus_encoder_create, opus_encoder_destroy, OpusDecoder, OpusEncoder, OPUS_APPLICATION_VOIP,
};
use std::os::raw::c_int;

pub const FRAME_SAMPLES: usize = 960;
/// Discord comfort noise / silence Opus frame (20 ms).
pub const SILENCE_OPUS: &[u8] = &[0xf8, 0xff, 0xfe];

pub struct Encoder {
    raw: *mut OpusEncoder,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let raw = unsafe { opus_encoder_create(48_000, 2, OPUS_APPLICATION_VOIP, &mut error) };
        if raw.is_null() || error != 0 {
            return Err(format!("opus encoder did not start ({error})"));
        }
        Ok(Self { raw })
    }

    /// Interleaved stereo PCM at 48 kHz, exactly 20 ms (960 frames).
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, String> {
        if pcm.len() != FRAME_SAMPLES * 2 {
            return Err("voice frame must be 20 ms of 48 kHz stereo".into());
        }
        let mut output = vec![0u8; 4_000];
        let written = unsafe {
            opus_encode_float(
                self.raw,
                pcm.as_ptr(),
                FRAME_SAMPLES as c_int,
                output.as_mut_ptr(),
                output.len() as c_int,
            )
        };
        if written < 0 {
            return Err(format!("opus encode failed ({written})"));
        }
        output.truncate(written as usize);
        Ok(output)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { opus_encoder_destroy(self.raw) }
            self.raw = std::ptr::null_mut();
        }
    }
}

pub struct Decoder {
    raw: *mut OpusDecoder,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let raw = unsafe { opus_decoder_create(48_000, 2, &mut error) };
        if raw.is_null() || error != 0 {
            return Err(format!("opus decoder did not start ({error})"));
        }
        Ok(Self { raw })
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>, String> {
        let mut pcm = vec![0f32; FRAME_SAMPLES * 2];
        let written = unsafe {
            opus_decode_float(
                self.raw,
                if packet.is_empty() {
                    std::ptr::null()
                } else {
                    packet.as_ptr()
                },
                packet.len() as c_int,
                pcm.as_mut_ptr(),
                FRAME_SAMPLES as c_int,
                0,
            )
        };
        if written < 0 {
            return Err(format!("opus decode failed ({written})"));
        }
        pcm.truncate((written as usize) * 2);
        Ok(pcm)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { opus_decoder_destroy(self.raw) }
            self.raw = std::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_silence() {
        let mut encoder = Encoder::new().unwrap();
        let packet = encoder.encode(&[0.0; FRAME_SAMPLES * 2]).unwrap();
        assert!(!packet.is_empty());
        assert!(packet.len() < 200);
    }
}
