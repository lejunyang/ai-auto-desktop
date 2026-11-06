param([string]$BuildDirectory = '', [switch]$BuildOnly)

# All effect checks run on a private X server, never the user's desktop.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-session-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$binary = Join-Path $BuildDirectory 'x11-session-protocol-test'
$capture = Join-Path $BuildDirectory 'x11_capture_helper'
$inputHelper = Join-Path $BuildDirectory 'x11_xtest_helper'
$flags = ((& pkg-config --cflags --libs x11 xtst) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'X11/XTest development packages unavailable' }
& g++ -std=c++17 -O2 -Wall -Wextra -Werror (Join-Path $root 'tests/linux/x11_session_protocol_test.cpp') -o $binary @flags
if ($LASTEXITCODE -ne 0) { throw 'session protocol test build failed' }
& g++ -std=c++17 -O2 -Wall -Wextra -Werror (Join-Path $root 'plugins/linux_atspi/x11_xtest_helper.cpp') -o $inputHelper @flags
if ($LASTEXITCODE -ne 0) { throw 'input helper build failed' }
& g++ -std=c++17 -O2 -Wall -Wextra -Werror -fPIE -D_FORTIFY_SOURCE=2 (Join-Path $root 'plugins/linux_atspi/x11_capture_helper.cpp') -o $capture @flags -pie '-Wl,-z,relro,-z,now'
if ($LASTEXITCODE -ne 0) { throw 'capture helper build failed' }
if (-not $BuildOnly) {
    & xvfb-run -a --server-args='-screen 0 640x480x24 -nolisten tcp' $binary $capture
    if ($LASTEXITCODE -ne 0) { throw 'session protocol test failed' }
}
