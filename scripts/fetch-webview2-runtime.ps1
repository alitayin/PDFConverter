[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)]
  [string]$Destination
)

$ErrorActionPreference = 'Stop'

# Pin the official Microsoft fixed runtime CAB linked from the WebView2
# download page. Update the URL, version and both digests together.
$version = '153.0.4234.48'
$packageName = "Microsoft.WebView2.FixedVersionRuntime.$version.x64.cab"
$url = "https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/08cd33ee-d109-49b8-9301-9f0bea43c575/$packageName"
$expectedSha256 = '11e8240cb0bc56dcd3e4498907203c251346f65107fe35a3a13e152c7d51c79e'
$expectedExecutableSha256 = '65afdc3965a6d1c4ccd5b47801fec8a16db15613d35c8e3a6d1fb6c0da970eea'

$root = [IO.Path]::GetFullPath($Destination)
$temp = Join-Path ([IO.Path]::GetTempPath()) ("minimal-pdf-webview2-" + [Guid]::NewGuid().ToString('N'))
$package = Join-Path $temp $packageName
$extract = Join-Path $temp 'extract'

try {
  New-Item -ItemType Directory -Path $temp -Force | Out-Null
  Invoke-WebRequest -Uri $url -OutFile $package -UseBasicParsing
  $actualSha256 = (Get-FileHash -Algorithm SHA256 -Path $package).Hash.ToLowerInvariant()
  if ($actualSha256 -ne $expectedSha256) {
    throw "WebView2 package SHA-256 mismatch: expected $expectedSha256, got $actualSha256"
  }

  New-Item -ItemType Directory -Path $extract -Force | Out-Null
  & expand.exe $package '-f:*' $extract | Out-Null
  if ($LASTEXITCODE -ne 0) {
    throw "Cannot extract official WebView2 CAB (exit $LASTEXITCODE)"
  }
  $source = Join-Path $extract "Microsoft.WebView2.FixedVersionRuntime.$version.x64"
  $executable = Join-Path $source 'msedgewebview2.exe'
  if (!(Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "WebView2 fixed runtime is missing $executable"
  }
  $actualExecutableSha256 = (Get-FileHash -Algorithm SHA256 -Path $executable).Hash.ToLowerInvariant()
  if ($actualExecutableSha256 -ne $expectedExecutableSha256) {
    throw "WebView2 executable SHA-256 mismatch: expected $expectedExecutableSha256, got $actualExecutableSha256"
  }

  if (Test-Path -LiteralPath $root) {
    if (!(Test-Path -LiteralPath $root -PathType Container) -or
        (Get-ChildItem -LiteralPath $root -Force | Select-Object -First 1)) {
      throw "WebView2 destination must be empty to prevent a mixed runtime: $root"
    }
  }
  New-Item -ItemType Directory -Path $root -Force | Out-Null
  Copy-Item -Path (Join-Path $source '*') -Destination $root -Recurse -Force
  Set-Content -Path (Join-Path $root 'RUNTIME_VERSION') -Value $version -NoNewline -Encoding ascii
  Set-Content -Path (Join-Path $root 'RUNTIME_PACKAGE_SHA256') -Value $expectedSha256 -NoNewline -Encoding ascii
  Set-Content -Path (Join-Path $root 'RUNTIME_EXECUTABLE_SHA256') -Value $expectedExecutableSha256 -NoNewline -Encoding ascii
  Write-Output "Prepared WebView2 fixed runtime $version at $root"
}
finally {
  if (Test-Path -LiteralPath $temp) {
    Remove-Item -LiteralPath $temp -Recurse -Force
  }
}
