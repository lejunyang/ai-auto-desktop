# The checkout's aad and helper must be built before this suite is run.
param(
    [Parameter(Mandatory = $true)][string]$Output,
    [ValidateRange(1, 5)][int]$SampleCount = 5
)
$ErrorActionPreference = 'Stop'
$root = (Get-Location).Path
$aad = Join-Path $root 'target/debug/aad'
$env:AAD_LINUX_XTEST_HELPER = Join-Path $root 'plugins/linux_atspi/.build/x11_xtest_helper'
foreach ($binary in $aad, $env:AAD_LINUX_XTEST_HELPER) {
    if (!(Test-Path $binary -PathType Leaf)) { throw "Build $binary before running the GTK fixture." }
}
$null = New-Item -ItemType Directory -Force -Path $Output
$outputPath = (Resolve-Path $Output).Path
$scratch = Join-Path $root ('target/timed-click-' + [Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $scratch
try {
    chmod 700 $scratch
    $fixture = Join-Path $scratch 'gtk-timed-click-fixture'
    $flags = (& pkg-config --cflags --libs gtk+-3.0) -split '\s+' | Where-Object { $_ }
    cc -std=c11 -O2 -Wall -Wextra -Werror (Join-Path $PSScriptRoot 'gtk_timed_click_fixture.c') -o $fixture @flags
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    foreach ($name in 'TMPDIR','XDG_RUNTIME_DIR','XDG_CONFIG_HOME','XDG_DATA_HOME','XDG_CACHE_HOME',
                     'XDG_STATE_HOME','AAD_CONTROL_LEASE_DIR','AAD_RECORDINGS_DIR') {
        $path = Join-Path $scratch $name
        $null = New-Item -ItemType Directory -Path $path
        chmod 700 $path
        Set-Item -Path "Env:$name" -Value $path
    }
    foreach ($name in 'DISPLAY','XAUTHORITY','DBUS_SESSION_BUS_ADDRESS','AT_SPI_BUS_ADDRESS','WAYLAND_DISPLAY',
                     'GDK_SCALE','GDK_DPI_SCALE','NO_AT_BRIDGE','GTK_MODULES','AAD_CDP_PORT') {
        Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    }
    $env:XDG_SESSION_TYPE = 'x11'
    $env:XDG_CURRENT_DESKTOP = 'XFCE'
    $env:GDK_BACKEND = 'x11'
    $env:GTK_A11Y = 'atspi'
    $env:LC_ALL = 'C.UTF-8'
    $xvfbLog = Join-Path $outputPath 'xvfb.log'
    $script = Join-Path $PSScriptRoot 'verify_x11_timed_click.py'
    & xvfb-run --auto-servernum --server-num=118 '--server-args=-screen 0 1024x768x24 -nolisten tcp -noreset' "--error-file=$xvfbLog" dbus-run-session -- /usr/bin/python3 $script $aad $fixture $outputPath --sample-count $SampleCount
    exit $LASTEXITCODE
} finally {
    Remove-Item -Recurse -Force $scratch
}
