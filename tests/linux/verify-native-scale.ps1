param(
    [Parameter(Mandatory = $true)][string]$Aad,
    [Parameter(Mandatory = $true)][string]$InputHelper,
    [Parameter(Mandatory = $true)][string]$CaptureHelper,
    [string]$BuildDirectory = '',
    [string]$Python = 'python3'
)

# Run alone on an authorized KDE/X11 desktop. Only owned Qt process scaling is
# changed. A Wayland-labelled negative case checks refusal, not Wayland support.
$ErrorActionPreference = 'Stop'
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-scale-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$binary = Join-Path $BuildDirectory 'qt-scale-fixture'
$flags = ((& pkg-config --cflags --libs Qt5Widgets) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Qt5Widgets development packages unavailable' }
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror (Join-Path $PSScriptRoot 'qt_scale_fixture.cpp') -o $binary @flags
if ($LASTEXITCODE -ne 0) { throw 'Qt scale fixture build failed' }
& $Python (Join-Path $PSScriptRoot 'native_scale.py') --aad (Resolve-Path $Aad).Path `
    --helper (Resolve-Path $InputHelper).Path --capture-helper (Resolve-Path $CaptureHelper).Path `
    --fixture $binary --output $BuildDirectory
if ($LASTEXITCODE -ne 0) { throw 'Native scale verification failed; inspect summary and native records' }
