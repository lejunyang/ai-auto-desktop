param([string]$OutputDirectory = 'dist')

# Native GNU Linux build. All generated files stay in the checkout.
$ErrorActionPreference = 'Stop'
$repository = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
Push-Location $repository
$previousHelperDirectory = $env:AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR
try {
    $dirty = & git status --porcelain --untracked-files=normal
    if ($LASTEXITCODE -or $dirty) { throw 'Commit source changes before building a traceable package.' }
    $commit = & git rev-parse HEAD
    if ($LASTEXITCODE) { throw 'Cannot identify source commit' }
    $compiler = & osdk exec -- rustc -vV
    if ($LASTEXITCODE -or ($compiler -join "`n") -notmatch 'release: 1\.98\.0\s') {
        throw 'Expected osdk-managed Rust 1.98.0'
    }
    $target = (($compiler | Where-Object { $_ -like 'host: *' }) -replace '^host: ', '').Trim()
    if ($target -notmatch '^(x86_64|aarch64)-unknown-linux-gnu$') { throw "Unsupported native host: $target" }
    $targetDirectory = Join-Path $repository "target"
    & osdk exec -- cargo build --release --locked -p aad-cli -p aad-browser --bins --target $target --target-dir $targetDirectory
    if ($LASTEXITCODE) { throw 'Rust build failed' }
    $env:AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR = Join-Path $repository "plugins/linux_atspi/.build-$target"
    foreach ($helper in @('xtest', 'capture')) {
        & sh "plugins/linux_atspi/build_x11_${helper}_helper.sh"
        if ($LASTEXITCODE) { throw "$helper build failed" }
    }
    $versionOutput = & (Join-Path $targetDirectory "$target/release/aad") --version
    if ($LASTEXITCODE -or $versionOutput -notmatch '^aad (\S+)$') { throw 'CLI version check failed' }
    $version = $Matches[1]
    & "$PSScriptRoot/package-cli.ps1" -Target $target -Version $version -TargetDirectory $targetDirectory -OutputDirectory $OutputDirectory
    $archive = Join-Path $OutputDirectory "aad-$version-$target.tar.gz"
    $manifest = [ordered]@{
        commit = $commit
        version = $version
        target = $target
        rustc = $compiler
        archive_sha256 = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
        installation_performed = $false
    }
    $manifest | ConvertTo-Json -Depth 4 | Set-Content "$archive.source.json" -Encoding UTF8
} finally {
    $env:AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR = $previousHelperDirectory
    Pop-Location
}
