//! Format detection and a single streaming decoder.
//!
//! The first bytes select gzip, zstd, xz (rejected), or plain text. Detection
//! waits while the buffer is still a proper prefix of a known magic, so a
//! short read of `1f` is not mistaken for text that later becomes a gzip file.
//! After the format is chosen the same decoder object is reused for the rest
//! of the file (until truncation or rotation resets it).

use crate::gzip::GzipDecoder;

const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
const XZ_MAGIC: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];
#[cfg(feature = "zstd")]
const OUT_CHUNK: usize = 64 * 1024;

enum Kind {
    /// Not enough bytes to distinguish a magic prefix from plain text.
    Sniff(Vec<u8>),
    Gzip(Box<GzipDecoder>),
    Plain,
    #[cfg(feature = "zstd")]
    Zstd(Box<ZstdDec>),
}

enum Detected {
    NeedMore,
    Gzip,
    Zstd,
    Xz,
    Plain,
}

pub struct Decoder {
    kind: Kind,
    /// Every byte accepted from the file since this decoder was created.
    /// Not cleared when a gzip member ends. A poll that re-read the file
    /// would add the old length again; tests assert it does not.
    bytes_fed: u64,
    /// Compressed-stream input from decoders that have already been reset.
    inflate_accum: u64,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            kind: Kind::Sniff(Vec::new()),
            bytes_fed: 0,
            inflate_accum: 0,
        }
    }

    pub fn is_plain_prefix(prefix: &[u8]) -> bool {
        matches!(detect(prefix), Detected::Plain)
    }

    pub fn select_plain(&mut self) {
        self.kind = Kind::Plain;
    }

    /// Bytes accepted from the file. The CLI does not read this; tests do,
    /// to prove a later poll does not feed the whole file again.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bytes_fed(&self) -> u64 {
        self.bytes_fed
    }

    /// Bytes passed into the compressed-stream decoder (raw DEFLATE or zstd),
    /// excluding gzip framing this crate parses itself. Plain text counts as 0.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn inflate_bytes(&self) -> u64 {
        self.inflate_accum
            + match &self.kind {
                Kind::Gzip(g) => g.inflate_bytes(),
                #[cfg(feature = "zstd")]
                Kind::Zstd(z) => z.fed,
                _ => 0,
            }
    }

    pub fn reset(&mut self) {
        self.absorb_inflate();
        self.kind = Kind::Sniff(Vec::new());
    }

    fn absorb_inflate(&mut self) {
        let extra = match &self.kind {
            Kind::Gzip(g) => g.inflate_bytes(),
            #[cfg(feature = "zstd")]
            Kind::Zstd(z) => z.fed,
            _ => 0,
        };
        self.inflate_accum += extra;
    }

    #[cfg(test)]
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        self.feed_limited(input, out, usize::MAX)
    }

    pub fn needs_drain(&self) -> bool {
        match &self.kind {
            Kind::Gzip(dec) => dec.needs_drain(),
            #[cfg(feature = "zstd")]
            Kind::Zstd(dec) => dec.needs_drain,
            _ => false,
        }
    }

    pub fn feed_limited(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), String> {
        if input.is_empty() && !self.needs_drain() {
            return Ok(());
        }
        match &mut self.kind {
            Kind::Sniff(buf) => {
                buf.extend_from_slice(input);
                self.bytes_fed += input.len() as u64;
                match detect(buf) {
                    Detected::NeedMore => Ok(()),
                    Detected::Plain => {
                        let buffered = match std::mem::replace(&mut self.kind, Kind::Plain) {
                            Kind::Sniff(buffered) => buffered,
                            _ => unreachable!("kind was sniff"),
                        };
                        out.extend_from_slice(&buffered);
                        Ok(())
                    }
                    Detected::Gzip => {
                        let buffered = take_sniff(&mut self.kind);
                        self.kind = Kind::Gzip(Box::new(GzipDecoder::new()));
                        self.push_decoded(&buffered, out, limit)
                    }
                    Detected::Zstd => {
                        let buffered = take_sniff(&mut self.kind);
                        self.kind = Kind::open_zstd()?;
                        self.push_decoded(&buffered, out, limit)
                    }
                    Detected::Xz => Err(
                        "xz/lzma is not supported (a static musl build would need liblzma)".into(),
                    ),
                }
            }
            Kind::Plain => {
                self.bytes_fed += input.len() as u64;
                out.extend_from_slice(input);
                Ok(())
            }
            Kind::Gzip(_) => {
                self.bytes_fed += input.len() as u64;
                self.push_decoded(input, out, limit)
            }
            #[cfg(feature = "zstd")]
            Kind::Zstd(_) => {
                self.bytes_fed += input.len() as u64;
                self.push_decoded(input, out, limit)
            }
        }
    }

    /// End of a non-following read.
    ///
    /// A leftover magic prefix is plain text: the file ended before it could
    /// become a real compressed stream. An incomplete gzip member or zstd
    /// frame is an error so truncated archives are not reported as success.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        match &mut self.kind {
            Kind::Sniff(buf) => {
                if buf.is_empty() {
                    return Ok(());
                }
                let buffered = take_sniff(&mut self.kind);
                self.kind = Kind::Plain;
                out.extend_from_slice(&buffered);
                Ok(())
            }
            Kind::Plain => Ok(()),
            Kind::Gzip(dec) => {
                if dec.at_member_boundary() {
                    Ok(())
                } else {
                    Err("truncated gzip stream (incomplete member)".into())
                }
            }
            #[cfg(feature = "zstd")]
            Kind::Zstd(dec) => dec.finish(out),
        }
    }

    fn push_decoded(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), String> {
        match &mut self.kind {
            Kind::Plain => {
                out.extend_from_slice(input);
                Ok(())
            }
            Kind::Gzip(dec) => dec.feed_limited(input, out, limit),
            #[cfg(feature = "zstd")]
            Kind::Zstd(dec) => dec.feed_limited(input, out, limit),
            Kind::Sniff(_) => unreachable!("push_decoded on sniff"),
        }
    }
}

