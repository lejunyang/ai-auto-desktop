param([Parameter(Mandatory = $true)][string]$BuildDirectory,
    [ValidateSet('5','6')][string]$QtMajor = '5', [string]$Qmake)

$ErrorActionPreference = 'Stop'
if (!$Qmake) { $Qmake = if ($QtMajor -eq '5') { 'qmake' } else { 'qmake6' } }
$qtVersion = & $Qmake -query QT_VERSION
if ($LASTEXITCODE -or $qtVersion -notmatch "^$QtMajor\.") { throw "Select the target application Qt $QtMajor qmake" }
New-Item -ItemType Directory -Force $BuildDirectory | Out-Null
Push-Location (Resolve-Path -LiteralPath $BuildDirectory)
try {
    & $Qmake (Join-Path $PSScriptRoot 'input.pro') | Out-Host
    if ($LASTEXITCODE) { throw 'Qt Wayland input plugin configuration failed' }
    & make | Out-Host
    if ($LASTEXITCODE) { throw 'Qt Wayland input plugin build failed' }
    (Resolve-Path -LiteralPath 'generic/libaadwaylandinput.so').Path
} finally {
    Pop-Location
}
