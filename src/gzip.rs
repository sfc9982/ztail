//! Streaming multi-member gzip.
//!
//! A gzip file is a sequence of members (RFC 1952). Each member is a 10-byte
//! header, optional extra/name/comment fields, a raw DEFLATE stream, then an
//! 8-byte trailer (CRC-32 and ISIZE). `gzip >> file` appends a new member; it
//! does not extend the previous DEFLATE stream.
//!
//! [`GzipDecoder`] keeps one [`flate2::Decompress`] alive for the current
//! member. Bytes already inflated stay inside that state. When a member ends
//! the inflater is reset and the next header is parsed. Calling code never
//! rewinds the file: it only passes bytes it has not passed before.
//!
//! Per-call work is proportional to the new input. Unconsumed trailer/header
//! fragments are parked in an internal buffer so the caller can advance its
//! file offset by the full slice length.

use crc32fast::Hasher;
use flate2::{Decompress, FlushDecompress, Status};

const FHCRC: u8 = 1 << 1;
const FEXTRA: u8 = 1 << 2;
const FNAME: u8 = 1 << 3;
const FCOMMENT: u8 = 1 << 4;
const RESERVED: u8 = 0xE0;

const HEADER_LEN: usize = 10;
const TRAILER_LEN: usize = 8;
const OUT_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
enum State {
    /// Waiting for the fixed 10-byte header (also the state between members).
    FixedHeader,
    ExtraLen,
    Extra,
    Name,
    Comment,
    Hcrc,
    Body,
    Trailer,
}

pub struct GzipDecoder {
    state: State,
    inflate: Decompress,
    flags: u8,
    extra_remaining: usize,
    crc: Hasher,
    uncomp_len: u32,
    /// Compressed bytes actually handed to [`Decompress`], excluding gzip framing.
    inflate_bytes: u64,
    /// Unconsumed input. `pos` is the first byte not yet parsed.
    buf: Vec<u8>,
    pos: usize,
}

impl GzipDecoder {
    pub fn new() -> Self {
        Self {
            state: State::FixedHeader,
            // `false` = raw DEFLATE. Gzip stores raw DEFLATE, not zlib-wrapped.
            inflate: Decompress::new(false),
            flags: 0,
            extra_remaining: 0,
            crc: Hasher::new(),
            uncomp_len: 0,
            inflate_bytes: 0,
            buf: Vec::new(),
            pos: 0,
        }
    }

    pub fn inflate_bytes(&self) -> u64 {
        self.inflate_bytes
    }

    /// True when we are between members with no buffered input: a finished
    /// stream, or a decoder that has not seen a header yet.
    pub fn at_member_boundary(&self) -> bool {
        matches!(self.state, State::FixedHeader) && self.pos >= self.buf.len()
    }