impl Kind {
    fn open_zstd() -> Result<Self, String> {
        #[cfg(feature = "zstd")]
        {
            Ok(Kind::Zstd(Box::new(ZstdDec::new()?)))
        }
        #[cfg(not(feature = "zstd"))]
        {
            Err(
                "this file looks like zstd, but ztail was built without zstd support \
                 (rebuild with --features zstd)"
                    .into(),
            )
        }
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

fn take_sniff(kind: &mut Kind) -> Vec<u8> {
    match std::mem::replace(kind, Kind::Plain) {
        Kind::Sniff(buffered) => buffered,
        _ => unreachable!("expected sniff state"),
    }
}

fn detect(buf: &[u8]) -> Detected {
    if buf.starts_with(GZIP_MAGIC) {
        return Detected::Gzip;
    }
    if buf.len() >= ZSTD_MAGIC.len() && buf.starts_with(ZSTD_MAGIC) {
        return Detected::Zstd;
    }
    if buf.len() >= XZ_MAGIC.len() && buf.starts_with(XZ_MAGIC) {
        return Detected::Xz;
    }
    if is_proper_prefix(buf, GZIP_MAGIC)
        || is_proper_prefix(buf, ZSTD_MAGIC)
        || is_proper_prefix(buf, XZ_MAGIC)
    {
        return Detected::NeedMore;
    }
    Detected::Plain
}

fn is_proper_prefix(buf: &[u8], magic: &[u8]) -> bool {
    !buf.is_empty() && buf.len() < magic.len() && magic.starts_with(buf)
}

/// Streaming zstd. Unconsumed input is parked because `run_on_buffers` will
/// not keep a partial frame that it could not finish.
#[cfg(feature = "zstd")]
struct ZstdDec {
    dec: zstd::stream::raw::Decoder<'static>,
    fed: u64,
    /// Unconsumed tail from a previous feed. Empty on the fast path so the
    /// next chunk is decoded in place.
    buf: Vec<u8>,
    /// True after we have accepted frame bytes and before `remaining == 0`.
    in_frame: bool,
    buf_pos: usize,
    needs_drain: bool,
}

#[cfg(feature = "zstd")]
impl ZstdDec {
    fn new() -> Result<Self, String> {
        let mut dec = zstd::stream::raw::Decoder::new().map_err(|e| format!("zstd: {e}"))?;
        // A hostile frame may request a giant history window before emitting
        // any bytes. Bound decoder-internal memory, not just our output Vec.
        dec.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(26))
            .map_err(|e| format!("zstd: {e}"))?;
        Ok(Self {
            dec,
            fed: 0,
            buf: Vec::new(),
            in_frame: false,
            buf_pos: 0,
            needs_drain: false,
        })
    }

    fn feed_limited(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), String> {
        let result = if self.buf.is_empty() && !input.is_empty() {
            let mut pos = 0;
            let result = self.pump(input, &mut pos, out, limit);
            self.buf.extend_from_slice(&input[pos..]);
            result
        } else {
            if !input.is_empty() {
                self.buf.drain(..self.buf_pos);
                self.buf_pos = 0;
                self.buf.extend_from_slice(input);
            }
            let mut data = std::mem::take(&mut self.buf);
            let mut pos = self.buf_pos;
            let result = self.pump(&data, &mut pos, out, limit);
            if pos == data.len() {
                data.clear();
                pos = 0;
            }
            self.buf = data;
            self.buf_pos = pos;
            result
        };
        self.needs_drain = out.len() >= limit;
        result
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        self.feed_limited(&[], out, usize::MAX)?;
        if !self.buf.is_empty() || self.in_frame {
            return Err("truncated zstd stream (incomplete frame)".into());
        }
        Ok(())
    }

    fn pump(
        &mut self,
        data: &[u8],
        pos: &mut usize,
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), String> {
        use zstd::stream::raw::Operation;

        loop {
            if out.len() >= limit {
                return Ok(());
            }
            let input = if *pos < data.len() {
                &data[*pos..]
            } else {
                &[]
            };
            let start = out.len();
            let capacity = OUT_CHUNK.min(limit - start);
            out.resize(start + capacity, 0);
            let status = self
                .dec
                .run_on_buffers(input, &mut out[start..])
                .map_err(|e| {
                    out.truncate(start);
                    format!("zstd: {e}")
                })?;
            out.truncate(start + status.bytes_written);
            *pos += status.bytes_read;
            self.fed += status.bytes_read as u64;
            if status.bytes_read > 0 || status.bytes_written > 0 {
                self.in_frame = true;
            }
            if status.remaining == 0 && (status.bytes_read > 0 || status.bytes_written > 0) {
                // End of a frame. More frames may follow in the buffer.
                self.in_frame = false;
            }
            if status.bytes_written == capacity {
                // Output buffer was full; decoder may still have pending bytes
                // even with no remaining input.
                continue;
            }
            if *pos >= data.len() {
                // No more input. Stop unless we just filled the output buffer
                // (handled above). Pending incomplete frames stay in `in_frame`.
                return Ok(());
            }
            if status.bytes_read == 0 && status.bytes_written == 0 {
                // Need more input to continue the current frame.
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use proptest::prelude::*;
    use std::io::Write;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn plain_text_and_magic_prefix() {
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        // A single 0x1f could still become gzip. Hold it.
        dec.feed(&[0x1f], &mut out).unwrap();
        assert!(out.is_empty());
        // The next byte is not 0x8b, so this is text.
        dec.feed(b" hello\n", &mut out).unwrap();
        assert_eq!(out, b"\x1f hello\n");
        assert_eq!(dec.bytes_fed(), 8);
    }

    #[test]
    fn finish_promotes_short_prefix_to_text() {
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        dec.feed(&[0x1f], &mut out).unwrap();
        dec.finish(&mut out).unwrap();
        assert_eq!(out, [0x1f]);
    }

    #[test]
    fn finish_rejects_truncated_gzip() {
        let compressed = gz(b"hello\nworld\n");
        let cut = compressed.len() / 2;
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        dec.feed(&compressed[..cut], &mut out).unwrap();
        let err = dec.finish(&mut out).unwrap_err();
        assert!(err.contains("truncated"), "{err}");
    }

    #[test]
    fn finish_accepts_complete_gzip() {
        let compressed = gz(b"hello\n");
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        dec.feed(&compressed, &mut out).unwrap();
        dec.finish(&mut out).unwrap();
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn xz_is_rejected() {
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        let magic = [0xfd, b'7', b'z', b'X', b'Z', 0x00];
        let err = dec.feed(&magic, &mut out).unwrap_err();
        assert!(err.contains("xz"), "{err}");
    }

    #[cfg(not(feature = "zstd"))]
    #[test]
    fn zstd_without_feature_is_an_error() {
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        let err = dec.feed(&[0x28, 0xb5, 0x2f, 0xfd], &mut out).unwrap_err();
        assert!(err.contains("zstd"), "{err}");
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_roundtrip_and_concatenated_frames() {
        let frame_a = zstd::encode_all(&b"alpha\n"[..], 1).unwrap();
        let frame_b = zstd::encode_all(&b"beta\n"[..], 1).unwrap();
        let mut blob = frame_a.clone();
        blob.extend_from_slice(&frame_b);

        let mut dec = Decoder::new();
        let mut out = Vec::new();
        let mid = frame_a.len() / 2;
        dec.feed(&blob[..mid], &mut out).unwrap();
        dec.feed(&blob[mid..], &mut out).unwrap();
        dec.finish(&mut out).unwrap();
        assert_eq!(out, b"alpha\nbeta\n");
        assert_eq!(dec.bytes_fed(), blob.len() as u64);
        assert!(dec.inflate_bytes() > 0);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_drains_large_output_from_one_feed() {
        // Highly compressible payload: one feed can fill multiple 64 KiB
        // output buffers from a short compressed frame.
        let payload = vec![b'a'; 200_000];
        let frame = zstd::encode_all(&payload[..], 1).unwrap();
        assert!(frame.len() < 64 * 1024);

        let mut dec = Decoder::new();
        let mut out = Vec::new();
        dec.feed(&frame, &mut out).unwrap();
        dec.finish(&mut out).unwrap();
        assert_eq!(out.len(), payload.len());
        assert!(out.iter().all(|&b| b == b'a'));
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn finish_rejects_truncated_zstd() {
        let frame = zstd::encode_all(&b"hello world\n"[..], 1).unwrap();
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        dec.feed(&frame[..frame.len() / 2], &mut out).unwrap();
        let err = dec.finish(&mut out).unwrap_err();
        assert!(err.contains("truncated"), "{err}");
    }

    fn force_plain(mut data: Vec<u8>) -> Vec<u8> {
        if !matches!(detect(&data), Detected::Plain) {
            data.insert(0, b'x');
        }
        data
    }

    fn feed_chunks(dec: &mut Decoder, data: &[u8], raw_splits: Vec<usize>) -> Vec<u8> {
        let mut points: Vec<usize> = raw_splits
            .into_iter()
            .filter(|&point| point <= data.len())
            .collect();
        points.sort_unstable();
        points.dedup();

        let mut out = Vec::new();
        let mut start = 0;
        for end in points.into_iter().chain(std::iter::once(data.len())) {
            if end <= start {
                continue;
            }
            dec.feed(&data[start..end], &mut out).unwrap();
            start = end;
        }
        dec.finish(&mut out).unwrap();
        out
    }

    proptest! {
        #[test]
        fn plain_bytes_survive_arbitrary_splits(
            data in prop::collection::vec(any::<u8>(), 0..512),
            raw_splits in prop::collection::vec(0usize..=512, 0..16),
        ) {
            let data = force_plain(data);
            let mut dec = Decoder::new();
            let out = feed_chunks(&mut dec, &data, raw_splits);
            prop_assert_eq!(&out, &data);
        }

        #[test]
        fn gzip_bytes_survive_arbitrary_splits(
            payload in prop::collection::vec(any::<u8>(), 0..512),
            raw_splits in prop::collection::vec(0usize..=2048, 0..16),
        ) {
            let blob = gz(&payload);
            let mut dec = Decoder::new();
            let out = feed_chunks(&mut dec, &blob, raw_splits);
            prop_assert_eq!(&out, &payload);
            prop_assert_eq!(dec.bytes_fed(), blob.len() as u64);
        }
    }

    #[cfg(feature = "zstd")]
    proptest! {
        #[test]
        fn zstd_bytes_survive_arbitrary_splits(
            payload in prop::collection::vec(any::<u8>(), 0..512),
            raw_splits in prop::collection::vec(0usize..=2048, 0..16),
        ) {
            let blob = zstd::encode_all(&payload[..], 1).unwrap();
            let mut dec = Decoder::new();
            let out = feed_chunks(&mut dec, &blob, raw_splits);
            prop_assert_eq!(&out, &payload);
            prop_assert_eq!(dec.bytes_fed(), blob.len() as u64);
        }
    }

    #[test]
    fn bounded_gzip_handles_input_and_output_boundaries() {
        let raw: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
        let mut compressed = gz(&raw);
        compressed.extend_from_slice(&gz(b"next\n"));
        for chunk in [1, 7, 64, 4096] {
            for limit in [1, 17, 1024] {
                let mut dec = Decoder::new();
                let mut got = Vec::new();
                let mut out = Vec::new();
                for input in compressed.chunks(chunk) {
                    out.clear();
                    dec.feed_limited(input, &mut out, limit).unwrap();
                    assert!(out.len() <= limit);
                    got.extend_from_slice(&out);
                    while dec.needs_drain() {
                        out.clear();
                        dec.feed_limited(&[], &mut out, limit).unwrap();
                        assert!(out.len() <= limit);
                        got.extend_from_slice(&out);
                    }
                }
                out.clear();
                dec.finish(&mut out).unwrap();
                got.extend_from_slice(&out);
                let mut expected = raw.clone();
                expected.extend_from_slice(b"next\n");
                assert_eq!(got, expected, "chunk={chunk}, limit={limit}");
            }
        }
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn bounded_zstd_resumes_concatenated_high_ratio_frames() {
        let raw = vec![b'x'; 3 * 1024 * 1024 + 17];
        let mut compressed = zstd::stream::encode_all(raw.as_slice(), 1).unwrap();
        compressed.extend_from_slice(&zstd::stream::encode_all(b"next\n".as_slice(), 1).unwrap());
        for chunk in [1, 17, compressed.len()] {
            let mut dec = Decoder::new();
            let mut got = Vec::new();
            let mut out = Vec::new();
            for input in compressed.chunks(chunk) {
                out.clear();
                dec.feed_limited(input, &mut out, 4096).unwrap();
                assert!(out.len() <= 4096);
                got.extend_from_slice(&out);
                while dec.needs_drain() {
                    out.clear();
                    dec.feed_limited(&[], &mut out, 4096).unwrap();
                    assert!(out.len() <= 4096);
                    got.extend_from_slice(&out);
                }
            }
            out.clear();
            dec.finish(&mut out).unwrap();
            got.extend_from_slice(&out);
            let mut expected = raw.clone();
            expected.extend_from_slice(b"next\n");
            assert_eq!(got, expected);
        }
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_rejects_excessive_history_window() {
        // No content-size field, a 128 MiB window, final empty raw block.
        let frame = [0x28, 0xb5, 0x2f, 0xfd, 0, 17 << 3, 1, 0, 0];
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        assert!(dec.feed(&frame, &mut out).unwrap_err().contains("zstd"));
        assert!(out.is_empty());
    }
}
