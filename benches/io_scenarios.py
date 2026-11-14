#!/usr/bin/env python3
"""Validated Linux CLI latency/RSS scenarios; requires GNU /usr/bin/time."""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import statistics
import subprocess
import tempfile
import time


def checked_digest(command):
    digest = hashlib.sha256()
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
        while block := child.stdout.read(1024 * 1024):
            digest.update(block)
        error = child.stderr.read()
        code = child.wait()
    return code, digest.hexdigest(), error.decode(errors="replace")[:300]


def timed(command):
    # Measuring the direct Python child with wait4 can include the parent's
    # pre-exec RSS high-water mark. GNU time forks from its small, exec'd image
    # and reports the actual ztail child's peak instead.
    with tempfile.NamedTemporaryFile(prefix="ztail-rss-") as stats:
        wrapped = ["/usr/bin/time", "-q", "-f", "%M", "-o", stats.name, "--", *command]
        start = time.perf_counter()
        result = subprocess.run(wrapped, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        elapsed = time.perf_counter() - start
        if result.returncode:
            raise RuntimeError(f"{command[0]} exited {result.returncode}")
        return elapsed, int(Path(stats.name).read_text().strip())


def follow_once(binary, path, payload, encoded):
    """Measure a real append through a pipe, with a readiness handshake."""
    path.write_bytes(b"")
    with tempfile.NamedTemporaryFile(prefix="ztail-rss-") as stats:
        command = ["/usr/bin/time", "-q", "-f", "%M", "-o", stats.name, "--",
                   str(binary), "-v", "-f", "-n", "0", "-s", "0.01", str(path)]
        with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                              start_new_session=True) as child:
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout, selectors.EVENT_READ)
                    deadline = time.monotonic() + 30

                    def read_ready():
                        remaining = deadline - time.monotonic()
                        if remaining <= 0 or not selector.select(remaining):
                            raise TimeoutError("follow output timed out")
                        block = os.read(child.stdout.fileno(), 65536)
                        if not block:
                            raise RuntimeError("follow exited before completing output")
                        return block

                    header = b""
                    while not header.endswith(b"\n"):
                        header += read_ready()
                    if not header.startswith(b"==> "):
                        raise RuntimeError("missing follow readiness header")
                    start = time.perf_counter()
                    with path.open("ab") as stream:
                        stream.write(encoded)
                    digest = hashlib.sha256()
                    received = 0
                    while received < len(payload):
                        block = read_ready()
                        digest.update(block)
                        received += len(block)
                    elapsed = time.perf_counter() - start
                    if received != len(payload) or digest.digest() != hashlib.sha256(payload).digest():
                        raise RuntimeError("invalid follow payload")
                    # The wrapper and tailer share this private process group.
                    # SIGINT reaches ztail, not an unrelated application.
                    os.killpg(child.pid, signal.SIGINT)
                    if child.wait(timeout=10) != 130:
                        raise RuntimeError(f"unexpected follow exit {child.returncode}")
                    return elapsed, int(Path(stats.name).read_text().strip())
            finally:
                if child.returncode is None:
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    child.wait()


def scenarios(root):
    line = b"2026-11-14T00:00:00Z INFO worker=12 request=42 complete payload\n"
    marker = b"FINAL MARKER\n"
    for name, count, compressed in [
        ("plain-small", 100, None),
        ("plain-large-tail", 1_000_000, None),
        ("gzip-normal", 200_000, 6),
        ("gzip-stored-over-8MiB", 160_000, 0),
        ("gzip-high-expansion", 1_000_000, 6),
    ]:
        path = root / (name + (".gz" if compressed is not None else ".log"))
        opener = gzip.open(path, "wb", compresslevel=compressed) if compressed is not None else path.open("wb")
        with opener as stream:
            for start in range(0, count, 10_000):
                stream.write(line * min(10_000, count - start))
            stream.write(marker)
        expected = hashlib.sha256(line * 9 + marker).hexdigest()
        yield name, [str(path)], expected

    path = root / "bulk.log"
    body = b"short log record\n" * 250_000
    path.write_bytes(body)
    yield "bulk-stdout", ["-n", "250000", str(path)], hashlib.sha256(body).hexdigest()

    path = root / "long.gz"
    body = b"x" * (4 * 1024 * 1024) + b"\n"
    with gzip.open(path, "wb") as stream:
        stream.write(body)
    # -n 10 retains all four forced segments plus the final newline record.
    yield "long-record", [str(path)], hashlib.sha256(body).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True, type=Path)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--runs", type=int, default=5)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    if not Path("/usr/bin/time").exists():
        parser.error("GNU /usr/bin/time is required for isolated peak RSS measurements")
    binaries = [("candidate", args.bin.resolve())]
    if args.baseline:
        binaries.insert(0, ("baseline", args.baseline.resolve()))
    results = []
    with tempfile.TemporaryDirectory(prefix="ztail-io-") as directory:
        for name, operands, expected in scenarios(Path(directory)):
            for label, binary in binaries:
                command = [str(binary), *operands]
                code, actual, error = checked_digest(command)
                if code or actual != expected:
                    row = dict(scenario=name, binary=label, valid=False, exit_code=code, error=error)
                else:
                    measurements = [timed(command) for _ in range(args.runs)]
                    row = dict(
                        scenario=name,
                        binary=label,
                        valid=True,
                        median_ms=round(statistics.median(t for t, _ in measurements) * 1000, 3),
                        peak_rss_kib=round(max(rss for _, rss in measurements)),
                        runs=args.runs,
                    )
                results.append(row)
                print(json.dumps(row), flush=True)
        for name, payload, compressed in [
            ("follow-tiny-plain", b"x\n" * 100_000, False),
            ("follow-high-expansion-gzip", b"log record\n" * 1_000_000, True),
        ]:
            encoded = gzip.compress(payload) if compressed else payload
            path = Path(directory) / name
            for label, binary in binaries:
                try:
                    measurements = [follow_once(binary, path, payload, encoded) for _ in range(args.runs)]
                    row = dict(
                        scenario=name,
                        binary=label,
                        valid=True,
                        median_ms=round(statistics.median(t for t, _ in measurements) * 1000, 3),
                        peak_rss_kib=round(max(rss for _, rss in measurements)),
                        runs=args.runs,
                    )
                except (OSError, RuntimeError, TimeoutError, subprocess.TimeoutExpired) as error:
                    row = dict(scenario=name, binary=label, valid=False, error=str(error))
                results.append(row)
                print(json.dumps(row), flush=True)
    if any(not row["valid"] for row in results if row["binary"] == "candidate"):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