    /// Accept the next slice of compressed bytes and append decompressed output.
    ///
    /// The whole slice is taken. Anything that cannot be parsed yet (a partial
    /// header, a partial trailer, or a partial DEFLATE symbol) stays here.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        if !input.is_empty() {
            self.buf.extend_from_slice(input);
        }
        let result = self.pump(out);
        self.park();
        result
    }

    fn park(&mut self) {
        if self.pos == 0 {
            return;
        }
        if self.pos >= self.buf.len() {
            self.buf.clear();
        } else {
            self.buf.drain(..self.pos);
        }
        self.pos = 0;
    }

    fn pump(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        loop {
            let available = self.buf.len().saturating_sub(self.pos);
            if available == 0 {
                return Ok(());
            }
            match self.state {
                State::FixedHeader => {
                    if available < HEADER_LEN {
                        return Ok(());
                    }
                    self.parse_fixed_header()?;
                    self.pos += HEADER_LEN;
                }
                State::ExtraLen => {
                    if available < 2 {
                        return Ok(());
                    }
                    let b = &self.buf[self.pos..self.pos + 2];
                    self.extra_remaining = u16::from_le_bytes([b[0], b[1]]) as usize;
                    self.pos += 2;
                    self.state = if self.extra_remaining == 0 {
                        self.after_extra()
                    } else {
                        State::Extra
                    };
                }
                State::Extra => {
                    let skip = self.extra_remaining.min(available);
                    self.pos += skip;
                    self.extra_remaining -= skip;
                    if self.extra_remaining == 0 {
                        self.state = self.after_extra();
                    } else {
                        return Ok(());
                    }
                }
                State::Name => {
                    if !self.skip_cstring()? {
                        return Ok(());
                    }
                    self.state = self.after_name();
                }
                State::Comment => {
                    if !self.skip_cstring()? {
                        return Ok(());
                    }
                    self.state = self.after_comment();
                }
                State::Hcrc => {
                    // Header CRC is optional and rarely written. We consume it
                    // so it is not fed to the inflater; we do not validate it.
                    if available < 2 {
                        return Ok(());
                    }
                    self.pos += 2;
                    self.state = State::Body;
                }
                State::Body => {
                    let (consumed, status) = self.inflate_some(out)?;
                    self.pos += consumed;
                    if status == Status::StreamEnd {
                        self.state = State::Trailer;
                        continue;
                    }
                    if consumed == 0 {
                        // Need more input (or the output chunk was the only
                        // thing holding us back, which `inflate_some` already
                        // loops on).
                        return Ok(());
                    }
                }
                State::Trailer => {
                    if available < TRAILER_LEN {
                        return Ok(());
                    }
                    self.check_trailer()?;
                    self.pos += TRAILER_LEN;
                    self.finish_member();
                }
            }
        }
    }

    fn parse_fixed_header(&mut self) -> Result<(), String> {
        let h = &self.buf[self.pos..self.pos + HEADER_LEN];
        if h[0] != 0x1f || h[1] != 0x8b {
            return Err("invalid gzip magic".into());
        }
        if h[2] != 8 {
            return Err(format!(
                "unsupported gzip compression method {} (only deflate/8)",
                h[2]
            ));
        }
        if h[3] & RESERVED != 0 {
            return Err(format!("invalid gzip flag bits {:#04x}", h[3]));
        }
        // Bit 0 (FTEXT) is a hint for the decompressed data and does not
        // change parsing. Bits we do honor: FEXTRA, FNAME, FCOMMENT, FHCRC.
        self.flags = h[3];
        self.state = self.after_fixed();
        Ok(())
    }

    fn after_fixed(&self) -> State {
        if self.flags & FEXTRA != 0 {
            State::ExtraLen
        } else {
            self.after_extra()
        }
    }

    fn after_extra(&self) -> State {
        if self.flags & FNAME != 0 {
            State::Name
        } else {
            self.after_name()
        }
    }

    fn after_name(&self) -> State {
        if self.flags & FCOMMENT != 0 {
            State::Comment
        } else {
            self.after_comment()
        }
    }

    fn after_comment(&self) -> State {
        if self.flags & FHCRC != 0 {
            State::Hcrc
        } else {
            State::Body
        }
    }

    /// Skip a NUL-terminated string. Returns false if the NUL is not buffered yet.
    fn skip_cstring(&mut self) -> Result<bool, String> {
        let rest = &self.buf[self.pos..];
        if let Some(i) = rest.iter().position(|&b| b == 0) {
            self.pos += i + 1;
            Ok(true)
        } else {
            // The bytes seen so far cannot contain the terminator. Drop them
            // so a long filename does not have to sit in memory.
            self.pos = self.buf.len();
            Ok(false)
        }
    }

    fn inflate_some(&mut self, out: &mut Vec<u8>) -> Result<(usize, Status), String> {
        let input_len = self.buf.len() - self.pos;
        let mut consumed = 0;
        let mut status;
        loop {
            let in_before = self.inflate.total_in();
            let out_before = self.inflate.total_out();
            let start = out.len();
            out.resize(start + OUT_CHUNK, 0);
            status = self
                .inflate
                .decompress(
                    &self.buf[self.pos + consumed..],
                    &mut out[start..],
                    FlushDecompress::None,
                )
                .map_err(|e| format!("deflate: {e}"))?;
            let in_delta = (self.inflate.total_in() - in_before) as usize;
            let out_delta = (self.inflate.total_out() - out_before) as usize;
            if in_delta > input_len - consumed {
                return Err("deflate reported consuming more bytes than it was given".into());
            }
            out.truncate(start + out_delta);
            consumed += in_delta;
            self.inflate_bytes += in_delta as u64;
            if out_delta > 0 {
                self.crc.update(&out[start..start + out_delta]);
                self.uncomp_len = self.uncomp_len.wrapping_add(out_delta as u32);
            }
            if status == Status::StreamEnd {
                break;
            }
            // Output buffer filled: the inflater may still have bytes to emit
            // for input it already consumed. Loop with a fresh buffer.
            if out_delta == OUT_CHUNK {
                continue;
            }
            if in_delta == 0 && out_delta == 0 {
                break;
            }
            if consumed == input_len {
                break;
            }
        }
        Ok((consumed, status))
    }

    fn check_trailer(&mut self) -> Result<(), String> {
        let t = &self.buf[self.pos..self.pos + TRAILER_LEN];
        let expect_crc = u32::from_le_bytes(t[0..4].try_into().unwrap());
        let expect_size = u32::from_le_bytes(t[4..8].try_into().unwrap());
        let got_crc = self.crc.clone().finalize();
        if got_crc != expect_crc {
            return Err(format!(
                "gzip crc mismatch (header {expect_crc:#010x}, computed {got_crc:#010x})"
            ));
        }
        if expect_size != self.uncomp_len {
            return Err(format!(
                "gzip size mismatch (header {expect_size}, computed {})",
                self.uncomp_len
            ));
        }
        Ok(())
    }

    fn finish_member(&mut self) {
        self.crc = Hasher::new();
        self.uncomp_len = 0;
        self.flags = 0;
        self.extra_remaining = 0;
        self.inflate.reset(false);
        self.state = State::FixedHeader;
    }
}

