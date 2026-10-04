#!/usr/bin/env bash
# Side-load libghostty-vt's Zig package dependencies into Zig's global cache.
#
# Zig's HTTP client receives `400 Bad Request` from deps.files.ghostty.org
# (curl fetches the same URLs fine), which breaks `cargo build` at the vendored
# `zig build` step. Zig verifies packages by content hash from build.zig.zon,
# so the transport doesn't matter: this script downloads each dependency with
# curl and imports it with `zig fetch <file>`.
#
# It runs in passes: fetching a package can reveal its own build.zig.zon with
# more dependencies (e.g. vaxis -> zigimg, uucode), which the next pass picks
# up from the Zig cache. git+https dependencies are rewritten to GitHub
# codeload tarballs of the pinned commit.
#
# Usage: scripts/fetch-zig-deps.sh   (then re-run cargo build)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vendor_dir="$repo_root/vendor/libghostty-vt"

command -v zig >/dev/null || { echo "zig not found on PATH" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl not found on PATH" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 not found on PATH" >&2; exit 1; }

# Zig 0.16 prints ZON; older releases printed JSON. Respect an explicit cache.
cache_dir="${ZIG_GLOBAL_CACHE_DIR:-}"
if [ -z "$cache_dir" ]; then
  cache_dir="$(zig env | python3 -c '
import re, sys
match = re.search(r"(?:\.global_cache_dir\s*=|\"global_cache_dir\"\s*:)\s*\"([^\"]+)\"", sys.stdin.read())
if not match:
    sys.exit("cannot read Zig global cache directory")
print(match[1])
')"
fi
echo "zig global cache: $cache_dir"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
done_list="$tmp_dir/done"
touch "$done_list"

collect_dependencies() {
  python3 - "$vendor_dir" "$cache_dir" <<'PYTHON'
import json
from pathlib import Path
import re
import sys

# Read ZON aggregates structurally: comments, field order, and line breaks do
# not determine which url/hash/lazy fields belong to a dependency. Unsupported
# syntax is an error rather than a silently omitted required dependency.
TOKEN = re.compile(r'\s+|//[^\n]*|"(?:[^"\\]|\\.)*"|[A-Za-z_][A-Za-z_0-9]*|[0-9][A-Za-z_0-9.]*|[.@{},=]')

class Zon:
    def __init__(self, text):
        self.tokens = []
        pos = 0
        while pos < len(text):
            match = TOKEN.match(text, pos)
            if not match:
                raise ValueError(f"unsupported ZON syntax at offset {pos}")
            token = match.group()
            pos = match.end()
            if not token.isspace() and not token.startswith("//"):
                self.tokens.append(token)
        self.pos = 0

    def peek(self):
        return self.tokens[self.pos] if self.pos < len(self.tokens) else None

    def take(self, expected=None):
        token = self.peek()
        if token is None or (expected is not None and token != expected):
            raise ValueError(f"expected {expected}, got {token}")
        self.pos += 1
        return token

    def value(self):
        token = self.take()
        if token == ".":
            if self.peek() != "{":
                return self.take()  # enum literal (e.g. package name)
            self.take("{")
            fields = {}
            while self.peek() != "}":
                # Struct field versus tuple member (.paths = .{"..."}).
                if self.peek() == "." and self.tokens[self.pos + 1] != "{":
                    self.take(".")
                    name = self.take()
                    if name == "@":
                        name = json.loads(self.take())
                    self.take("=")
                    if name in fields:
                        raise ValueError(f"duplicate field {name}")
                    fields[name] = self.value()
                else:
                    self.value()
                if self.peek() != "}":
                    self.take(",")
            self.take("}")
            return fields
        if token.startswith('"'):
            return json.loads(token)
        return token

paths = sorted(Path(sys.argv[1]).rglob("build.zig.zon"))
paths += sorted((Path(sys.argv[2]) / "p").glob("*/build.zig.zon"))
if not paths:
    sys.exit("no build.zig.zon files found")
deps = {}
for path in paths:
    try:
        parser = Zon(path.read_text())
        root = parser.value()
        if parser.peek() is not None or not isinstance(root, dict):
            raise ValueError("invalid root aggregate")
        for dep in root.get("dependencies", {}).values():
            if "url" not in dep:
                continue
            url, expected = dep["url"], dep.get("hash", "")
            if not url or not expected or any(c.isspace() for c in url + expected):
                raise ValueError("URL dependency must have a URL and hash without whitespace")
            lazy = dep.get("lazy", "false")
            if lazy not in ("true", "false"):
                raise ValueError("invalid lazy value")
            key = (url, expected)
            # A required reference wins if the same package also appears lazy.
            deps[key] = deps.get(key, True) and lazy == "true"
    except (ValueError, TypeError, AttributeError, IndexError) as error:
        sys.exit(f"{path}: {error}")
for (url, expected), lazy in sorted(deps.items()):
    print(url, expected, "optional" if lazy else "required", sep="\t")
PYTHON
}

fetch_one() {
  local url="$1" expected="$2" computed file
  case "$url" in
    git+https://github.com/*\#*)
      local repo="${url#git+https://github.com/}"
      local commit="${repo#*#}"
      repo="${repo%%#*}"
      repo="${repo%.git}"
      url="https://codeload.github.com/${repo}/tar.gz/${commit}"
      # codeload's basename is the revision, not an archive extension.
      file="$tmp_dir/package.tar.gz"
      ;;
    git+*)
      echo "FAIL unsupported git host: $url" >&2
      return 1
      ;;
    *) file="$tmp_dir/$(basename "${url%%\?*}")" ;;
  esac
  if ! curl -fsSL "$url" -o "$file"; then
    echo "FAIL curl: $url" >&2
    return 1
  fi
  # Zig 0.16 needs a build.zig cwd even when importing a local archive.
  if ! computed="$(cd "$vendor_dir" && zig fetch "$file")"; then
    echo "FAIL zig fetch rejected: $url" >&2
    return 1
  fi
  if [ "$computed" != "$expected" ]; then
    echo "FAIL hash: $url expected=$expected computed=$computed" >&2
    return 1
  fi
  echo "ok $url expected=$expected computed=$computed"
  rm -f "$file"
}

required_ok=0 required_failed=0 optional_ok=0 optional_failed=0
for pass in 1 2 3; do
  # Do not hide parser failure in process substitution: incomplete discovery
  # must fail the prefetch, even if every dependency found so far succeeded.
  collect_dependencies > "$tmp_dir/dependencies"
  new=0
  while IFS=$'\t' read -r url expected kind; do
    entry="$url $expected $kind"
    grep -qxF "$entry" "$done_list" && continue
    echo "$entry" >> "$done_list"
    new=1
    if fetch_one "$url" "$expected"; then
      if [ "$kind" = required ]; then
        required_ok=$((required_ok + 1))
      else
        optional_ok=$((optional_ok + 1))
      fi
    elif [ "$kind" = required ]; then
      required_failed=$((required_failed + 1))
    else
      optional_failed=$((optional_failed + 1))
      echo "optional (lazy) not fetched: $url" >&2
    fi
  done < "$tmp_dir/dependencies"
  [ "$new" -eq 1 ] || break
done

printf 'required ok=%s failed=%s; optional ok=%s failed=%s\n' \
  "$required_ok" "$required_failed" "$optional_ok" "$optional_failed"
[ "$required_failed" -eq 0 ]
