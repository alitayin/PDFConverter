[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)]
  [string]$Destination
)

$ErrorActionPreference = 'Stop'

$version = '155.0.8057.0'
$archiveUrl = 'https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/8057/pdfium-win-x64.tgz'
$archiveSha256 = 'e307d519e42f2e69b1b531f0c2a32dffcdf3891ec0eba60328ba51a57cec01ed'
$dllSha256 = '55e7ebef29a1ec9523d1adb8b260a73e7dfb0f64d3f0285121d20ecd6148ef18'
$licenses = @(
  'pdfium.txt', 'libopenjpeg.txt', 'abseil.txt', 'lcms.txt', 'agg23.txt',
  'libjpeg_turbo.md', 'llvm-libc.txt', 'zlib.txt', 'freetype.txt',
  'libjpeg_turbo.ijg', 'icu.txt', 'simdutf.txt', 'fast_float.txt', 'libpng.txt'
)

$root = [IO.Path]::GetFullPath($Destination)
$temp = Join-Path ([IO.Path]::GetTempPath()) ('minimal-pdf-pdfium-' + [Guid]::NewGuid().ToString('N'))
$archive = Join-Path $temp 'pdfium-win-x64.tgz'
$extracted = Join-Path $temp 'extracted'

try {
  if (Test-Path -LiteralPath $root) {
    if (!(Test-Path -LiteralPath $root -PathType Container) -or
        (Get-ChildItem -LiteralPath $root -Force | Select-Object -First 1)) {
      throw "PDFium destination must be empty to avoid a mixed runtime: $root"
    }
  }

  New-Item -ItemType Directory -Path $temp, $extracted -Force | Out-Null
  Invoke-WebRequest -Uri $archiveUrl -OutFile $archive -UseBasicParsing
  if ((Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant() -ne $archiveSha256) {
    throw 'PDFium archive SHA-256 mismatch'
  }

  # The fixed, hash-verified archive is the only input passed to tar.exe.
  & tar.exe -xzf $archive -C $extracted
  if ($LASTEXITCODE -ne 0) {
    throw "Cannot extract pinned PDFium archive (exit $LASTEXITCODE)"
  }
  $sourceDll = Join-Path $extracted 'bin/pdfium.dll'
  if (!(Test-Path -LiteralPath $sourceDll -PathType Leaf) -or
      (Get-FileHash -Algorithm SHA256 -LiteralPath $sourceDll).Hash.ToLowerInvariant() -ne $dllSha256) {
    throw 'PDFium DLL SHA-256 mismatch'
  }

  $buildVersion = (Get-Content -LiteralPath (Join-Path $extracted 'VERSION') -Raw)
  if ($buildVersion -notmatch '(?m)^MAJOR=155\r?$' -or
      $buildVersion -notmatch '(?m)^BUILD=8057\r?$') {
    throw 'PDFium archive version does not match the approved release'
  }
  $buildArgs = Get-Content -LiteralPath (Join-Path $extracted 'args.gn') -Raw
  if ($buildArgs -notmatch 'target_os = "win"' -or
      $buildArgs -notmatch 'target_cpu = "x64"' -or
      $buildArgs -notmatch 'pdf_enable_v8 = false' -or
      $buildArgs -notmatch 'pdf_enable_xfa = false') {
    throw 'PDFium archive build options do not match the approved Windows x64 binary'
  }
  if (!(Test-Path -LiteralPath (Join-Path $extracted 'LICENSE') -PathType Leaf)) {
    throw 'PDFium archive is missing the root LICENSE'
  }
  foreach ($name in $licenses) {
    if (!(Test-Path -LiteralPath (Join-Path $extracted "licenses/$name") -PathType Leaf)) {
      throw "PDFium archive is missing license $name"
    }
  }

  New-Item -ItemType Directory -Path (Join-Path $root 'licenses') -Force | Out-Null
  Copy-Item -LiteralPath $sourceDll -Destination (Join-Path $root 'pdfium.dll')
  Copy-Item -LiteralPath (Join-Path $extracted 'LICENSE') -Destination (Join-Path $root 'LICENSE')
  foreach ($name in $licenses) {
    Copy-Item -LiteralPath (Join-Path $extracted "licenses/$name") -Destination (Join-Path $root "licenses/$name")
  }
  Set-Content -LiteralPath (Join-Path $root 'RUNTIME_VERSION') -Value $version -NoNewline -Encoding ascii
  Set-Content -LiteralPath (Join-Path $root 'RUNTIME_ARCHIVE_SHA256') -Value $archiveSha256 -NoNewline -Encoding ascii
  Set-Content -LiteralPath (Join-Path $root 'RUNTIME_DLL_SHA256_SOURCE') -Value $dllSha256 -NoNewline -Encoding ascii
  Write-Output "Prepared pinned PDFium $version with 15 license files at $root"
}
finally {
  if (Test-Path -LiteralPath $temp) {
    Remove-Item -LiteralPath $temp -Recurse -Force
  }
}
