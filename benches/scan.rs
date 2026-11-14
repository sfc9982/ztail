//! Hot-path timings for the initial scan and one follow poll after a short append.
//!
//! Dev-only (`criterion`). The musl release build does not compile benches.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use flate2::write::GzEncoder;
use flate2::Compression;
use ztail::TailFile;

fn write_plain(path: &Path, lines: usize) {
    let mut f = std::fs::File::create(path).unwrap();
    for i in 0..lines {
        writeln!(f, "line {i} payload").unwrap();
    }
}

fn write_gzip(path: &Path, lines: usize) {
    let mut enc = GzEncoder::new(std::fs::File::create(path).unwrap(), Compression::fast());
    for i in 0..lines {
        writeln!(enc, "line {i} payload").unwrap();
    }
    enc.finish().unwrap();
}

fn scan(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.txt");
    let gz = dir.path().join("plain.txt.gz");
    // A few megabytes of text, and the gzip of the same shape.
    write_plain(&plain, 200_000);
    write_gzip(&gz, 200_000);

    c.bench_function("read_initial plain", |b| {
        b.iter(|| {
            let mut tail = TailFile::open(&plain, false).unwrap();
            tail.read_initial(10, true).unwrap();
        });
    });
    c.bench_function("read_initial gzip", |b| {
        b.iter(|| {
            let mut tail = TailFile::open(&gz, false).unwrap();
            tail.read_initial(10, true).unwrap();
        });
    });

    c.bench_function("poll after tiny append", |b| {
        b.iter_batched(
            || {
                let path = dir.path().join("follow.txt");
                write_plain(&path, 1_000);
                let mut tail = TailFile::open(&path, false).unwrap();
                tail.read_initial(10, false).unwrap();
                let mut f = OpenOptions::new().append(true).open(&path).unwrap();
                writeln!(f, "appended").unwrap();
                tail
            },
            |mut tail| tail.poll().unwrap(),
            criterion::BatchSize::SmallInput,
        );
    });
}

fn extremes(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("large.log");
    write_plain(&plain, 2_000_000);
    c.bench_function("large plain last 10 (reverse seek)", |b| {
        b.iter(|| {
            let mut tail = TailFile::open(&plain, false).unwrap();
            tail.read_initial(10, true).unwrap()
        });
    });

    let raw = b"high ratio record\n".repeat(1_000_000);
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(&raw).unwrap();
    let compressed = enc.finish().unwrap();
    let path = dir.path().join("ratio.gz");
    std::fs::write(&path, &compressed).unwrap();
    c.bench_function("high ratio gzip initial bounded output", |b| {
        b.iter(|| {
            let mut tail = TailFile::open(&path, false).unwrap();
            tail.read_initial(10, true).unwrap()
        });
    });

    c.bench_function("high ratio gzip follow drain all budgets", |b| {
        b.iter_batched(
            || {
                let follow = dir.path().join("append.gz");
                std::fs::write(&follow, []).unwrap();
                let mut tail = TailFile::open(&follow, false).unwrap();
                tail.read_initial(0, false).unwrap();
                std::fs::write(&follow, &compressed).unwrap();
                tail
            },
            |mut tail| {
                let mut bytes = 0;
                loop {
                    let update = tail.poll().unwrap();
                    assert!(update.error.is_none());
                    bytes += update.lines.iter().map(Vec::len).sum::<usize>();
                    if !tail.has_pending_work() {
                        break;
                    }
                }
                assert_eq!(bytes, raw.len());
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, scan, extremes);
criterion_main!(benches);
