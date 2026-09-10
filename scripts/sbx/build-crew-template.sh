#!/usr/bin/env bash
# Build the herdr-crew sandbox template image and load it into the Docker
# Sandboxes runtime image store. Run on the host (needs Docker Desktop + sbx).
#
# The sandbox runtime does not share the host Docker daemon's image store, so
# a locally built image must be loaded with `sbx template load`. Once loaded,
# the image is cached: new task sandboxes created from the herdr-crew kit
# start fast because all tools are already baked in.
#
# Usage:
#   scripts/sbx/build-crew-template.sh [tag]
#
# Optional environment overrides (default: latest):
#   HERDR_VERSION, CLAUDE_CODE_VERSION, CODEX_VERSION, GEMINI_CLI_VERSION
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
context="$repo_root/kits/herdr-crew/template"
tag="${1:-herdr-crew:local}"

command -v docker >/dev/null || { echo "docker not found on PATH" >&2; exit 1; }
command -v sbx >/dev/null || { echo "sbx not found on PATH" >&2; exit 1; }

build_args=()
for var in HERDR_VERSION CLAUDE_CODE_VERSION CODEX_VERSION GEMINI_CLI_VERSION; do
  if [ -n "${!var:-}" ]; then
    build_args+=(--build-arg "$var=${!var}")
  fi
done

echo "building $tag from $context"
docker build -t "$tag" "${build_args[@]+"${build_args[@]}"}" "$context"

tar_file="$(mktemp -t herdr-crew-template-XXXXXX).tar"
trap 'rm -f "$tar_file"' EXIT
docker image save "$tag" -o "$tar_file"

echo "loading $tag into the sandbox runtime image store"
sbx template load "$tar_file"

cat <<EOF

done. next steps:
  sbx setup ssh                                        # once per host
  sbx kit validate $repo_root/kits/herdr-crew
  sbx create ./kits/herdr-crew/ <task-dir> --name herdr-task-<slug>
  ssh herdr-task-<slug>.sbx herdr status server        # sanity check
  herdr --remote ssh://herdr-task-<slug>.sbx           # attach from host herdr
EOF
