param([string]$BuildDirectory = '')

# Compile and exercise the clipboard protocol on its own X server. No live
# desktop input or clipboard is accessed. Syntax remains compatible with PS 5.1.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-paste-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$binary = Join-Path $BuildDirectory 'x11-paste-protocol-test'
$flags = ((& pkg-config --cflags --libs x11 xtst) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'X11/XTest development packages unavailable' }
& g++ -std=c++17 -O2 -Wall -Wextra -Werror `
    (Join-Path $root 'tests/linux/x11_paste_protocol_test.cpp') -o $binary @flags
if ($LASTEXITCODE -ne 0) { throw 'paste protocol test build failed' }
& xvfb-run -a --server-args='-screen 0 640x480x24 -nolisten tcp' $binary
if ($LASTEXITCODE -ne 0) { throw 'paste protocol test failed' }
