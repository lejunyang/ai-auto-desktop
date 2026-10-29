param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl')]
    [string]$Target,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [string]$AlpineMirror = 'https://dl-cdn.alpinelinux.org/alpine'
)

# Native-architecture builds only. A pinned, checksum-verified Alpine rootfs
# supplies musl C++ and X11 static libraries; Rust stays under the host's osdk.
# bubblewrap confines apk and the compiler to this temporary filesystem. It
# does not change the host's libraries or require a Docker daemon. PS 5.1 syntax.
$ErrorActionPreference = 'Stop'
$architecture = $Target.Split('-')[0]
$hostArchitecture = (& uname -m).Trim()
if ($hostArchitecture -ne $architecture) { throw "Build $Target on a native $architecture Linux runner" }
$mirrorUri = [uri]$AlpineMirror
if (-not $mirrorUri.IsAbsoluteUri -or $mirrorUri.Scheme -ne 'https' -or
    $mirrorUri.UserInfo -or $mirrorUri.Query -or $mirrorUri.Fragment) { throw 'Use a plain HTTPS Alpine mirror URL' }
$AlpineMirror = $AlpineMirror.TrimEnd('/')
foreach ($program in @('bwrap', 'tar', 'curl')) { Get-Command $program -ErrorAction Stop | Out-Null }
$digests = @{
    x86_64 = '57e364a241bab0699cd8a760c0fd7b270e9cbcbd3cee822cb2b07ca6d925c16c'
    aarch64 = '7e9b34ff3304381909e566e2c93de450664ccc724191261423974fc121c11623'
}
$repository = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
New-Item -ItemType Directory -Force $OutputDirectory | Out-Null
$output = (Resolve-Path $OutputDirectory).Path
& chmod 700 $output
if ($LASTEXITCODE) { throw 'Cannot protect helper output directory' }
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('aad-musl-' + [guid]::NewGuid().ToString('N'))
$rootfs = Join-Path $scratch 'rootfs'
New-Item -ItemType Directory -Path $rootfs -Force | Out-Null
try {
    $archive = Join-Path $scratch 'alpine.tar.gz'
    $url = "$AlpineMirror/v3.22/releases/$architecture/alpine-minirootfs-3.22.7-$architecture.tar.gz"
    & curl --fail --location --retry 3 --max-time 600 --output $archive $url
    if ($LASTEXITCODE) { throw 'Alpine rootfs download failed' }
    if ((Get-FileHash $archive -Algorithm SHA256).Hash.ToLower() -cne $digests[$architecture]) {
        throw 'Alpine rootfs SHA256 mismatch'
    }
    & tar --no-same-owner -xzf $archive -C $rootfs
    if ($LASTEXITCODE) { throw 'Alpine rootfs extraction failed' }
    Copy-Item /etc/resolv.conf (Join-Path $rootfs 'etc/resolv.conf') -Force
    # Use the host's trusted CA bundle for the initial package download too.
    Copy-Item /etc/ssl/certs/ca-certificates.crt (Join-Path $rootfs 'etc/ssl/cert.pem') -Force
    [IO.File]::WriteAllText((Join-Path $rootfs 'etc/apk/repositories'), "$AlpineMirror/v3.22/main`n")
    $build = @'
set -eu
apk add --no-cache g++ pkgconf curl xz meson libx11-dev libx11-static libxtst-dev libxtst-static libxext-static libxi-static libxcb-static
# Alpine 3.22's libXau Meson package ships only a shared library. Rebuild the
# matching source as static; the digest is from aports/3.22-stable/libxau.
cd /tmp
curl --fail --location --retry 3 --max-time 180 -o libXau.tar.xz https://www.x.org/releases/individual/lib/libXau-1.0.12.tar.xz
echo '4bbe8796f4a14340499d5f75046955905531ea2948944dfc3d6069f8b86c1710042bfc7918d459320557883e6631359d48e6173c69c62ff572314e864ff97c5e  libXau.tar.xz' | sha512sum -c -
tar -xf libXau.tar.xz
meson setup /tmp/xau-build /tmp/libXau-1.0.12 --prefix=/usr --default-library=static --buildtype=release
meson compile -C /tmp/xau-build
meson install -C /tmp/xau-build
export AAD_LINUX_STATIC=1
export AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR=/tmp/output
sh /src/plugins/linux_atspi/build_x11_xtest_helper.sh
sh /src/plugins/linux_atspi/build_x11_capture_helper.sh
'@
    & bwrap --unshare-user --uid 0 --gid 0 --unshare-pid --die-with-parent `
        --bind $rootfs / --dev /dev --proc /proc `
        --ro-bind (Join-Path $repository 'plugins') /src/plugins --bind $output /tmp/output `
        --chdir /src --setenv PATH /usr/sbin:/usr/bin:/sbin:/bin /bin/sh -c $build
    if ($LASTEXITCODE) { throw 'Static musl helper build failed' }
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
