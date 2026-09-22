param(
  [Parameter(Mandatory = $true)]
  [string] $Destination
)

$ErrorActionPreference = 'Stop'

$version = '26.2.6'
$url = "https://download.documentfoundation.org/libreoffice/stable/$version/win/x86_64/LibreOffice_${version}_Win_x86-64.msi"
$destinationRoot = [IO.Path]::GetFullPath($Destination)
$msi = Join-Path $env:RUNNER_TEMP "LibreOffice_${version}_Win_x86-64.msi"
$extract = Join-Path $env:RUNNER_TEMP "libreoffice-extract-$([guid]::NewGuid().ToString('N'))"
$target = Join-Path $destinationRoot 'LibreOffice'

if (Test-Path -LiteralPath $target) {
  throw "Refusing to replace existing LibreOffice staging directory: $target"
}
New-Item -ItemType Directory -Force -Path $destinationRoot, $extract | Out-Null

Write-Host "Downloading official LibreOffice $version Windows MSI"
Invoke-WebRequest -Uri $url -OutFile $msi
if (-not (Test-Path -LiteralPath $msi) -or (Get-Item -LiteralPath $msi).Length -eq 0) {
  throw 'LibreOffice Windows MSI download is missing or empty'
}

if ($env:LIBREOFFICE_WINDOWS_SHA256) {
  $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $msi).Hash.ToLowerInvariant()
  $expected = $env:LIBREOFFICE_WINDOWS_SHA256.Trim().ToLowerInvariant()
  if ($actual -ne $expected) {
    throw "LibreOffice Windows MSI SHA-256 mismatch: $actual"
  }
}

$arguments = @('/a', $msi, '/qn', "TARGETDIR=$extract")
$process = Start-Process -FilePath 'msiexec.exe' -ArgumentList $arguments -Wait -PassThru -WindowStyle Hidden
if ($process.ExitCode -ne 0) {
  throw "LibreOffice administrative extraction failed with exit code $($process.ExitCode)"
}

$soffice = Get-ChildItem -LiteralPath $extract -Filter 'soffice.exe' -Recurse -File | Select-Object -First 1
if (-not $soffice) {
  throw 'Extracted LibreOffice runtime does not contain soffice.exe'
}
$bundle = $soffice.Directory.Parent
New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item -Path (Join-Path $bundle.FullName '*') -Destination $target -Recurse -Force

foreach ($notice in @('LICENSE', 'NOTICE')) {
  $destinationNotice = Join-Path $target $notice
  if (-not (Test-Path -LiteralPath $destinationNotice)) {
    $sourceNotice = Get-ChildItem -LiteralPath $target -Filter $notice -Recurse -File | Select-Object -First 1
    if (-not $sourceNotice) {
      throw "Extracted LibreOffice runtime is missing $notice"
    }
    Copy-Item -LiteralPath $sourceNotice.FullName -Destination $destinationNotice
  }
}

Write-Host "Staged LibreOffice runtime at $target"
