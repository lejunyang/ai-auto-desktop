param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu',
        'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl',
        'x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc',
        'x86_64-apple-darwin', 'aarch64-apple-darwin')]
    [string]$Target,
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$OutputDirectory = 'dist'
)

# Package the actual target build, preserving helper bundles and executable
# modes. The target runner executes the extracted CLI too. PS 5.1 syntax.
$ErrorActionPreference = 'Stop'
$repository = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
New-Item -ItemType Directory -Force $OutputDirectory | Out-Null
$output = (Resolve-Path $OutputDirectory).Path
$name = "aad-$Version-$Target"
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('aad-package-' + [guid]::NewGuid().ToString('N'))
$staging = Join-Path $scratch $name
$unpacked = Join-Path $scratch 'verify'
New-Item -ItemType Directory -Path $staging, $unpacked -Force | Out-Null
$windowsTarget = $Target.EndsWith('-windows-msvc')
$executable = 'aad'
if ($windowsTarget) { $executable = 'aad.exe' }
$browserHelper = 'aad-browser'
if ($windowsTarget) { $browserHelper = 'aad-browser.exe' }
try {
    Copy-Item (Join-Path $repository "target/$Target/release/$executable") $staging
    Copy-Item (Join-Path $repository "target/$Target/release/$browserHelper") $staging
    if ($Target.Contains('-linux-')) {
        $helpers = Join-Path $repository "plugins/linux_atspi/.build-$Target"
        Copy-Item (Join-Path $helpers 'x11_xtest_helper') (Join-Path $staging 'aad-linux-xtest-helper')
        Copy-Item (Join-Path $helpers 'x11_capture_helper') (Join-Path $staging 'aad-linux-capture-helper')
    }
    if ($Target.EndsWith('-apple-darwin')) {
        $helper = Join-Path $repository 'plugins/macos_ax/.build/MacOSAXHelper.app'
        # Native lipo inspection catches a helper built for the runner rather
        # than the requested architecture before it enters the archive.
        $architecture = 'x86_64'
        if ($Target.StartsWith('aarch64-')) { $architecture = 'arm64' }
        & lipo (Join-Path $helper 'Contents/MacOS/MacOSAXHelper') -verify_arch $architecture
        if ($LASTEXITCODE) { throw 'macOS helper architecture mismatch' }
        & lipo (Join-Path $staging $executable) -verify_arch $architecture
        if ($LASTEXITCODE) { throw 'macOS CLI architecture mismatch' }
        & lipo (Join-Path $staging $browserHelper) -verify_arch $architecture
        if ($LASTEXITCODE) { throw 'macOS browser helper architecture mismatch' }
        & cp -R $helper $staging
        if ($LASTEXITCODE) { throw 'Cannot copy signed macOS helper' }
    }
    Copy-Item (Join-Path $repository 'README.md') $staging
    if ($windowsTarget) {
        # x64 emulation could hide an accidentally packaged x64 executable on
        # an ARM host; verify the PE machine field independently of execution.
        $expected = 0x8664
        if ($Target.StartsWith('aarch64-')) { $expected = 0xaa64 }
        foreach ($binary in @($executable, $browserHelper)) {
            $pe = [IO.File]::ReadAllBytes((Join-Path $staging $binary))
            $offset = [BitConverter]::ToInt32($pe, 0x3c)
            $machine = [BitConverter]::ToUInt16($pe, $offset + 4)
            if ($machine -ne $expected) { throw "Windows $binary architecture mismatch" }
        }
        $archive = Join-Path $output "$name.zip"
        Compress-Archive -Path $staging -DestinationPath $archive -Force
        Expand-Archive -LiteralPath $archive -DestinationPath $unpacked
    } else {
        $archive = Join-Path $output "$name.tar.gz"
        & tar -czf $archive -C $scratch $name
        if ($LASTEXITCODE) { throw 'Cannot package CLI archive' }
        & tar -xzf $archive -C $unpacked
        if ($LASTEXITCODE) { throw 'Cannot read back CLI archive' }
    }
    $cli = Join-Path $unpacked "$name/$executable"
    $actual = & $cli --version
    if ($LASTEXITCODE -ne 0 -or $actual -cne "aad $Version") { throw "Packaged CLI version mismatch: $actual" }
    $helpText = & $cli --help
    if ($LASTEXITCODE -ne 0 -or ($helpText -join "`n") -notmatch 'control') { throw 'Packaged CLI help failed' }
    $browserHelp = & (Join-Path $unpacked "$name/$browserHelper") --help
    if ($LASTEXITCODE -ne 0 -or ($browserHelp -join "`n") -notmatch '--profile-source') { throw 'Packaged browser helper failed' }
    if ($Target.EndsWith('-apple-darwin')) {
        & /usr/bin/codesign --verify --strict --deep (Join-Path $unpacked "$name/MacOSAXHelper.app")
        if ($LASTEXITCODE) { throw 'Extracted macOS helper signature failed verification' }
    }
    $hash = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
    [IO.File]::WriteAllText("$archive.sha256", "$hash  $([IO.Path]::GetFileName($archive))`n")
    Write-Output $archive
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
