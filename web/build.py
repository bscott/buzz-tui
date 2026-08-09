#!/usr/bin/env python3
"""Render the buzz-tui site.

The point of generating rather than hand-writing this page is that the version
and the keybinding table come from the program itself. A hand-maintained site
starts accurate and quietly stops being so; this one cannot, because building it
runs the binary and asks.

    python3 web/build.py --binary target/release/buzztui --out site

Requires nothing outside the standard library.
"""

from __future__ import annotations

import argparse
import html
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB = ROOT / "web"


def crate_version() -> str:
    """The version in Cargo.toml, which release tags are checked against."""
    for line in (ROOT / "Cargo.toml").read_text().splitlines():
        match = re.match(r'^version\s*=\s*"([^"]+)"', line)
        if match:
            return f"v{match.group(1)}"
    raise SystemExit("could not read version from Cargo.toml")


def keymap(binary: Path) -> tuple[str, str]:
    """Runs `buzztui keys` and turns its output into grouped HTML tables.

    A throwaway home keeps the build from reading, or creating, anything in the
    configuration of whoever is running it.
    """
    with tempfile.TemporaryDirectory() as home:
        env = {**os.environ, "BUZZTUI_HOME": home, "NO_COLOR": "1"}
        result = subprocess.run(
            [str(binary), "keys"],
            capture_output=True,
            text=True,
            env=env,
            check=True,
        )

    leader = "ctrl+b"
    groups: list[tuple[str, list[tuple[str, str]]]] = []
    current: list[tuple[str, str]] | None = None

    for line in result.stdout.splitlines():
        if not line.strip():
            continue
        if line.startswith("leader "):
            leader = line.split(None, 1)[1].strip()
            continue
        if not line.startswith("  "):
            # A heading; anything after the table is prose and ends the parse.
            if line.startswith("rebind"):
                break
            current = []
            groups.append((line.strip(), current))
            continue
        if current is None:
            continue
        # `  <keys>  <description>  <action>` with runs of spaces between.
        parts = re.split(r"\s{2,}", line.strip())
        if len(parts) >= 2:
            current.append((parts[0], parts[1]))

    if not groups:
        raise SystemExit("`buzztui keys` produced no bindings")

    rendered = []
    for name, rows in groups:
        body = "\n".join(
            f"        <tr><td class=\"k\">{html.escape(k)}</td>"
            f"<td class=\"d\">{html.escape(d)}</td></tr>"
            for k, d in rows
        )
        rendered.append(
            f'      <div>\n        <h3>{html.escape(name)}</h3>\n'
            f"        <table>\n{body}\n        </table>\n      </div>"
        )
    return leader, "\n".join(rendered)


def hero() -> str:
    """The captured terminal frame, with colour applied by class.

    Kept beside this script rather than inline so that refreshing it means
    replacing a capture, not editing markup.
    """
    raw = (WEB / "hero.txt").read_text().rstrip("\n")
    out = []
    for line in raw.split("\n"):
        escaped = html.escape(line)
        # The rail, the separator, and the box drawing are all structure.
        escaped = re.sub(
            r"([│┌┐└┘─]+)", r'<span class="rail">\1</span>', escaped
        )
        escaped = re.sub(
            r"(#\s?\w[\w-]*)", r'<span class="name">\1</span>', escaped, count=1
        )
        escaped = re.sub(r"(● online)", r'<span class="ok">\1</span>', escaped)
        escaped = re.sub(
            r"(\b\d{2}:\d{2}\b)", r'<span class="dim">\1</span>', escaped
        )
        escaped = re.sub(
            r"(NAVIGATE)", r'<span class="chip"> \1 </span>', escaped
        )
        out.append(escaped)
    return "\n".join(out)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/buzztui")
    parser.add_argument("--out", default="site")
    parser.add_argument("--repo", default="bscott/buzz-tui")
    parser.add_argument(
        "--domain",
        default="",
        help="custom domain; writes a CNAME so Pages serves at the root",
    )
    args = parser.parse_args()

    binary = Path(args.binary)
    if not binary.exists():
        raise SystemExit(f"{binary} not found; build it first")

    leader, keys = keymap(binary)
    version = crate_version()

    page = (WEB / "index.html.in").read_text()
    for token, value in {
        "{{VERSION}}": version,
        "{{REPO}}": args.repo,
        "{{LEADER}}": leader,
        "{{KEYS}}": keys,
        "{{HERO}}": hero(),
    }.items():
        page = page.replace(token, value)

    leftover = re.findall(r"\{\{[A-Z_]+\}\}", page)
    if leftover:
        raise SystemExit(f"unreplaced placeholders: {sorted(set(leftover))}")

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    (out / "index.html").write_text(page)
    shutil.copy(WEB / "style.css", out / "style.css")
    # Pages serves a custom domain only when the deployed tree names it.
    if args.domain:
        (out / "CNAME").write_text(args.domain + "\n")
    # Stops Pages running the output through Jekyll, which would ignore any
    # file beginning with an underscore.
    (out / ".nojekyll").touch()

    print(f"built {out}/index.html for {version} ({len(keys)} bytes of keymap)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
