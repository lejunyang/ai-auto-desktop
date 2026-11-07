param(
    [string]$Aad = '',
    [string]$InputHelper = '',
    [string]$CaptureHelper = '',
    [string]$BuildDirectory = '',
    [switch]$BuildOnly
)
$ErrorActionPreference = 'Stop'
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-scoped-capture-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$fixture = Join-Path $BuildDirectory 'qt-scoped-capture-fixture'
$flags = ((& pkg-config --cflags --libs Qt5Widgets) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Qt5Widgets development packages unavailable' }
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror (Join-Path $PSScriptRoot 'qt_scoped_capture_fixture.cpp') -o $fixture @flags
if ($LASTEXITCODE -ne 0) { throw 'Scoped capture fixture build failed' }
if ($BuildOnly) { return }
if (-not $Aad -or -not $InputHelper -or -not $CaptureHelper) { throw 'Aad, InputHelper and CaptureHelper are required' }
& python3 (Join-Path $PSScriptRoot 'scoped_capture.py') --aad (Resolve-Path $Aad).Path `
    --helper (Resolve-Path $InputHelper).Path --capture-helper (Resolve-Path $CaptureHelper).Path `
    --fixture $fixture --output $BuildDirectory
if ($LASTEXITCODE -ne 0) { throw 'Scoped capture verification failed; inspect summary and native records' }
