#!/usr/bin/env python3
"""Check the native owner, resolved Rust bindings, and optional consumer locks."""
import argparse
import json
from pathlib import Path
import re
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--sys-source", type=Path, required=True)
parser.add_argument("--consumer-lock", type=Path, action="append", default=[])
args = parser.parse_args()
owner = Path(__file__).resolve().parent.parent
contract = json.loads((owner / "libghostty-vt-contract.json").read_text())
version = contract["rustBindingsVersion"]
manifest = tomllib.loads((owner / "Cargo.toml").read_text())
for name in ("libghostty-vt", "libghostty-vt-sys"):
    request = manifest["workspace"]["dependencies"][name]
    if isinstance(request, dict):
        request = request["version"]
    if request != f"={version}":
        raise SystemExit(f"{name}: owner must request exactly ={version}, got {request}")
for lock in [owner / "Cargo.lock", *args.consumer_lock]:
    packages = tomllib.loads(lock.read_text())["package"]
    for name in ("libghostty-vt", "libghostty-vt-sys"):
        versions = [p["version"] for p in packages if p["name"] == name]
        if versions != [version]:
            raise SystemExit(f"{lock}: {name}: expected only {version}, got {versions}")
sys_manifest = tomllib.loads((args.sys_source / "Cargo.toml").read_text())
if sys_manifest["package"]["version"] != version:
    raise SystemExit("sys source version does not match owner contract")
source = (args.sys_source / "build.rs").read_text()
match = re.search(r'const GHOSTTY_COMMIT: &str = "([0-9a-f]{40})";', source)
if match is None or match[1] != contract["ghosttyRev"]:
    raise SystemExit("sys source Ghostty commit does not match owner contract")
print(f"PASS: bindings {version}, Ghostty {contract['ghosttyRev']}, {1 + len(args.consumer_lock)} lockfiles")
