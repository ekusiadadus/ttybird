#!/usr/bin/env python3
"""Render the Homebrew formula from checksums emitted by package_release.py."""

import argparse
import re
from pathlib import Path
from string import Template


VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?")
SHA256 = re.compile(r"[0-9a-f]{64}")
ROOT = Path(__file__).resolve().parents[2]
TEMPLATE = Path(__file__).with_name("ttybird.rb.in")


def read_checksum(path: Path, archive: str) -> str:
    fields = path.read_text().split()
    if len(fields) != 2 or fields[1].lstrip("*") != archive:
        raise SystemExit(f"{path} does not contain a checksum for {archive}")
    if not SHA256.fullmatch(fields[0]):
        raise SystemExit(f"{path} does not contain a lowercase SHA-256 digest")
    return fields[0]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist", type=Path, default=ROOT / "dist")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail instead of replacing an out-of-date output file",
    )
    args = parser.parse_args()

    if not VERSION.fullmatch(args.version):
        raise SystemExit(f"unsupported release version: {args.version!r}")

    names = {
        "macos": f"ttybird-{args.version}-aarch64-apple-darwin.tar.gz",
        "linux": f"ttybird-{args.version}-x86_64-unknown-linux-gnu.tar.gz",
    }
    rendered = Template(TEMPLATE.read_text()).substitute(
        version=args.version,
        macos_sha256=read_checksum(
            args.dist / names["macos"].replace(".tar.gz", ".sha256"), names["macos"]
        ),
        linux_sha256=read_checksum(
            args.dist / names["linux"].replace(".tar.gz", ".sha256"), names["linux"]
        ),
    )

    if args.check:
        if not args.output.exists() or args.output.read_text() != rendered:
            raise SystemExit(f"{args.output} is not the rendered {args.version} formula")
        return

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(rendered)


if __name__ == "__main__":
    main()
