param([string]$BuildDirectory = '')

# The handshake must work with no X server and must not consume stdin.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-helper-protocol-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$binary = Join-Path $BuildDirectory 'x11_xtest_helper'
$flags = ((& pkg-config --cflags --libs x11 xtst) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'X11/XTest development packages unavailable' }
& g++ -std=c++17 -O2 -Wall -Wextra -Werror `
    (Join-Path $root 'plugins/linux_atspi/x11_xtest_helper.cpp') -o $binary @flags
if ($LASTEXITCODE -ne 0) { throw 'helper build failed' }
$description = & env -u DISPLAY -u XAUTHORITY -u XDG_SESSION_TYPE -u XDG_CURRENT_DESKTOP $binary describe | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or $description.helper -ne 'aad-linux-xtest' -or $description.protocol -ne 1) {
    throw 'invalid helper handshake'
}
$required = @('focus-info', 'type-text', 'pointer-click', 'pointer-gesture', 'drag', 'key-chord',
    'window-info', 'window-action', 'paste', 'clipboard-read', 'global-key', 'record-input', 'pointer-modifiers',
    'anchored-drag-path', 'anchored-drag-between')
foreach ($feature in $required) {
    if ($description.features -notcontains $feature) { throw "missing helper feature: $feature" }
}
if (@($description.features | Select-Object -Unique).Count -ne @($description.features).Count) {
    throw 'duplicate features'
}
$invalid = & env -u DISPLAY -u XAUTHORITY $binary describe --unexpected | ConvertFrom-Json
if ($LASTEXITCODE -eq 0 -or $invalid.dispatch_started -ne $false) {
    throw 'invalid describe arguments were not rejected without input'
}
Write-Output 'PASS: helper protocol and feature handshake without any display'
