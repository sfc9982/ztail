//! Line splitting that holds a trailing incomplete line.
//!
//! A follow-mode reader must not print a partial last line and then print it
//! again when the rest arrives. Bytes are emitted only once a `\n` is seen.
//! The newline byte is kept on the line so the output is a faithful copy.
//!
//! Incomplete lines are capped at [`MAX_PARTIAL_LINE`]. A longer record is
//! force-emitted in chunks so a newline-free payload cannot grow without bound.

/// Hard cap on a buffered incomplete line (1 MiB).
pub const MAX_PARTIAL_LINE: usize = 1024 * 1024;

/// Splits an unstructured byte stream into complete lines.
#[derive(Debug, Default)]
pub struct LineBuffer {
    partial: Vec<u8>,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self {
            partial: Vec::new(),
        }
    }

    /// Append decompressed (or plain) bytes.
    ///
    /// Returns every line that became complete, including its trailing `\n`.
    /// A trailing fragment with no newline stays buffered, unless it exceeds
    /// [`MAX_PARTIAL_LINE`], in which case it is force-emitted without a
    /// newline so memory stays bounded.
    #[cfg(test)]
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        self.push_with(data, |line| {
            lines.push(line);
            None
        });
        lines
    }

    /// Like [`push`](Self::push), but `on_line` may return a buffer to reuse
    /// for the next line. The initial last-N scan returns the evicted ring
    /// entry so a long file allocates about N line buffers.
    pub fn push_with(&mut self, data: &[u8], mut on_line: impl FnMut(Vec<u8>) -> Option<Vec<u8>>) {
        let mut spare: Option<Vec<u8>> = None;
        let mut start = 0;
        for (i, &byte) in data.iter().enumerate() {
            if byte == b'\n' {
                self.partial.extend_from_slice(&data[start..=i]);
                let line = take_line(&mut self.partial, &mut spare);
                spare = on_line(line);
                start = i + 1;
            }
        }
        if start < data.len() {
            self.partial.extend_from_slice(&data[start..]);
            while self.partial.len() > MAX_PARTIAL_LINE {
                let rest = self.partial.split_off(MAX_PARTIAL_LINE);
                let line = if let Some(mut slot) = spare.take() {
                    slot.clear();
                    slot.extend_from_slice(&self.partial);
                    self.partial = rest;
                    slot
                } else {
                    std::mem::replace(&mut self.partial, rest)
                };
                spare = on_line(line);
            }
        }
    }

    pub fn clear(&mut self) {
        self.partial.clear();
    }

    #[cfg(test)]
    pub fn partial(&self) -> &[u8] {
        &self.partial
    }
}

/// Remember `line` in a ring that stores at most `n` lines.
///
/// `n == 0` drops every line. This is the initial view: one forward pass, no
/// second read, and no "print everything when the file is short" special case.
/// Returns the evicted buffer so the caller can reuse its allocation.
pub fn retain_last(
    ring: &mut std::collections::VecDeque<Vec<u8>>,
    n: usize,
    line: Vec<u8>,
) -> Option<Vec<u8>> {
    if n == 0 {
        return Some(line);
    }
    if ring.len() == n {
        let old = ring.pop_front();
        ring.push_back(line);
        return old;
    }
    ring.push_back(line);
    None
}

fn take_line(partial: &mut Vec<u8>, spare: &mut Option<Vec<u8>>) -> Vec<u8> {
    if let Some(mut slot) = spare.take() {
        slot.clear();
        slot.extend_from_slice(partial);
        partial.clear();
        slot
    } else {
        std::mem::take(partial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn partial_line_completes_on_newline() {
        let mut buf = LineBuffer::new();
        assert!(buf.push(b"hel").is_empty());
        assert_eq!(buf.partial(), b"hel");
        assert!(buf.push(b"lo").is_empty());
        let lines = buf.push(b" world\nnext");
        assert_eq!(lines, vec![b"hello world\n".to_vec()]);
        assert_eq!(buf.partial(), b"next");
        let lines = buf.push(b" line\n");
        assert_eq!(lines, vec![b"next line\n".to_vec()]);
        assert!(buf.partial().is_empty());
    }

    #[test]
    fn split_across_many_chunks() {
        let mut buf = LineBuffer::new();
        let mut got = Vec::new();
        for chunk in [b"a\nb".as_slice(), b"\nc", b"d\n", b"\n"] {
            got.extend(buf.push(chunk));
        }
        assert_eq!(
            got,
            vec![
                b"a\n".to_vec(),
                b"b\n".to_vec(),
                b"cd\n".to_vec(),
                b"\n".to_vec()
            ]
        );
    }

    #[test]
    fn ring_keeps_last_n_and_fewer_than_n() {
        let mut ring = VecDeque::new();
        for i in 0..3 {
            retain_last(&mut ring, 10, format!("{i}\n").into_bytes());
        }
        let kept: Vec<_> = ring.iter().cloned().collect();
        assert_eq!(
            kept,
            vec![b"0\n".to_vec(), b"1\n".to_vec(), b"2\n".to_vec()]
        );

        let mut ring = VecDeque::new();
        for i in 0..15 {
            retain_last(&mut ring, 10, format!("{i}\n").into_bytes());
        }
        let kept: Vec<_> = ring
            .iter()
            .map(|l| String::from_utf8(l.clone()).unwrap())
            .collect();
        assert_eq!(kept, (5..15).map(|i| format!("{i}\n")).collect::<Vec<_>>());
    }

    #[test]
    fn ring_of_zero_drops_lines() {
        let mut ring = VecDeque::new();
        retain_last(&mut ring, 0, b"a\n".to_vec());
        assert!(ring.is_empty());
    }

    #[test]
    fn oversized_partial_is_force_emitted() {
        let mut buf = LineBuffer::new();
        let chunk = vec![b'x'; MAX_PARTIAL_LINE + 10];
        let lines = buf.push(&chunk);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), MAX_PARTIAL_LINE);
        assert_eq!(buf.partial().len(), 10);
    }
}
