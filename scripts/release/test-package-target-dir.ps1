# Independent cargo build -> package, with same-version stale repository binaries.
$ErrorActionPreference = 'Stop'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString('N'))
$previous = $env:CARGO_TARGET_DIR
New-Item -ItemType Directory "$fixture/scripts/release", "$fixture/src/bin" -Force | Out-Null
Copy-Item "$PSScriptRoot/package-cli.ps1" "$fixture/scripts/release"
Copy-Item "$PSScriptRoot/../../osdk.toml", "$PSScriptRoot/../../osdk.lock" $fixture
Push-Location $fixture
try {
    Set-Content README.md fixture
    Set-Content Cargo.toml "[package]`nname = 'fixture'`nversion = '0.0.4'`nedition = '2021'"
    $target = 'x86_64-unknown-linux-gnu'
    $helpers = "plugins/linux_atspi/.build-$target"
    New-Item -ItemType Directory $helpers -Force | Out-Null
    Set-Content "$helpers/x11_xtest_helper" fixture
    Set-Content "$helpers/x11_capture_helper" fixture
    foreach ($custom in @('relative-output', "$fixture/absolute-output", '', 'repository-config')) {
        $env:CARGO_TARGET_DIR = 'target'
        foreach ($name in @('aad', 'aad-browser')) {
            Set-Content "src/bin/$name.rs" 'fn main() { if std::env::args().any(|a| a == "--version") { println!("aad 0.0.4"); } else { println!("control --profile-source stale"); } }'
        }
        & cargo build --release --target $target
        if ($LASTEXITCODE) { throw 'Stale build failed' }
        if ($custom -eq 'repository-config') {
            New-Item -ItemType Directory .cargo -Force | Out-Null
            Set-Content .cargo/config.toml "[build]`ntarget-dir = 'fresh-output'"
        }
        $env:CARGO_TARGET_DIR = $custom
        if (-not $custom -or $custom -eq 'repository-config') { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
        foreach ($name in @('aad', 'aad-browser')) {
            Set-Content "src/bin/$name.rs" 'fn main() { if std::env::args().any(|a| a == "--version") { println!("aad 0.0.4"); } else { println!("control --profile-source fresh"); } }'
        }
        & cargo build --release --target $target
        if ($LASTEXITCODE) { throw 'Fresh build failed' }
        if ($custom -eq 'repository-config') {
            Push-Location ([IO.Path]::GetTempPath())
            try {
                $caller = (Get-Location).Path
                $archive = & "$fixture/scripts/release/package-cli.ps1" -Target $target -Version '0.0.4' -OutputDirectory "$fixture/dist"
                if ((Get-Location).Path -ne $caller) { throw 'Caller directory changed' }
            } finally { Pop-Location }
        } else {
            $archive = & ./scripts/release/package-cli.ps1 -Target $target -Version '0.0.4'
        }
        New-Item -ItemType Directory unpacked -Force | Out-Null
        & tar -xzf $archive -C unpacked
        if ($LASTEXITCODE) { throw 'Extract failed' }
        foreach ($name in @('aad', 'aad-browser')) {
            $help = & "./unpacked/aad-0.0.4-$target/$name" --help
            if ($help -notmatch 'fresh' -or $help -match 'stale') { throw "Wrong $name" }
        }
    }
    # Relative environment paths are relative to the caller, even when the
    # manifest and packaging script belong to a different directory.
    foreach ($callerDirectory in @("$fixture/subdirectory", "$fixture-external")) {
        New-Item -ItemType Directory $callerDirectory -Force | Out-Null
        Copy-Item "$fixture/osdk.toml", "$fixture/osdk.lock" $callerDirectory
        $env:CARGO_TARGET_DIR = 'target'
        foreach ($name in @('aad', 'aad-browser')) {
            Set-Content "src/bin/$name.rs" 'fn main() { if std::env::args().any(|a| a == "--version") { println!("aad 0.0.4"); } else { println!("control --profile-source stale"); } }'
        }
        & cargo build --release --target $target
        if ($LASTEXITCODE) { throw 'Stale build failed' }
        foreach ($name in @('aad', 'aad-browser')) {
            Set-Content "src/bin/$name.rs" 'fn main() { if std::env::args().any(|a| a == "--version") { println!("aad 0.0.4"); } else { println!("control --profile-source fresh"); } }'
        }
        Push-Location $callerDirectory
        try {
            & cargo build --manifest-path "$fixture/Cargo.toml" --release --target $target
            if ($LASTEXITCODE) { throw 'Caller-relative fresh build failed' }
            $archive = & "$fixture/scripts/release/package-cli.ps1" -Target $target -Version '0.0.4' -OutputDirectory "$callerDirectory/dist"
            if ((Get-Location).Path -ne $callerDirectory) { throw 'Caller directory changed' }
            if ($env:CARGO_TARGET_DIR -cne 'target') { throw 'Caller target environment changed' }
            New-Item -ItemType Directory unpacked -Force | Out-Null
            & tar -xzf $archive -C unpacked
            if ($LASTEXITCODE) { throw 'Caller-relative extract failed' }
            foreach ($name in @('aad', 'aad-browser')) {
                $help = & "./unpacked/aad-0.0.4-$target/$name" --help
                if ($LASTEXITCODE -or $help -notmatch 'fresh' -or $help -match 'stale') { throw "Wrong caller-relative $name" }
            }
        } finally { Pop-Location }
    }
    'PASS: relative, absolute, unset, repository config, subdirectory and external caller target directories'
} finally {
    Pop-Location
    $env:CARGO_TARGET_DIR = $previous
    Remove-Item $fixture -Recurse -Force
    Remove-Item "$fixture-external" -Recurse -Force -ErrorAction SilentlyContinue
}
