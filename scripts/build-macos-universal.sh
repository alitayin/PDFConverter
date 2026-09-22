#!/bin/zsh
set -euo pipefail

repo_root="${0:A:h}/.."
cd "$repo_root"

if [[ "$(uname -s)" != "Darwin" ]]; then
  print -u2 "macOS universal builds require macOS and Xcode Command Line Tools"
  exit 2
fi

for target in aarch64-apple-darwin x86_64-apple-darwin; do
  if ! rustup target list --installed | rg -q "^${target}$"; then
    print -u2 "missing ${target} Rust target; install it with the configured proxy first"
    exit 2
  fi
done

if ! xcrun --sdk macosx --show-sdk-path >/dev/null 2>&1; then
  print -u2 "macOS SDK unavailable"
  exit 2
fi

export CARGO_NET_OFFLINE="${CARGO_NET_OFFLINE:-false}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"

if [[ "${RELEASE_STRICT:-0}" == "1" ]]; then
  if [[ -z "${APPLE_SIGNING_IDENTITY:-}" || "${APPLE_SIGNING_IDENTITY}" == "-" ]]; then
    print -u2 "strict macOS builds require a Developer ID Application signing identity"
    exit 2
  fi
  if [[ -z "${APPLE_ID:-}" || -z "${APPLE_PASSWORD:-}" || -z "${APPLE_TEAM_ID:-}" ]]; then
    print -u2 "strict macOS builds require Apple notarization credentials"
    exit 2
  fi
else
  if [[ -n "${APPLE_SIGNING_IDENTITY:-}" && "${APPLE_SIGNING_IDENTITY}" != "-" ]]; then
    print -u2 "Developer ID packaging requires RELEASE_STRICT=1"
    exit 2
  fi
  export APPLE_SIGNING_IDENTITY="-"
  # Adhoc development builds must never attempt to submit to Apple.
  unset APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID
  unset APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH
fi

# Tauri's universal build lipo-merges the main binary itself, but the Cargo
# package also contains the protocol worker in `src/bin/`. The bundler expects
# every non-main binary at the universal target path, so prepare that worker
# explicitly before Tauri starts its two architecture builds.
worker_dir="${repo_root}/src-tauri/target"
cargo build --locked --release --target aarch64-apple-darwin --manifest-path "${repo_root}/src-tauri/Cargo.toml" --bin pdf_to_txt_worker
cargo build --locked --release --target x86_64-apple-darwin --manifest-path "${repo_root}/src-tauri/Cargo.toml" --bin pdf_to_txt_worker
mkdir -p "${worker_dir}/universal-apple-darwin/release"
lipo -create \
  -output "${worker_dir}/universal-apple-darwin/release/pdf_to_txt_worker" \
  "${worker_dir}/aarch64-apple-darwin/release/pdf_to_txt_worker" \
  "${worker_dir}/x86_64-apple-darwin/release/pdf_to_txt_worker"

# Tauri signs the worker and app before app bundling completes. Package the DMG
# from this signed app separately so an existing image is not removed by the
# Tauri DMG bundler and an unsigned copy can never become the installable app.
CI="${CI:-true}" cargo tauri build --target universal-apple-darwin --bundles app
"${repo_root}/scripts/package-macos-dmg.sh" --replace-existing
