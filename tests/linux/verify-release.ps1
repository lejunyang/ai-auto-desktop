param(
    [Parameter(Mandatory = $true)][string]$Archive,
    [Parameter(Mandatory = $true)][string]$Version,
    [version]$MaxGlibc = '2.35'
)

# Inspect the extracted release, not the build tree. Compatible with PS 5.1;
# execution requires Linux, tar and readelf.
$ErrorActionPreference = 'Stop'
$archivePath = (Resolve-Path -LiteralPath $Archive).Path
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('aad-release-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    & tar -xzf $archivePath -C $scratch
    if ($LASTEXITCODE -ne 0) { throw 'Cannot extract the release archive' }
    $root = Join-Path $scratch "aad-$Version-x86_64-unknown-linux-gnu"
    foreach ($name in @('aad', 'aad-linux-xtest-helper', 'aad-linux-capture-helper')) {
        $binary = Join-Path $root $name
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Missing $name" }
        & test -x $binary
        if ($LASTEXITCODE -ne 0) { throw "$name is not executable" }
        $symbols = & readelf --version-info $binary
        if ($LASTEXITCODE -ne 0) { throw "Cannot inspect $name" }
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
    Write-Output "Linux release $Version passed"
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
