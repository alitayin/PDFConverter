#!/bin/zsh
set -euo pipefail

repo_root="${0:A:h:h}"
cd "$repo_root"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  print -u2 "Electron arm64 development packaging requires Apple Silicon macOS"
  exit 2
fi
if [[ "${RELEASE_STRICT:-0}" == "1" ]]; then
  print -u2 "Electron production signing, notarization and cross-platform release gates are not yet configured"
  exit 2
fi
node scripts/check-version.mjs

office_sha256=94bb3248df074c225490a8a6d1d9dc87c7d6783dbb7a8e9f0d0c3d94348552af
office_url=https://download.documentfoundation.org/libreoffice/stable/26.2.6/mac/aarch64/LibreOffice_26.2.6_MacOS_aarch64.dmg
stage_dir="$repo_root/electron/bin/macos/arm64"
package_version="$(node -p 'require("./package.json").version')"
preview_name="${MINIMALPDF_PREVIEW_NAME:-$package_version}"
if [[ ! "$package_version" =~ '^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9][A-Za-z0-9.-]*)?$' \
  || ! "$preview_name" =~ '^[A-Za-z0-9][A-Za-z0-9._-]*$' ]]; then
  print -u2 "Invalid package version or preview directory name"
  exit 2
fi
preview_root="$repo_root/dist-electron/previews"
output_dir="$preview_root/$preview_name"
app_path="$output_dir/mac-arm64/Ayst Arc PDF.app"
dmg_path="$output_dir/MinimalPdfConverter-${package_version}-arm64.dmg"
if [[ ! -e "$repo_root/dist-electron" ]]; then
  mkdir "$repo_root/dist-electron"
fi
if [[ ! -d "$repo_root/dist-electron" || -L "$repo_root/dist-electron" \
  || -L "$preview_root" || ( -e "$preview_root" && ! -d "$preview_root" ) ]]; then
  print -u2 "Electron preview parent must be a real directory"
  exit 2
fi
for existing in "$stage_dir" "$output_dir" "$app_path" "$dmg_path"; do
  if [[ -e "$existing" || -L "$existing" ]]; then
    print -u2 "Refusing to replace existing packaging artifact: $existing"
    exit 2
  fi
done
if [[ ! -f out/index.html ]]; then
  print -u2 "Missing Next.js static export; run pnpm build first"
  exit 2
fi
available_kib="$(df -Pk "$repo_root" | awk 'NR == 2 { print $4 }')"
if [[ ! "$available_kib" =~ '^[0-9]+$' || "$available_kib" -lt 5242880 ]]; then
  print -u2 "Electron packaging requires at least 5 GiB free on the repository volume"
  exit 2
fi
if ! rustup target list --installed | grep -Eq '^aarch64-apple-darwin$'; then
  print -u2 "Missing aarch64-apple-darwin Rust target"
  exit 2
fi

temp_dir="$(mktemp -d /tmp/minimalpdf-electron-arm64.XXXXXX)"
stage_tmp=''
stage_created=no
output_created=no
mounted=no
cleanup() {
  local exit_code=$?
  if [[ "$mounted" == yes ]]; then
    hdiutil detach "$temp_dir/mount" >/dev/null || print -u2 "Could not detach verified LibreOffice image"
  fi
  if [[ -n "$stage_tmp" && -d "$stage_tmp" ]]; then
    rm -r -- "$stage_tmp"
  fi
  if [[ "$stage_created" == yes && -d "$stage_dir" ]]; then
    rm -r -- "$stage_dir"
  fi
  if (( exit_code != 0 )) && [[ "$output_created" == yes && -d "$output_dir" && ! -L "$output_dir" ]]; then
    rm -r -- "$output_dir"
  fi
  rm -r -- "$temp_dir"
  return "$exit_code"
}
trap cleanup EXIT

if [[ ! -d "$preview_root" ]]; then
  mkdir "$preview_root"
fi
if [[ -L "$preview_root" || ! -d "$preview_root" ]]; then
  print -u2 "Electron preview parent changed before staging"
  exit 2
fi
mkdir "$output_dir"
output_created=yes

if [[ -n "${MINIMALPDF_PREVIEW_OFFICE_APP:-}" ]]; then
  if [[ -z "${MINIMALPDF_PREVIEW_ENGINE:-}" || "$MINIMALPDF_PREVIEW_OFFICE_APP" != /Applications/LibreOffice.app ]]; then
    print -u2 "Local Office is only permitted in an explicit development preview"
    exit 2
  fi
  source_app="$MINIMALPDF_PREVIEW_OFFICE_APP"
else
  office_dmg="${OFFICE_DMG:-$temp_dir/LibreOffice_26.2.6_MacOS_aarch64.dmg}"
  if [[ -z "${OFFICE_DMG:-}" ]]; then
    print "Downloading official LibreOffice 26.2.6 macOS arm64 image"
    scripts/env-proxy.sh curl --fail --location --silent --show-error \
      --connect-timeout 30 --max-time 600 --max-filesize 330000000 \
      --output "$office_dmg" "$office_url"
  fi
  if [[ ! -f "$office_dmg" || "$(stat -f %z "$office_dmg")" != 297798926 ]]; then
    print -u2 "LibreOffice DMG is missing or has the wrong size"
    exit 2
  fi
  actual_sha256="$(shasum -a 256 "$office_dmg" | awk '{print $1}')"
  if [[ "$actual_sha256" != "$office_sha256" ]]; then
    print -u2 "LibreOffice DMG SHA-256 mismatch"
    exit 2
  fi

  mkdir "$temp_dir/mount"
  hdiutil attach -readonly -nobrowse -noautoopen -mountpoint "$temp_dir/mount" "$office_dmg" >/dev/null
  mounted=yes
  source_app="$temp_dir/mount/LibreOffice.app"
