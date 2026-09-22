#!/bin/zsh
set -euo pipefail

repo_root="${0:A:h}/.."
cd "$repo_root"

export CARGO_NET_OFFLINE="${CARGO_NET_OFFLINE:-true}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"

node scripts/check-version.mjs
node_modules/.bin/tsc --noEmit
node_modules/.bin/next build
cargo fmt --manifest-path rust-engine/Cargo.toml --all -- --check
cargo test --locked --manifest-path rust-engine/Cargo.toml --all-targets
cargo clippy --locked --manifest-path rust-engine/Cargo.toml --all-targets -- -D warnings
cargo build --locked --manifest-path rust-engine/Cargo.toml --bin pdf_to_txt_worker
node scripts/quality-smoke.mjs
node --test scripts/conversion.test.mjs
node --test scripts/job-progress.test.mjs
node --test scripts/real-quality-check.test.mjs
node --test scripts/check-windows-pdfium.test.mjs
node --test scripts/windows-pdfium-smoke.test.mjs
node --test scripts/electron-architecture.test.mjs
node scripts/generate-release-manifest.mjs --source-only

if [[ -n "${RELEASE_ARTIFACT:-}" ]]; then
  node scripts/generate-release-manifest.mjs --artifact "$RELEASE_ARTIFACT"
fi

echo "source/build gates passed; commercial release still requires real-document and clean-install evidence"
