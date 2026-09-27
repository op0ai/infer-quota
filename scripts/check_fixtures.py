#!/usr/bin/env python3
"""Fail CI if fixtures/docs contain live-looking credential material.

Filename `*.redacted.*` is not a pass: values are inspected.
Placeholders such as `<redacted>`, `<redacted len=64>` are allowed.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
JWT = re.compile(r"eyJ[A-Za-z0-9_-]{8,}")
CRED_KEYS = {
    "access_token",
    "refresh_token",
    "accesstoken",
    "refreshtoken",
    "sessiontoken",
    "workoscursorsessiontoken",
    "id_token",
    "idtoken",
    "authfingerprint",
}
PLACEHOLDER = re.compile(
    r"(?i)^(<|redact|sha256|dummy|placeholder|none|null|\s*$)"
)


def fail(msg: str) -> None:
    print(msg, file=sys.stderr)
    raise SystemExit(1)


def walk(obj, path: Path, key: str | None = None) -> None:
    if isinstance(obj, dict):
        for k, v in obj.items():
            walk(v, path, k)
        return
    if isinstance(obj, list):
        for item in obj:
            walk(item, path, key)
        return
    if not isinstance(obj, str):
        return
    if JWT.search(obj):
        fail(f"JWT-like material in {path} key={key!r}")
    if key and key.lower() in CRED_KEYS:
        if PLACEHOLDER.search(obj.strip()) or "<redacted" in obj.lower():
            return
        if len(obj) >= 20:
            fail(f"live-looking credential value in {path} key={key!r}")


def scan_json_text(text: str, path: Path) -> None:
    if path.suffix == ".jsonl":
        for i, line in enumerate(text.splitlines(), 1):
            line = line.strip()
            if not line:
                continue
            try:
                walk(json.loads(line), path, None)
            except json.JSONDecodeError as e:
                fail(f"invalid JSONL {path}:{i}: {e}")
        return
    try:
        walk(json.loads(text), path, None)
    except json.JSONDecodeError as e:
        fail(f"invalid JSON {path}: {e}")


def main() -> None:
    for folder in (ROOT / "fixtures", ROOT / "docs"):
        if not folder.is_dir():
            continue
        for path in folder.rglob("*"):
            if not path.is_file():
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            if path.suffix in {".json", ".jsonl"}:
                scan_json_text(text, path)
            elif JWT.search(text):
                fail(f"JWT-like material in {path}")


if __name__ == "__main__":
    main()