fi
verify_official_office() {
  local candidate="$1"
  local signature
  [[ -d "$candidate" && ! -L "$candidate" ]] || return 1
  [[ -f "$candidate/Contents/Resources/LICENSE" && -f "$candidate/Contents/Resources/NOTICE" ]] || return 1
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$candidate/Contents/Info.plist")" == 26.2.6.3 ]] || return 1
  [[ "$(lipo -archs "$candidate/Contents/MacOS/soffice")" == arm64 ]] || return 1
  signature="$(codesign -dv --verbose=4 "$candidate" 2>&1)" || return 1
  print -r -- "$signature" | grep -Eq '^TeamIdentifier=7P5S3ZLCN7$' || return 1
  codesign --verify --deep --strict "$candidate"
}
verify_background_office() {
  local candidate="$1"
  [[ -d "$candidate" && ! -L "$candidate" ]] || return 1
  [[ -f "$candidate/Contents/Resources/LICENSE" && -f "$candidate/Contents/Resources/NOTICE" ]] || return 1
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$candidate/Contents/Info.plist")" == 26.2.6.3 ]] || return 1
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :LSUIElement' "$candidate/Contents/Info.plist")" == true ]] || return 1
  [[ "$(lipo -archs "$candidate/Contents/MacOS/soffice")" == arm64 ]] || return 1
  codesign --verify --deep --strict "$candidate"
}
print "Verifying pristine LibreOffice image"
verify_official_office "$source_app" || { print -u2 "Official LibreOffice image validation failed"; exit 2; }

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"
export ELECTRON_GET_USE_PROXY="${ELECTRON_GET_USE_PROXY:-true}"
if [[ -n "${HTTPS_PROXY:-${https_proxy:-}}" ]]; then
  export GLOBAL_AGENT_HTTPS_PROXY="${GLOBAL_AGENT_HTTPS_PROXY:-${HTTPS_PROXY:-${https_proxy:-}}}"
fi
if [[ -n "${HTTP_PROXY:-${http_proxy:-}}" ]]; then
  export GLOBAL_AGENT_HTTP_PROXY="${GLOBAL_AGENT_HTTP_PROXY:-${HTTP_PROXY:-${http_proxy:-}}}"
fi
mkdir -p electron/bin/macos electron/.generated
if [[ ! -f electron/.generated/icon.png ]]; then
  sips -s format png -z 1024 1024 public/ayst-arc-mark.png --out electron/.generated/icon.png >/dev/null
fi

engine_binary="$repo_root/rust-engine/target/aarch64-apple-darwin/release/minimal-pdf-converter"
if [[ -n "${MINIMALPDF_PREVIEW_ENGINE:-}" ]]; then
  engine_binary="$repo_root/rust-engine/target/debug/minimal-pdf-converter"
  if [[ "$MINIMALPDF_PREVIEW_ENGINE" != "$engine_binary" || ! -f "$engine_binary" || -L "$engine_binary" ]]; then
    print -u2 "Preview engine must be the repository's built debug executable"
    exit 2
  fi
  print "Packaging the prebuilt debug engine for a development preview"
else
  cargo build --locked --release --target aarch64-apple-darwin --manifest-path rust-engine/Cargo.toml --bin minimal-pdf-converter
fi

stage_tmp="$(mktemp -d "$repo_root/electron/bin/macos/.arm64-stage.XXXXXX")"
mkdir -p "$stage_tmp/bin" "$stage_tmp/office"
ditto "$source_app" "$stage_tmp/office/LibreOffice.app"
ditto "$engine_binary" "$stage_tmp/bin/minimal-pdf-converter"
if [[ "$(lipo -archs "$stage_tmp/bin/minimal-pdf-converter")" != arm64 ]]; then
  print -u2 "Staged Rust bridge is not arm64-only"
  exit 2
fi
print "Verifying staged LibreOffice runtime"
verify_official_office "$stage_tmp/office/LibreOffice.app" || { print -u2 "Staged LibreOffice validation failed"; exit 2; }
office_stage="$stage_tmp/office/LibreOffice.app"
plutil -insert LSUIElement -bool YES "$office_stage/Contents/Info.plist"
codesign --force --deep --sign "${MINIMALPDF_OFFICE_SIGN_IDENTITY:--}" "$office_stage"
verify_background_office "$office_stage" || { print -u2 "Background LibreOffice validation failed"; exit 2; }
if [[ "$mounted" == yes ]]; then
  hdiutil detach "$temp_dir/mount" >/dev/null
  mounted=no
fi
if [[ -e "$stage_dir" || -L "$stage_dir" ]]; then
  print -u2 "Arm64 staging destination appeared during the build: $stage_dir"
  exit 2
fi
mv "$stage_tmp" "$stage_dir"
stage_created=yes
stage_tmp=''

CSC_IDENTITY_AUTO_DISCOVERY=false node_modules/.bin/electron-builder --mac dmg --arm64 \
  --config electron-builder.yml "--config.directories.output=$output_dir" --publish never
if [[ ! -f "$dmg_path" || ! -f "$app_path/Contents/Resources/bin/minimal-pdf-converter" ]]; then
  print -u2 "Electron arm64 build is missing its DMG or packaged Rust bridge"
  exit 2
fi
if [[ "$(lipo -archs "$app_path/Contents/Resources/bin/minimal-pdf-converter")" != arm64 ]]; then
  print -u2 "Packaged Rust bridge is not arm64-only"
  exit 2
fi
print "Verifying packaged LibreOffice runtime"
verify_background_office "$app_path/Contents/Resources/office/LibreOffice.app" || { print -u2 "Packaged LibreOffice validation failed"; exit 2; }
print "Built unsigned arm64 Electron development package: $dmg_path"
