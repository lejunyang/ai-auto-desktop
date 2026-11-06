param([string]$BuildDirectory = '')

# Keys, buttons and cancellation run on a private X server, never the user's desktop.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-modifiers-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$binary = Join-Path $BuildDirectory 'x11-modifier-protocol-test'
$flags = ((& pkg-config --cflags --libs x11 xtst) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'X11/XTest development packages unavailable' }
& g++ -std=c++17 -O2 -Wall -Wextra -Werror `
    (Join-Path $root 'tests/linux/x11_modifier_protocol_test.cpp') -o $binary @flags
if ($LASTEXITCODE -ne 0) { throw 'modifier protocol test build failed' }
& xvfb-run -a --server-args='-screen 0 640x480x24 -nolisten tcp' $binary
if ($LASTEXITCODE -ne 0) { throw 'modifier protocol test failed' }
