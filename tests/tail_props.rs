//! Property tests for the public `TailFile` API: arbitrary append boundaries
//! must not lose, duplicate, or reorder complete lines.

use std::io::Write;
use std::path::Path;

use proptest::prelude::*;
use ztail::TailFile;

fn append(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn appended_batches_match_written_lines(
        lines in prop::collection::vec("[a-z0-9]{0,16}", 0..20),
        batches in prop::collection::vec(0usize..=8, 1..=8),
        trailing_partial in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("follow.log");
        std::fs::write(&path, b"").unwrap();

        let mut tail = TailFile::open(&path, false).unwrap();
        let mut got = tail.read_initial(10_000, false).unwrap();

        let mut body = Vec::new();
        for line in &lines {
            body.extend_from_slice(line.as_bytes());
            body.push(b'\n');
        }
        let has_partial = trailing_partial && !lines.is_empty();
        if has_partial {
            body.pop();
        }

        let mut offset = 0;
        let mut batch = 0;
        while offset < body.len() {
            let size = batches[batch % batches.len()].max(1);
            let end = (offset + size).min(body.len());
            append(&path, &body[offset..end]);
            got.extend(tail.poll().unwrap().lines);
            offset = end;
            batch += 1;
        }

        let complete = if has_partial {
            &lines[..lines.len() - 1]
        } else {
            &lines[..]
        };
        let expected: Vec<Vec<u8>> = complete
            .iter()
            .map(|line| format!("{line}\n").into_bytes())
            .collect();
        prop_assert_eq!(got, expected);

        if has_partial {
            append(&path, b"\n");
            let update = tail.poll().unwrap();
            let expected = vec![format!("{}\n", lines.last().unwrap()).into_bytes()];
            prop_assert_eq!(update.lines, expected);
        }
    }
}
