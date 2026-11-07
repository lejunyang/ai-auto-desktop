param(
    [Parameter(Mandatory = $true)][string]$Aad,
    [Parameter(Mandatory = $true)][string]$InputHelper,
    [string]$Code = '/usr/share/code/code',
    [string]$Python = '/usr/bin/python3',
    [string]$Node = 'node',
    [string]$OutputDirectory = ''
)

# Run alone on an authorized X11 desktop. Uses a new Code profile and owned Qt
# fixture. Python dbus and Node built-in WebSocket provide read-only checks.
$ErrorActionPreference = 'Stop'
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-canonical-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$OutputDirectory = (Resolve-Path $OutputDirectory).Path
$fixture = Join-Path $OutputDirectory 'qt-canonical-fixture'
$flags = ((& pkg-config --cflags --libs Qt5Widgets) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Qt5Widgets development packages unavailable' }
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror (Join-Path $PSScriptRoot 'qt_atspi_fixture.cpp') -o $fixture @flags
if ($LASTEXITCODE -ne 0) { throw 'Qt fixture build failed' }
& $Python (Join-Path $PSScriptRoot 'canonical_invoke.py') `
    --aad (Resolve-Path $Aad).Path --helper (Resolve-Path $InputHelper).Path `
    --code (Resolve-Path $Code).Path --node $Node --qt-fixture $fixture --output $OutputDirectory
if ($LASTEXITCODE -ne 0) { throw 'Canonical invoke failed; inspect summary and native records' }
