#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if [[ -z "${TARGET:-}" || -z "${ARTIFACT:-}" ]]; then
  echo "error: TARGET and ARTIFACT must be set by the release workflow" >&2
  exit 1
fi

mkdir -p dist

src="target/${TARGET}/release/gh-envoy"
if [[ ! -f "$src" ]]; then
  src="${src}.exe"
fi

if [[ ! -f "$src" ]]; then
  echo "error: release binary not found for target ${TARGET}" >&2
  exit 1
fi

cp "$src" "dist/${ARTIFACT}"
