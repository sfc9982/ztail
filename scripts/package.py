#!/usr/bin/env python3
"""Build release archives from an already-built, locally runnable Linux binary.

Python >=3.11; stdlib only. Does not publish, sign, or overwrite artifacts.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
SHELL_FILES = {"bash": "ztail.bash", "zsh": "_ztail", "fish": "ztail.fish",
               "elvish": "ztail.elv", "powershell": "ztail.ps1"}
TARGETS = ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl")


def run(binary: Path, *args: str) -> bytes:
    env = dict(os.environ, LC_ALL="C", NO_COLOR="1", CLICOLOR_FORCE="0")
    return subprocess.run([str(binary), *args], check=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, env=env, timeout=30).stdout


def package(binary: Path, target: str, flavor: str, output: Path,
            tag: str | None = None, allow_unlicensed: bool = False) -> Path:
    metadata = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    version = metadata["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("unsupported package version")
    if target not in TARGETS or flavor not in ("minimal", "zstd"):
        raise ValueError("unsupported target/flavor")
    if tag is not None and tag != f"v{version}":
        raise ValueError(f"tag must match Cargo.toml: v{version}")
    # A source repository without a license must not accidentally become a release.
    licenses = sorted(p for p in ROOT.glob("LICENSE*") if p.is_file())
    licensed = bool(metadata.get("license") and licenses)
    if not licensed:
        if not allow_unlicensed:
            raise ValueError("release blocked: choose a license, set package.license and add LICENSE first")
    binary = binary.resolve(strict=True)
    if run(binary, "--version").decode().strip() != f"ztail {version}":
        raise ValueError("binary version does not match Cargo.toml")
    info = json.loads(run(binary, "--build-info"))
    expected_features = ["gzip", "plain"] + (["zstd"] if flavor == "zstd" else [])
    if not isinstance(info, dict) or info.get("schema_version") != 1:
        raise ValueError("unsupported binary build-info schema")
    if info.get("name") != "ztail" or info.get("version") != version:
        raise ValueError("binary build-info version/name mismatch")
    if info.get("target") != target:
        raise ValueError("binary target does not match requested archive target")
    features = info.get("features")
    if not isinstance(features, list) or not all(isinstance(f, str) for f in features) or sorted(features) != sorted(expected_features):
        raise ValueError("binary features do not match requested archive flavor")
    name = f"ztail-{version}-{target}-{flavor}"
    entries = {"ztail": binary.read_bytes(), "README.md": (ROOT / "README.md").read_bytes(),
               "CHANGELOG.md": (ROOT / "CHANGELOG.md").read_bytes()}
    entries.update({p.name: p.read_bytes() for p in licenses})
    for document in ["CONTRIBUTING.md", "SECURITY.md"]:
        if (ROOT / document).is_file():
            entries[document] = (ROOT / document).read_bytes()
    entries["man/ztail.1"] = run(binary, "--language=en", "--color=never", "--manpage")
    entries["BUILD-INFO.json"] = (json.dumps(info, sort_keys=True, indent=2) + "\n").encode()

    if not licensed:
        entries["LICENSE-UNSET.txt"] = b"DEVELOPMENT ARTIFACT ONLY. No distribution license has been selected.\n"
    for doc in sorted((ROOT / "docs").glob("*.md")):
        entries[f"docs/{doc.name}"] = doc.read_bytes()
    for shell, filename in SHELL_FILES.items():
        entries[f"completions/{filename}"] = run(binary, "--language=en", "--color=never", "--completions", shell)
    entries["BUILD-INFO.txt"] = f"version={version}\ntarget={target}\nflavor={flavor}\n".encode()
    # Normalize tar metadata and gzip timestamps for reproducible packaging of
    # identical input files. No host paths, owners, or build times are recorded.
    archive = io.BytesIO()
    with gzip.GzipFile(fileobj=archive, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
            for relative, data in sorted(entries.items()):
                info = tarfile.TarInfo(f"{name}/{relative}")
                info.size = len(data)
                info.mode = 0o755 if relative == "ztail" else 0o644
                info.mtime = 0
                tar.addfile(info, io.BytesIO(data))
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"{name}.tar.gz"
    checksum = destination.with_name(destination.name + ".sha256")
    if destination.exists() or checksum.exists():
        raise FileExistsError("release archive or checksum already exists")
    data = archive.getvalue()
    # Exclusive creation protects prior release artifacts from accidental overwrite.
    with destination.open("xb") as file:
        file.write(data)
    with checksum.open("x", encoding="utf-8", newline="\n") as file:
        file.write(f"{hashlib.sha256(data).hexdigest()}  {destination.name}\n")
    return destination


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--flavor", required=True, choices=["minimal", "zstd"])
    parser.add_argument("--output", type=Path, default=ROOT / "target" / "dist")
    parser.add_argument("--tag")
    parser.add_argument("--allow-unlicensed", action="store_true", help="local development verification ONLY")
    args = parser.parse_args()
    try:
        print(package(args.binary, args.target, args.flavor, args.output, args.tag, args.allow_unlicensed))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"package: {error}\n")


if __name__ == "__main__":
    main()
