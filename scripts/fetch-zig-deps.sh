#!/usr/bin/env bash
# Side-load libghostty-vt's Zig package dependencies into Zig's global cache.
#
# Zig 0.15's HTTP client receives `400 Bad Request` from deps.files.ghostty.org
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

cache_dir="$(zig env 2>/dev/null | tr ',' '\n' | grep -o '"global_cache_dir"[^"]*"[^"]*"' | cut -d'"' -f4 || true)"
[ -n "$cache_dir" ] || cache_dir="$HOME/.cache/zig"
echo "zig global cache: $cache_dir"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
done_list="$tmp_dir/done"
touch "$done_list"

collect_urls() {
  {
    find "$vendor_dir" -name build.zig.zon -exec grep -h '\.url = "' {} + 2>/dev/null
    find "$cache_dir/p" -maxdepth 2 -name build.zig.zon -exec grep -h '\.url = "' {} + 2>/dev/null
  } | grep -o '"[a-z+]*://[^"]*"' | tr -d '"' | sort -u
}

fetch_one() {
  local url="$1"
  case "$url" in
    git+https://github.com/*\#*)
      # git+https://github.com/owner/repo#commit -> codeload tarball
      local repo="${url#git+https://github.com/}"
      local commit="${repo#*#}"
      repo="${repo%%#*}"
      url="https://github.com/${repo}/archive/${commit}.tar.gz"
      ;;
    git+*)
      echo "skip (unsupported git host): $url"
      return 0
      ;;
  esac
  # zig fetch infers the archive format from the file extension, so keep the
  # URL's basename.
  local file="$tmp_dir/$(basename "$url")"
  if curl -fsSL "$url" -o "$file"; then
    if zig fetch "$file" >/dev/null 2>&1; then
      echo "ok   $url"
    else
      echo "FAIL zig fetch rejected: $url" >&2
      return 1
    fi
  else
    echo "FAIL curl: $url" >&2
    return 1
  fi
  rm -f "$file"
}

failures=0
for pass in 1 2 3; do
  new=0
  while IFS= read -r url; do
    grep -qxF "$url" "$done_list" && continue
    echo "$url" >> "$done_list"
    new=1
    fetch_one "$url" || failures=$((failures + 1))
  done < <(collect_urls)
  [ "$new" -eq 1 ] || break
done

if [ "$failures" -gt 0 ]; then
  echo "$failures fetch(es) failed; the build may still work if they were optional (lazy) packages." >&2
fi
echo "done. re-run: cargo build --release"
