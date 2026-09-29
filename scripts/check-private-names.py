#!/usr/bin/env python3
"""Keep private machine, person, and agent names out of this public repository.

Fails when a tracked file names one of the fleet's machines, a home directory
under /home or /Users, a person/ ID, or an agent/fleet/ ID. Write invented
examples instead: web1.example.com, /home/example, /Users/example,
person/alex, agent/example/worker.

Machine names are listed as SHA-256 digests of the lowercase name, so this
file does not publish the names it keeps out. To add a machine, append the
output of `printf %s NAME | shasum -a 256`.

A line that must stay as it is goes in .private-names-allow: one regular
expression per line, searched in `path:text of the line`. Blank lines and
lines starting with # are ignored.

Run it from anywhere inside the repository: python3 scripts/check-private-names.py
"""

import hashlib
import re
import subprocess
import sys
from pathlib import Path

MACHINE_DIGESTS = {
    "a994ffcab684f9e2136b0681c20d2a5ef5f23a5cb7b64ab8934d15a888cc4ea3",
    "2c619e6895b513481a07dae6c4b5781282ef1eee7a7f16744fafe7dbbd3e5ca2",
    "46b6312339466d3b206325f6f402e1fae56cd65f117c779e3b9259833ffbcdf0",
    "9fe430e5274621be518924ed90d2355f39eeb1d4d98fb00ea0022f3a26ba5595",
    "5881d66b8738b483578c8479c59f09856a5447b47bbd2660cff7dc92e1607a2d",
    "294aa8d75483b8331e3ba6a7f24aea15202747f36de65197e7bc6194880b2558",
}

ALLOW_FILE = ".private-names-allow"

WORD = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
HOME = re.compile(r"/(?:home|Users)/([A-Za-z0-9._-]+)")
PERSON = re.compile(r"(?<![A-Za-z0-9_-])person/([A-Za-z0-9._-]+)")
AGENT = re.compile(r"(?<![A-Za-z0-9_-])agent/fleet/")
INVENTED_PEOPLE = {"alex"}


def invented(name):
    return name.lower().startswith("example")


def digest(word):
    return hashlib.sha256(word.encode()).hexdigest()


def names_a_machine(line):
    for word in WORD.findall(line.lower()):
        for part in {word, *word.split("-")}:
            if digest(part) in MACHINE_DIGESTS:
                return True
    return False


def problems(line):
    if names_a_machine(line):
        yield "names a private machine"
    for match in HOME.finditer(line):
        if not invented(match.group(1)):
            yield f"home path {match.group(0)!r}: use /home/example or /Users/example"
    for match in PERSON.finditer(line):
        name = match.group(1)
        if not invented(name) and name.lower() not in INVENTED_PEOPLE:
            yield f"person ID {match.group(0)!r}: use person/alex"
    if AGENT.search(line):
        yield "agent/fleet/ ID: use agent/example/..."


def main():
    root = Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
    )
    script = Path(__file__).resolve()
    this = script.relative_to(root).as_posix() if script.is_relative_to(root) else None
    allow_path = root / ALLOW_FILE
    allowed = []
    if allow_path.exists():
        for raw in allow_path.read_text().splitlines():
            if raw.strip() and not raw.startswith("#"):
                allowed.append(re.compile(raw))

    files = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=root, check=True, capture_output=True,
    ).stdout.decode().split("\0")

    found = 0
    for path in files:
        if not path or path in (this, ALLOW_FILE):
            continue
        full = root / path
        if not full.is_file():
            continue
        data = full.read_bytes()
        if b"\0" in data:
            continue
        for number, line in enumerate(data.decode("utf-8", "replace").splitlines(), 1):
            located = f"{path}:{line}"
            if any(pattern.search(located) for pattern in allowed):
                continue
            for problem in problems(line):
                print(f"{path}:{number}: {problem}")
                found += 1

    if found:
        print(
            f"\n{found} private name(s) found. Replace each with an invented "
            f"example, or add a pattern to {ALLOW_FILE} if the line must stay.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