impl Default for GzipDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::MultiGzDecoder;
    use flate2::write::GzEncoder;
    use flate2::{Compression, GzBuilder};
    use std::io::{Read, Write};

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn gz_meta(data: &[u8]) -> Vec<u8> {
        let mut enc = GzBuilder::new()
            .filename("app.log")
            .comment("ztail")
            .extra(vec![1, 2, 3, 4])
            .write(Vec::new(), Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn oracle(data: &[u8]) -> Vec<u8> {
        let mut dec = MultiGzDecoder::new(data);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        out
    }

    fn decode_all(data: &[u8]) -> Result<Vec<u8>, String> {
        let mut dec = GzipDecoder::new();
        let mut out = Vec::new();
        dec.feed(data, &mut out)?;
        Ok(out)
    }

    #[test]
    fn single_member_roundtrip() {
        let payload = b"alpha\nbeta\ngamma\n";
        let compressed = gz(payload);
        assert_eq!(decode_all(&compressed).unwrap(), payload);
    }

    #[test]
    fn header_with_name_comment_and_extra() {
        let payload = b"line\n";
        let compressed = gz_meta(payload);
        assert_eq!(decode_all(&compressed).unwrap(), payload);
    }

    #[test]
    fn multi_member_and_arbitrary_splits() {
        let mut data = gz(b"one\ntwo\n");
        data.extend(gz_meta(b"three\nfour\n"));
        data.extend(gz(b""));
        data.extend(gz(b"five\n"));
        let expected = oracle(&data);
        assert_eq!(decode_all(&data).unwrap(), expected);

        let mut splits: Vec<usize> = vec![
            0,
            1,
            2,
            3,
            9,
            10,
            data.len() / 2,
            data.len().saturating_sub(1),
            data.len(),
        ];
        splits.sort_unstable();
        splits.dedup();
        for split in splits {
            let mut dec = GzipDecoder::new();
            let mut out = Vec::new();
            dec.feed(&data[..split], &mut out).unwrap();
            dec.feed(&data[split..], &mut out).unwrap();
            assert_eq!(out, expected, "split at {split}");
        }

        let mut dec = GzipDecoder::new();
        let mut out = Vec::new();
        for byte in &data {
            dec.feed(&[*byte], &mut out).unwrap();
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn incomplete_member_does_not_error() {
        let compressed = gz(b"hello\nworld\n");
        let mut dec = GzipDecoder::new();
        let mut out = Vec::new();
        // Header is 10 bytes; cut inside the deflate stream, before the trailer.
        let cut = (compressed.len() / 2).max(11).min(compressed.len() - 1);
        dec.feed(&compressed[..cut], &mut out).unwrap();
        dec.feed(&compressed[cut..], &mut out).unwrap();
        assert_eq!(out, b"hello\nworld\n");
    }

    #[test]
    fn hcrc_is_not_passed_to_inflate() {
        // Fixed header with FHCRC set, plus two header-crc bytes and no body.
        let header = [0x1f, 0x8b, 8, FHCRC, 0, 0, 0, 0, 0, 255, 0x00, 0x00];
        let mut dec = GzipDecoder::new();
        let mut out = Vec::new();
        dec.feed(&header, &mut out).unwrap();
        assert!(out.is_empty());
        assert_eq!(dec.inflate_bytes(), 0);
    }

    #[test]
    fn bad_method_and_bad_crc() {
        let mut dec = GzipDecoder::new();
        let mut out = Vec::new();
        let bad_method = [0x1f, 0x8b, 0, 0, 0, 0, 0, 0, 0, 255];
        assert!(dec.feed(&bad_method, &mut out).is_err());

        // Trailer layout: 4-byte CRC32, then 4-byte ISIZE. Corrupt the CRC.
        let mut compressed = gz(b"hello\n");
        let crc_byte = compressed.len() - 8;
        compressed[crc_byte] ^= 0xff;
        let err = decode_all(&compressed).unwrap_err();
        assert!(
            err.contains("crc mismatch"),
            "expected crc failure, got {err}"
        );

        let mut isize_bad = gz(b"hello\n");
        let last = isize_bad.len() - 1;
        isize_bad[last] ^= 0xff;
        let err = decode_all(&isize_bad).unwrap_err();
        assert!(
            err.contains("size mismatch"),
            "expected isize failure, got {err}"
        );
    }

    #[test]
    fn at_member_boundary_after_complete_stream() {
        let compressed = gz(b"hello\n");
        let mut dec = GzipDecoder::new();
        let mut out = Vec::new();
        assert!(dec.at_member_boundary());
        dec.feed(&compressed[..compressed.len() / 2], &mut out)
            .unwrap();
        assert!(!dec.at_member_boundary());
        dec.feed(&compressed[compressed.len() / 2..], &mut out)
            .unwrap();
        assert!(dec.at_member_boundary());
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn large_payload_is_fully_emitted() {
        // Bigger than two output chunks, so a full 64 KiB inflate buffer must
        // be drained instead of stalling inside the member.
        let mut payload = Vec::new();
        for i in 0..8_000 {
            writeln!(payload, "row {i:05} {}", "x".repeat(40)).unwrap();
        }
        assert!(payload.len() > 128 * 1024);
        let compressed = gz(&payload);
        assert_eq!(decode_all(&compressed).unwrap(), payload);
    }

    #[test]
    fn inflate_bytes_count_only_deflate_payload() {
        let compressed = gz(b"abc\n");
        let mut dec = GzipDecoder::new();
        let mut out = Vec::new();
        dec.feed(&compressed, &mut out).unwrap();
        // 10 byte header + 8 byte trailer are not inflate input.
        assert!(dec.inflate_bytes() + 18 <= compressed.len() as u64);
        assert!(dec.inflate_bytes() > 0);
    }
}
