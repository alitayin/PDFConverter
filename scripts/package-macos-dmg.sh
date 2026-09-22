#!/bin/zsh
set -euo pipefail

repo_root="${0:A:h}/.."
cd "$repo_root"

if [[ "$(uname -s)" != "Darwin" || $# -gt 1 || ( $# -eq 1 && "$1" != "--replace-existing" ) ]]; then
  print -u2 "usage (macOS): scripts/package-macos-dmg.sh [--replace-existing]"
  exit 64
fi

version="$(tr -d '[:space:]' < "${repo_root}/VERSION")"
bundle_dir="${repo_root}/src-tauri/target/universal-apple-darwin/release/bundle"
app="${bundle_dir}/macos/Ayst Arc PDF.app"
dmg_dir="${bundle_dir}/dmg"
dmg="${dmg_dir}/Ayst Arc PDF_${version}_universal.dmg"

if [[ ! -d "$app" || -L "$app" || -L "$dmg_dir" || -L "$dmg" || ( -e "$dmg" && ! -f "$dmg" ) ]]; then
  print -u2 "missing app or invalid DMG destination"
  exit 2
fi
if [[ -e "$dmg" && "${1:-}" != "--replace-existing" ]]; then
  print -u2 "existing DMG preserved; use --replace-existing to retain a backup and rebuild"
  exit 2
fi
codesign --verify --deep --strict --verbose=2 "$app"
node "${repo_root}/scripts/verify-release-artifact.mjs" "$app"
for binary in "${app}/Contents/MacOS/minimal-pdf-converter" "${app}/Contents/MacOS/pdf_to_txt_worker"; do
  architectures="$(lipo -archs "$binary")"
  if [[ " $architectures " != *" arm64 "* || " $architectures " != *" x86_64 "* ]]; then
    print -u2 "Universal DMG requires arm64 and x86_64: $binary"
    exit 2
  fi
done

identity="${APPLE_SIGNING_IDENTITY:--}"
if [[ "${RELEASE_STRICT:-0}" == "1" ]]; then
  if [[ "$identity" == "-" ]]; then
    print -u2 "strict DMG packaging requires Developer ID signing"
    exit 2
  fi
  xcrun stapler validate "$app"
elif [[ "$identity" != "-" ]]; then
  print -u2 "Developer ID packaging requires RELEASE_STRICT=1"
  exit 2
fi

mkdir -p "$dmg_dir"
temporary_dir="$(mktemp -d "${bundle_dir}/.package-dmg.XXXXXXXX")"
cleanup() {
  if [[ -n "${temporary_dir:-}" && -d "$temporary_dir" ]]; then
    command rm -r -- "$temporary_dir"
  fi
}
trap cleanup EXIT
staging="${temporary_dir}/volume"
mkdir "$staging"
ditto --rsrc --extattr "$app" "${staging}/Ayst Arc PDF.app"
ln -s /Applications "${staging}/Applications"
codesign --verify --deep --strict --verbose=2 "${staging}/Ayst Arc PDF.app"

temporary_dmg="${temporary_dir}/Ayst Arc PDF_${version}_universal.dmg"
hdiutil create -quiet -format UDZO -srcfolder "$staging" -volname 'Ayst Arc PDF' "$temporary_dmg"
hdiutil verify -quiet "$temporary_dmg"

if [[ "$identity" != "-" ]]; then
  codesign --force --sign "$identity" --timestamp "$temporary_dmg"
  codesign --verify --strict --verbose=2 "$temporary_dmg"
fi

# Validate the mounted, copied app before replacing an older image. The DMG
# has not been notarized yet, so this check intentionally runs in local mode.
RELEASE_STRICT=0 node "${repo_root}/scripts/macos-dmg-smoke.mjs" "$temporary_dmg" --no-launch

if [[ -e "$dmg" ]]; then
  backup_dir="${bundle_dir}/dmg-backups"
  if [[ -L "$backup_dir" ]]; then
    print -u2 "refusing symlinked DMG backup directory: $backup_dir"
    exit 2
  fi
  mkdir -p "$backup_dir"
  backup="${backup_dir}/Ayst Arc PDF_${version}_universal.$(date -u +%Y%m%dT%H%M%SZ).$$.dmg"
  mv "$dmg" "$backup"
  if ! mv "$temporary_dmg" "$dmg"; then
    mv "$backup" "$dmg"
    print -u2 "could not install DMG; previous image restored"
    exit 1
  fi
  print "previous DMG retained: $backup"
else
  mv "$temporary_dmg" "$dmg"
fi
print "verified signed-app DMG: $dmg"
