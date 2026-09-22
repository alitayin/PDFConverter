#!/bin/zsh
set -euo pipefail

# Optional developer network settings. Product runtime never inherits these
# values; this wrapper only affects the command launched below.
export http_proxy="${http_proxy:-http://127.0.0.1:7899}"
export https_proxy="${https_proxy:-http://127.0.0.1:7899}"
export HTTP_PROXY="${HTTP_PROXY:-http://127.0.0.1:7899}"
export HTTPS_PROXY="${HTTPS_PROXY:-http://127.0.0.1:7899}"
export all_proxy="${all_proxy:-socks5h://127.0.0.1:7898}"
export ALL_PROXY="${ALL_PROXY:-socks5h://127.0.0.1:7898}"

if (( $# == 0 )); then
  print -u2 "usage: scripts/env-proxy.sh <command> [args...]"
  exit 64
fi
exec "$@"
