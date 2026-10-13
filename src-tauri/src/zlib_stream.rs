use flate2::{Decompress, FlushDecompress, Status};
use std::io;

/// Discord gateway zlib-stream context. One inflater lives for the whole socket.
pub struct ZlibStream {
    dec: Decompress,
}

impl ZlibStream {
    pub fn new() -> Self {
        Self {
            dec: Decompress::new(true),
        }
    }

    pub fn decode(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        let mut offset = 0usize;
        let mut spins = 0u32;
        while offset < input.len() && spins < 10_000 {
            spins += 1;
            let in_before = self.dec.total_in();
            let out_before = self.dec.total_out();
            let status = self
                .dec
                .decompress(&input[offset..], &mut buf, FlushDecompress::Sync)?;
            let consumed = (self.dec.total_in() - in_before) as usize;
            let produced = (self.dec.total_out() - out_before) as usize;
            if produced > 0 {
                out.extend_from_slice(&buf[..produced]);
            }
            if consumed == 0 && produced == 0 {
                break;
            }
            offset += consumed;
            if status == Status::StreamEnd {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    #[test]
    fn round_trip_sync_flush() {
        let raw = br#"{"op":10,"d":{"heartbeat_interval":41250},"s":null,"t":null}"#;
        let mut compressor = Compress::new(Compression::default(), true);
        let mut compressed = vec![0u8; raw.len() + 64];
        compressor
            .compress(raw, &mut compressed, FlushCompress::Sync)
            .unwrap();
        let n = compressor.total_out() as usize;
        let mut stream = ZlibStream::new();
        let decoded = stream.decode(&compressed[..n]).unwrap();
        assert_eq!(decoded, raw);
    }
}
