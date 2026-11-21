param([Parameter(Mandatory = $true)][string]$BuildDirectory)

$ErrorActionPreference = 'Stop'
$qtVersion = & qmake -query QT_VERSION
if ($LASTEXITCODE -or $qtVersion -notmatch '^5\.') { throw 'Select the target application Qt 5 qmake on PATH' }
New-Item -ItemType Directory -Force $BuildDirectory | Out-Null
Push-Location (Resolve-Path -LiteralPath $BuildDirectory)
try {
    & qmake (Join-Path $PSScriptRoot 'input.pro') | Out-Host
    if ($LASTEXITCODE) { throw 'Qt Wayland input plugin configuration failed' }
    & make | Out-Host
    if ($LASTEXITCODE) { throw 'Qt Wayland input plugin build failed' }
    (Resolve-Path -LiteralPath 'generic/libaadwaylandinput.so').Path
} finally {
    Pop-Location
}
