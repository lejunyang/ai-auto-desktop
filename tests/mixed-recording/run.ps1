param([switch]$Inside)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '../..')
if (!$Inside) {
    & xvfb-run -a -s '-screen 0 1200x800x24 -nolisten tcp' dbus-run-session -- (Get-Process -Id $PID).Path -NoProfile -File $PSCommandPath -Inside
    exit $LASTEXITCODE
}

$dir = New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid()))
$env:XDG_RUNTIME_DIR = "$dir"
& chmod 700 "$dir"
$env:XDG_SESSION_TYPE = 'x11'
$env:XDG_CURRENT_DESKTOP = 'KDE'
$env:GDK_BACKEND = 'x11'
$env:GTK_A11Y = 'always'
$env:QT_ACCESSIBILITY = '1'
$env:QT_LINUX_ACCESSIBILITY_ALWAYS_ON = '1'
Remove-Item Env:NO_AT_BRIDGE, Env:AT_SPI_BUS_ADDRESS -ErrorAction SilentlyContinue
if (!$env:AAD_TEST_CHROME) { $env:AAD_TEST_CHROME = (Get-Command microsoft-edge).Source }
if (!$env:AAD_LINUX_XTEST_HELPER) {
    & ./plugins/linux_atspi/build_x11_xtest_helper.sh
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    $env:AAD_LINUX_XTEST_HELPER = (Resolve-Path plugins/linux_atspi/.build/x11_xtest_helper).Path
}
$wmCommand = Get-Command openbox, xfwm4 -ErrorAction SilentlyContinue | Select-Object -First 1
$wm = Start-Process $wmCommand.Source -PassThru -RedirectStandardError "$dir/wm.log"
try {
    for ($i = 0; $i -lt 80; $i++) {
        if ((xprop -root _NET_SUPPORTING_WM_CHECK) -match 'window id') { break }
        Start-Sleep -Milliseconds 100
    }
    $env:AAD_MIXED_FIXTURE = "$dir/mixed-qt"
    $flags = (pkg-config --cflags --libs Qt5Widgets) -split '\s+' | Where-Object { $_ }
    & g++ -std=c++17 -fPIC -no-pie tests/mixed-recording/windows.cpp -o $env:AAD_MIXED_FIXTURE @flags
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    & node --test tests/mixed-recording/live.test.mjs
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    $env:AAD_MIXED_FIXTURE = "$dir/mixed-gtk"
    $flags = (pkg-config --cflags --libs gtk+-3.0) -split '\s+' | Where-Object { $_ }
    & cc tests/mixed-recording/windows.c -o $env:AAD_MIXED_FIXTURE @flags
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    & node --test tests/mixed-recording/live.test.mjs
    exit $LASTEXITCODE
} finally {
    if (!$wm.HasExited) { $wm.Kill(); $wm.WaitForExit() }
    Remove-Item "$dir" -Recurse -Force
}
