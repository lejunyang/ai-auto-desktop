# Requires pwsh, Node, Chromium, the selected XDG file portal, and KWin Wayland
# (--virtual) or Xvfb + xfwm4. Run one private display at a time.
param(
    [ValidateSet('wayland', 'x11')][string]$Desktop = 'wayland',
    [ValidateSet('gtk', 'kde')][string]$Backend = 'gtk',
    [string]$Chrome = $env:AAD_TEST_CHROME,
    [string]$Aad = $env:AAD_TEST_CLI,
    [switch]$DirectGtk4,
    [switch]$RecordSaveAs,
    [switch]$InsideSession
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '../..')

if ($InsideSession) {
    if ($Desktop -eq 'wayland') {
        $wm = Start-Process kwin_wayland -ArgumentList '--virtual','--no-lockscreen','--socket',$env:WAYLAND_DISPLAY -PassThru
    } else {
        $wm = Start-Process xfwm4 -ArgumentList '--compositor=off' -PassThru
    }
    try {
        $ready = $false
        for ($i = 0; $i -lt 150 -and !$wm.HasExited; $i++) {
            if ($Desktop -eq 'wayland') {
                $ready = Test-Path (Join-Path $env:XDG_RUNTIME_DIR $env:WAYLAND_DISPLAY)
            } else {
                $ready = (xprop -root _NET_SUPPORTING_WM_CHECK) -match '0x[1-9a-f][0-9a-f]*'
            }
            if ($ready) { break }
            Start-Sleep -Milliseconds 80
        }
        if (!$ready) { throw 'Private window manager did not start' }
        if (!$DirectGtk4) {
            & gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop -m org.freedesktop.DBus.Properties.Get org.freedesktop.portal.FileChooser version
            if ($LASTEXITCODE) { throw 'File portal did not start' }
        }
        $test = if ($RecordSaveAs) { 'tests/browser/record-save-as.test.mjs' } elseif ($DirectGtk4) { 'tests/browser/native-gtk4-save.test.mjs' } elseif ($Backend -eq 'kde') { 'tests/browser/native-kde-picker.test.mjs' } else { 'tests/browser/native-portal-picker.test.mjs' }
        & node --test $test
        exit $LASTEXITCODE
    } finally {
        if (!$wm.HasExited) { $wm.Kill(); $wm.WaitForExit() }
    }
}

if (!$Chrome) { throw 'Supply -Chrome or set AAD_TEST_CHROME' }
if ($DirectGtk4 -and $Desktop -ne 'x11') { throw 'Direct GTK4 verification requires X11' }
if ($Backend -eq 'kde' -and $Desktop -ne 'wayland') { throw 'KDE verification requires private KWin Wayland' }
if (!$Aad) { $Aad = 'target/debug/aad' }
$env:AAD_TEST_CHROME = (Get-Command $Chrome -CommandType Application).Source
$env:AAD_TEST_CLI = (Get-Command $Aad -CommandType Application).Source
$env:AAD_TEST_FILE_PORTAL = if ($DirectGtk4) { '0' } else { '1' }
$env:AAD_TEST_DIRECT_GTK4_SAVE = if ($DirectGtk4) { '1' } else { '0' }
$env:AAD_TEST_KDE_PICKER = if ($Backend -eq 'kde') { '1' } else { '0' }
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('p' + [Guid]::NewGuid().ToString('N').Substring(0,7))
$null = New-Item $scratch -ItemType Directory
try {
    $env:XDG_RUNTIME_DIR = Join-Path $scratch 'r'
    $env:XDG_CONFIG_HOME = Join-Path $scratch 'cfg'
    $env:XDG_CACHE_HOME = Join-Path $scratch 'cache'
    $env:XDG_DATA_HOME = Join-Path $scratch 'data'
    $null = New-Item $env:XDG_RUNTIME_DIR,$env:XDG_CONFIG_HOME,$env:XDG_CACHE_HOME,$env:XDG_DATA_HOME -ItemType Directory
    & chmod 700 $scratch $env:XDG_RUNTIME_DIR
    if ($LASTEXITCODE) { throw 'Could not protect private runtime directory' }
    $portalConfig = New-Item (Join-Path $env:XDG_CONFIG_HOME 'xdg-desktop-portal') -ItemType Directory
    "[preferred]`ndefault=$Backend" | Set-Content (Join-Path $portalConfig.FullName 'portals.conf')
    if (!$DirectGtk4) {
        # Older portal versions predate portals.conf. Give this private bus
        # only the selected installed backend.
        $env:XDG_DESKTOP_PORTAL_DIR = Join-Path $scratch 'portals'
        $null = New-Item $env:XDG_DESKTOP_PORTAL_DIR -ItemType Directory
        Copy-Item "/usr/share/xdg-desktop-portal/portals/$Backend.portal" $env:XDG_DESKTOP_PORTAL_DIR
    }
    $env:TMPDIR = $scratch
    $env:XDG_SESSION_TYPE = $Desktop
    $env:GDK_BACKEND = $Desktop
    $env:GTK_A11Y = 'atspi'
    $env:LANGUAGE = 'en'
    $env:LC_ALL = 'C.UTF-8'
    $env:GSETTINGS_BACKEND = 'memory'
    Remove-Item Env:DISPLAY,Env:WAYLAND_DISPLAY,Env:AT_SPI_BUS_ADDRESS,Env:NO_AT_BRIDGE,Env:SESSION_MANAGER,Env:QT_QPA_PLATFORM -ErrorAction SilentlyContinue
    $inner = @('--', (Get-Process -Id $PID).Path, '-NoProfile', '-File', $PSCommandPath, '-InsideSession', '-Desktop', $Desktop, '-Backend', $Backend)
    if ($RecordSaveAs) { $inner += '-RecordSaveAs' }
    if ($DirectGtk4) {
        $services = New-Item (Join-Path $env:XDG_DATA_HOME 'dbus-1/services') -ItemType Directory -Force
        "[D-BUS Service]`nName=org.freedesktop.portal.Desktop`nExec=/usr/bin/false" | Set-Content (Join-Path $services.FullName 'org.freedesktop.portal.Desktop.service')
        $inner += '-DirectGtk4'
    }
    if ($Desktop -eq 'wayland') {
        $env:WAYLAND_DISPLAY = 'picker'
        $env:XDG_CURRENT_DESKTOP = 'GNOME'
        if ($Backend -eq 'kde') {
            $env:XDG_CURRENT_DESKTOP = 'KDE'
            $env:QT_QPA_PLATFORM = 'wayland'
            $env:QT_LINUX_ACCESSIBILITY_ALWAYS_ON = '1'
            $env:QT_ACCESSIBILITY = '1'
            $env:KDE_FULL_SESSION = 'true'
        }
        & dbus-run-session @inner
    } else {
        $env:XDG_CURRENT_DESKTOP = 'XFCE'
        & xvfb-run -a -n 411 -s '-screen 0 1280x960x24 -nolisten tcp' dbus-run-session @inner
    }
    exit $LASTEXITCODE
} finally {
    Remove-Item -Recurse -Force $scratch
}
