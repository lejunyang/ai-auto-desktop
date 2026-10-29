param(
    [Parameter(Mandatory = $true)][string]$Archive,
    [Parameter(Mandatory = $true)][string]$Version,
    [ValidateSet('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu',
        'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl')]
    [string]$Target = 'x86_64-unknown-linux-gnu',
    [version]$MaxGlibc = '2.35'
)

# Inspect the extracted release, not the build tree. Compatible with PS 5.1;
# execution requires Linux, tar and readelf (plus bubblewrap for musl).
$ErrorActionPreference = 'Stop'
$archivePath = (Resolve-Path -LiteralPath $Archive).Path
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('aad-release-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
$previousLocale = $env:LC_ALL
try {
    # readelf translates header labels under the user's locale.
    $env:LC_ALL = 'C'
    & tar -xzf $archivePath -C $scratch
    if ($LASTEXITCODE -ne 0) { throw 'Cannot extract the release archive' }
    $root = Join-Path $scratch "aad-$Version-$Target"
    foreach ($name in @('aad', 'aad-browser', 'aad-linux-xtest-helper', 'aad-linux-capture-helper')) {
        $binary = Join-Path $root $name
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Missing $name" }
        & test -x $binary
        if ($LASTEXITCODE -ne 0) { throw "$name is not executable" }
        $header = & readelf --file-header $binary
        if ($LASTEXITCODE -ne 0) { throw "Cannot read $name ELF header" }
        $machine = 'Advanced Micro Devices X86-64'
        if ($Target.StartsWith('aarch64-')) { $machine = 'AArch64' }
        if (($header -join "`n") -notmatch "Machine:\s+$machine\s") { throw "$name has the wrong architecture for $Target" }
        $symbols = & readelf --version-info $binary
        if ($LASTEXITCODE -ne 0) { throw "Cannot inspect $name" }
        if ($Target.EndsWith('-musl')) {
            $segments = & readelf --program-headers $binary
            if ($LASTEXITCODE -ne 0) { throw "Cannot inspect $name segments" }
            $dynamic = & readelf --dynamic $binary
            if ($LASTEXITCODE -ne 0) { throw "Cannot inspect $name dynamic dependencies" }
            if (($segments -join "`n") -match '\bINTERP\b' -or
                ($dynamic -join "`n") -match '\(NEEDED\)' -or
                ($symbols -join "`n") -match 'GLIBC_') {
                throw "$name is not fully static; the musl archive must not need a loader or shared libraries"
            }
            Write-Output "$name is static $machine (no interpreter, shared libraries or GLIBC symbols)"
            continue
        }
        $versions = @([regex]::Matches(($symbols -join "`n"), 'Name: GLIBC_(\d+\.\d+(?:\.\d+)?)\b') |
            ForEach-Object { [version]$_.Groups[1].Value } | Sort-Object -Unique)
        if ($versions.Count -eq 0) { throw "$name has no verifiable glibc requirements" }
        $required = $versions[-1]
        if ($required -gt $MaxGlibc) { throw "$name requires GLIBC_$required; release baseline is GLIBC_$MaxGlibc" }
        Write-Output "$name requires GLIBC_$required (baseline $MaxGlibc)"
    }
    $aad = Join-Path $root 'aad'
    $actual = & $aad --version
    if ($LASTEXITCODE -ne 0 -or $actual -cne "aad $Version") { throw "Release version mismatch: $actual" }
    $helpText = & $aad --help
    if ($LASTEXITCODE -ne 0 -or ($helpText -join "`n") -notmatch 'control') { throw 'Release CLI help failed' }
    if ($Target.EndsWith('-musl')) {
        # The host may have musl installed. An empty root proves startup does
        # not accidentally depend on that loader or on any host shared library.
        $isolatedVersion = & bwrap --unshare-user --unshare-pid --unshare-net --die-with-parent `
            --tmpfs / --ro-bind $root /app --dev /dev --proc /proc --chdir /app --clearenv /app/aad --version
        if ($LASTEXITCODE -ne 0 -or $isolatedVersion -cne "aad $Version") { throw 'Static CLI failed in an empty filesystem' }
        Write-Output 'Static CLI starts in an empty filesystem without host libraries'
    }
    Write-Output "Linux release $Version ($Target) passed"
} finally {
    $env:LC_ALL = $previousLocale
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
