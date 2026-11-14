#!/usr/bin/env python3
"""Exercise a source install/uninstall entirely under an isolated staging root."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--all-features", action="store_true")
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("run this Linux installation check inside Linux/WSL")
    scratch = ROOT / "target"
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ztail-install-", dir=scratch) as temporary:
        stage = Path(temporary).resolve()
        assert stage.parent == scratch.resolve()
        sentinel = stage / "usr/share/ztail/unrelated.txt"
        sentinel.parent.mkdir(parents=True)
        sentinel.write_text("preserve unrelated files", encoding="utf-8")
        command = ["make", "PREFIX=/usr", f"DESTDIR={stage}", "BINDIR=/usr/bin",
                   "DATADIR=/usr/share", "MANDIR=/usr/share/man"]
        if args.all_features:
            command.append("CARGO_FEATURES=--all-features")
        subprocess.run(command + ["install"], cwd=ROOT, check=True)
        expected = ["usr/bin/ztail", "usr/share/man/man1/ztail.1",
                    "usr/share/bash-completion/completions/ztail",
                    "usr/share/zsh/site-functions/_ztail",
                    "usr/share/fish/vendor_completions.d/ztail.fish",
                    "usr/share/ztail/completions/elvish",
                    "usr/share/ztail/completions/powershell",
                    "usr/share/ztail/BUILD-INFO.json"]
        for relative in expected:
            path = stage / relative
            assert path.is_file() and path.stat().st_size > 0, relative
            assert path.stat().st_mode & 0o777 == (0o755 if relative == "usr/bin/ztail" else 0o644), relative
        actual = {p.relative_to(stage).as_posix() for p in stage.rglob("*") if p.is_file()}
        assert actual == set(expected) | {"usr/share/ztail/unrelated.txt"}, actual
        info = json.loads(subprocess.check_output([str(stage / "usr/bin/ztail"), "--build-info"]))
        assert info["name"] == "ztail"
        assert ("zstd" in info["features"]) == args.all_features
        assert b".TH ZTAIL" in (stage / "usr/share/man/man1/ztail.1").read_bytes()
        subprocess.run(command + ["uninstall"], cwd=ROOT, check=True)
        assert sentinel.read_text(encoding="utf-8") == "preserve unrelated files"
        assert not any((stage / relative).exists() for relative in expected)
    print("Install/uninstall OK: eight managed files; unrelated file preserved")


if __name__ == "__main__":
    main()
