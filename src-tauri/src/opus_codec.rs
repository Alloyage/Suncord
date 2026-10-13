use std::os::raw::{c_int, c_void};

const APPLICATION_VOIP: c_int = 2048;
pub const FRAME_SAMPLES: usize = 960;

pub struct Encoder {
    raw: *mut c_void,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let raw = unsafe { opus_encoder_create(48_000, 2, APPLICATION_VOIP, &mut error) };
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

#[link(name = "opus", kind = "static")]
extern "C" {
    fn opus_encoder_create(
        sample_rate: c_int,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encode_float(
        encoder: *mut c_void,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: c_int,
    ) -> c_int;
    fn opus_encoder_destroy(encoder: *mut c_void);
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
