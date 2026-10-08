#!/usr/bin/env python3
"""Build the exact MoonDesk Worker Companion ZIP used for releases/store upload."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

RUNTIME_FILES = [
    "background.js",
    "chatgpt-dom.js",
    "content.js",
    "manifest.json",
    "model-state-main.js",
    "popup.css",
    "popup.html",
    "popup.js",
    "provider-correlation-main.js",
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--source",
        type=Path,
        default=Path("extensions/moondesk-worker-companion"),
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expected-version")
    return parser.parse_args()


def validate_manifest(source: Path, expected_version: str | None) -> dict[str, object]:
    manifest_path = source / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("manifest_version") != 3:
        raise SystemExit("Worker Companion must remain a Manifest V3 extension")
    version = manifest.get("version")
    if not isinstance(version, str) or not version:
        raise SystemExit("Worker Companion manifest version is missing")
    if expected_version is not None and version != expected_version:
        raise SystemExit(
            f"Worker Companion version {version!r} does not match release {expected_version!r}"
        )
    return manifest


def package(source: Path, output: Path, expected_version: str | None) -> None:
    validate_manifest(source, expected_version)
    for name in RUNTIME_FILES:
        path = source / name
        if not path.is_file():
            raise SystemExit(f"missing Worker Companion runtime file: {path}")

    output.parent.mkdir(parents=True, exist_ok=True)
    with ZipFile(output, "w", compression=ZIP_DEFLATED, compresslevel=9) as archive:
        for name in RUNTIME_FILES:
            # Fixed metadata makes the archive reproducible from identical source bytes instead of
            # inheriting checkout timestamps or platform-specific file modes.
            info = ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            archive.writestr(info, (source / name).read_bytes())

    with ZipFile(output, "r") as archive:
        names = archive.namelist()
        if names != RUNTIME_FILES:
            raise SystemExit(f"unexpected Worker Companion archive contents: {names!r}")
        manifest = json.loads(archive.read("manifest.json"))
        if manifest.get("manifest_version") != 3:
            raise SystemExit("packaged Worker Companion manifest is not Manifest V3")
        if expected_version is not None and manifest.get("version") != expected_version:
            raise SystemExit("packaged Worker Companion version changed during packaging")


def main() -> None:
    args = parse_args()
    package(args.source, args.output, args.expected_version)


if __name__ == "__main__":
    main()
