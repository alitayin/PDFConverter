#!/bin/zsh
set -euo pipefail

if [[ $# -ne 1 || ! -d "$1" || "$1" != *.app ]]; then
  print -u2 "usage: scripts/sign-macos-adhoc.sh path/to/Application.app"
  exit 64
fi

app="$1"
# This is only a local smoke-test signature. It is intentionally rejected by
# RELEASE_STRICT=1; commercial builds must use a Developer ID identity.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict --verbose=2 "$app"
print "adhoc app signature verified: $app"
