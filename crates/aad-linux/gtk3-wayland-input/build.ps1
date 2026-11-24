param([Parameter(Mandatory=$true)][string]$BuildDirectory)
$ErrorActionPreference = 'Stop'
$output = New-Item -ItemType Directory -Force $BuildDirectory
$library = Join-Path $output.FullName 'libaad-gtk3-input.so'
$flags = ((& pkg-config --cflags --libs gtk+-wayland-3.0 atk-bridge-2.0 atspi-2 dbus-1) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE) { throw 'GTK 3 Wayland and AT-SPI development packages are required' }
& cc -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared (Join-Path $PSScriptRoot 'entry.c') -o $library @flags
if ($LASTEXITCODE) { throw 'Could not build the GTK 3 input module' }
$library
