# Exercise the real build entry and packager with deterministic compiler output.
$ErrorActionPreference = 'Stop'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString('N'))
$previous = $env:CARGO_TARGET_DIR
try {
    New-Item -ItemType Directory "$fixture/scripts/release" -Force | Out-Null
    Copy-Item "$PSScriptRoot/build-local-linux.ps1", "$PSScriptRoot/package-cli.ps1" "$fixture/scripts/release"
    Set-Content "$fixture/README.md" 'fixture'
    function global:git {
        if ($args[0] -eq 'rev-parse') { 'fixture-commit' }
        $global:LASTEXITCODE = 0
    }
    function global:osdk {
        if ($args -contains 'rustc') {
            'release: 1.98.0'
            'host: x86_64-unknown-linux-gnu'
        } else {
            $index = [array]::IndexOf($args, '--target-dir')
            if ($index -lt 0) { throw 'Build did not pin target-dir' }
            $directory = $args[$index + 1]
            if ($directory -ne (Join-Path $fixture 'target')) { throw 'Unexpected target-dir' }
            foreach ($name in @('aad', 'aad-browser')) {
                Set-Content "$directory/x86_64-unknown-linux-gnu/release/$name" "#!/bin/sh`n# freshly-built`nif [ `"`$1`" = --version ]; then echo 'aad 0.0.4'; else echo 'control --profile-source'; fi"
                & chmod +x "$directory/x86_64-unknown-linux-gnu/release/$name"
            }
        }
        $global:LASTEXITCODE = 0
    }
    function global:sh {
        New-Item -ItemType Directory $env:AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR -Force | Out-Null
        foreach ($name in @('x11_xtest_helper', 'x11_capture_helper')) {
            Set-Content (Join-Path $env:AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR $name) 'fresh-helper'
        }
        $global:LASTEXITCODE = 0
    }
    foreach ($custom in @('relative-output', (Join-Path $fixture 'external-output'), '')) {
        $env:CARGO_TARGET_DIR = $custom
        $release = "$fixture/target/x86_64-unknown-linux-gnu/release"
        New-Item -ItemType Directory $release -Force | Out-Null
        foreach ($name in @('aad', 'aad-browser')) {
            Set-Content "$release/$name" "#!/bin/sh`n# stale-same-version`necho 'aad 0.0.4'"
            & chmod +x "$release/$name"
        }
        & "$fixture/scripts/release/build-local-linux.ps1" -OutputDirectory "$fixture/dist"
        New-Item -ItemType Directory "$fixture/unpacked" -Force | Out-Null
        & tar -xzf "$fixture/dist/aad-0.0.4-x86_64-unknown-linux-gnu.tar.gz" -C "$fixture/unpacked"
        foreach ($name in @('aad', 'aad-browser')) {
            $content = Get-Content "$fixture/unpacked/aad-0.0.4-x86_64-unknown-linux-gnu/$name" -Raw
            if ($content -notmatch 'freshly-built' -or $content -match 'stale-same-version') { throw "Stale $name packaged" }
        }
    }
    'PASS: relative, absolute and unset CARGO_TARGET_DIR package fresh build'
} finally {
    $env:CARGO_TARGET_DIR = $previous
    Remove-Item Function:git, Function:osdk, Function:sh -ErrorAction SilentlyContinue
    Remove-Item $fixture -Recurse -Force
}
