use flate2::{Decompress, FlushDecompress, Status};
use std::io;

const MAX_COMPRESSED: usize = 16 * 1024 * 1024;
const MAX_DECODED: usize = 64 * 1024 * 1024;

/// Discord gateway `compress=zlib-stream` inflater.
/// Bytes are buffered until a ZLIB sync flush trailer (`00 00 FF FF`), then inflated
/// with a connection-long context. READY payloads often span multiple WS frames.
pub struct ZlibStream {
    dec: Decompress,
    pending: Vec<u8>,
}

impl ZlibStream {
    pub fn new() -> Self {
        Self {
            dec: Decompress::new(true),
            pending: Vec::with_capacity(64 * 1024),
        }
    }

    /// Push a binary WebSocket chunk. Returns `Ok(Some(json))` when a full message
    /// has been flushed; `Ok(None)` while still accumulating.
    pub fn push(&mut self, chunk: &[u8]) -> io::Result<Option<Vec<u8>>> {
        if chunk.is_empty() {
            return Ok(None);
        }
        if self.pending.len().saturating_add(chunk.len()) > MAX_COMPRESSED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gateway compressed message exceeded its size limit",
            ));
        }
        self.pending.extend_from_slice(chunk);
        if self.pending.len() < 4
            || self.pending[self.pending.len() - 4..] != [0x00, 0x00, 0xFF, 0xFF]
        {
            return Ok(None);
        }

        let input = std::mem::take(&mut self.pending);
        let mut out = Vec::with_capacity(input.len().saturating_mul(4).max(4096));
        let mut buf = [0u8; 32 * 1024];
        let mut offset = 0usize;
        loop {
            let in_before = self.dec.total_in();
            let out_before = self.dec.total_out();
            let status = self
                .dec
                .decompress(&input[offset..], &mut buf, FlushDecompress::Sync)?;
            let consumed = (self.dec.total_in() - in_before) as usize;
            let produced = (self.dec.total_out() - out_before) as usize;
            if produced > 0 {
                if out.len() + produced > MAX_DECODED {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "gateway decoded message exceeded its size limit",
                    ));
                }
                out.extend_from_slice(&buf[..produced]);
            }
            if consumed == 0 && produced == 0 {
                if offset < input.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "gateway decompression stalled",
                    ));
                }
                break;
            }
            offset += consumed;
            match status {
                Status::Ok | Status::BufError => {
                    if offset == input.len() && produced < buf.len() {
                        break;
                    }
                }
                Status::StreamEnd => break,
            }
        }
        if out.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zlib-stream produced an empty gateway message",
            ));
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    fn compress_message(raw: &[u8]) -> Vec<u8> {
        let mut compressor = Compress::new(Compression::default(), true);
        let mut compressed = vec![0u8; raw.len() + 64];
        compressor
            .compress(raw, &mut compressed, FlushCompress::Sync)
            .unwrap();
        compressed.truncate(compressor.total_out() as usize);
        compressed
    }

    #[test]
    fn round_trip_flush_trailer() {
        let raw = br#"{"op":10,"d":{"heartbeat_interval":41250},"s":null,"t":null}"#;
        let compressed = compress_message(raw);
        assert_eq!(
            &compressed[compressed.len() - 4..],
            &[0x00, 0x00, 0xFF, 0xFF]
        );
        let mut stream = ZlibStream::new();
        let decoded = stream.push(&compressed).unwrap().unwrap();
        assert_eq!(decoded, raw);
    }

    #[test]
    fn split_frames_wait_for_trailer() {
        let raw = br#"{"t":"READY","op":0,"s":1,"d":{"session_id":"abc"}}"#;
        let compressed = compress_message(raw);
        let mid = compressed.len() / 2;
        let mut stream = ZlibStream::new();
        assert!(stream.push(&compressed[..mid]).unwrap().is_none());
        let decoded = stream.push(&compressed[mid..]).unwrap().unwrap();
        assert_eq!(decoded, raw);
    }
}
