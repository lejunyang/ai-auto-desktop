param([string]$BuildDirectory = '')
$ErrorActionPreference = 'Stop'
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-window-title-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Force -Path $BuildDirectory | Out-Null
$binary = Join-Path (Resolve-Path $BuildDirectory).Path 'x11-window-title-test'
$flags = ((& pkg-config --cflags --libs x11 xtst) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'X11 development packages unavailable' }
& g++ -std=c++17 -O2 -Wall -Wextra -Werror (Join-Path $PSScriptRoot 'x11_window_title_test.cpp') -o $binary @flags
if ($LASTEXITCODE -ne 0) { throw 'Window title test compilation failed' }
& xvfb-run -a --server-args='-screen 0 800x400x24 -nolisten tcp' $binary
if ($LASTEXITCODE -ne 0) { throw 'Window title identity verification failed' }
