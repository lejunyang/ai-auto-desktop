param(
    [Parameter(Mandatory = $true)][string]$Aad,
    [Parameter(Mandatory = $true)][string]$Fixture,
    [string]$ComposingMethod = 'pinyin'
)

# Run alone on a KDE/X11 desktop with Fcitx5. Fixture is the executable built
# from qt_atspi_fixture.cpp. Only this new window is changed. PS 5.1 syntax.
$ErrorActionPreference = 'Stop'
$Aad = (Resolve-Path -LiteralPath $Aad).Path
$Fixture = (Resolve-Path -LiteralPath $Fixture).Path
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('aad-fcitx-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
$oldEnvironment = @{}
foreach ($key in @('QT_ACCESSIBILITY', 'QT_LINUX_ACCESSIBILITY_ALWAYS_ON', 'QT_IM_MODULE')) {
    $oldEnvironment[$key] = [Environment]::GetEnvironmentVariable($key)
}
$env:QT_ACCESSIBILITY = '1'
$env:QT_LINUX_ACCESSIBILITY_ALWAYS_ON = '1'
$env:QT_IM_MODULE = 'fcitx'
$process = $null
$originalMethod = $null
$field = @('--role', 'text', '--name', 'Qt fixture text entry')
function Invoke-Aad([string[]]$Arguments, [switch]$ExpectFailure) {
    $raw = & $Aad --compact @Arguments
    if (($LASTEXITCODE -eq 0) -eq [bool]$ExpectFailure) { throw "Unexpected aad exit: $raw" }
    return ($raw | ConvertFrom-Json)
}
function Invoke-InputMethod([string]$Method, [string[]]$Arguments = @()) {
    $reply = & gdbus call --session --dest org.fcitx.Fcitx5 --object-path /controller `
        --method "org.fcitx.Fcitx.Controller1.$Method" @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Fcitx5 $Method failed" }
    return ($reply -join "`n")
}
function Assert-Text([string]$Expected) {
    $until = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $found = Invoke-Aad (@('find', $resource) + $field)
        if ($found.node.value -ceq $Expected) { return }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $until)
    throw "Actual text '$($found.node.value)' differs from '$Expected'"
}
function Set-TestInputMethod([string]$Method) {
    Invoke-InputMethod 'SetCurrentIM' @($Method) | Out-Null
    $until = [DateTime]::UtcNow.AddSeconds(3)
    do {
        $current = Invoke-InputMethod 'CurrentInputMethod'
        if ($current -ceq "('$Method',)") { return }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $until)
    throw "Input method '$Method' is not active: $current"
}
try {
    $process = Start-Process -FilePath $Fixture -PassThru `
        -RedirectStandardOutput (Join-Path $scratch 'stdout') -RedirectStandardError (Join-Path $scratch 'stderr')
    $until = [DateTime]::UtcNow.AddSeconds(20)
    do {
        $apps = Invoke-Aad @('apps')
        $resource = @($apps.apps | Where-Object { $_.metadata.process_id -eq $process.Id } | Select-Object -ExpandProperty resource_id)
        if ($resource.Count -eq 1) { $resource = $resource[0]; break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $until)
    if (-not ($resource -is [string])) { throw 'Qt fixture was not discovered uniquely' }
    Invoke-Aad (@('do', 'fill', $resource) + $field + @('--value', 'unchanged')) | Out-Null
    Invoke-Aad (@('do', 'focus', $resource) + $field) | Out-Null
    $current = Invoke-InputMethod 'CurrentInputMethod'
    if ($current -notmatch "^\('([A-Za-z0-9_-]+)',\)$") { throw "Unrecognized Fcitx5 reply: $current" }
    $originalMethod = $Matches[1]
    Set-TestInputMethod $ComposingMethod
    $denied = Invoke-Aad (@('do', 'type-text', $resource) + $field + @('--text', 'nihao ')) -ExpectFailure
    $cause = $denied.error
    while ($cause.cause) { $cause = $cause.cause }
    if ($cause.code -ne 'DRIVER.INPUT_METHOD_ACTIVE' -or $cause.details.input_dispatched -ne $false) {
        throw "Wrong refusal: $($denied | ConvertTo-Json -Depth 12)"
    }
    Assert-Text 'unchanged'
    Write-Output 'PASS: composition refused before keys, original text unchanged'
    # Semantic fill remains usable even with the composing method selected.
    Invoke-Aad (@('do', 'fill', $resource) + $field + @('--value', 'semantic fill')) | Out-Null
    Assert-Text 'semantic fill'
    Set-TestInputMethod 'keyboard-us'
    Invoke-Aad (@('do', 'key', $resource) + $field + @('--keys', 'ctrl,a')) | Out-Null
    Invoke-Aad (@('do', 'type-text', $resource) + $field + @('--text', 'exact 123')) | Out-Null
    Assert-Text 'exact 123'
    Write-Output 'PASS: fill and direct keyboard input verified by fresh observations'
} finally {
    try {
        if ($originalMethod -and $process -and -not $process.HasExited) {
            Invoke-Aad (@('do', 'focus', $resource) + $field) | Out-Null
            Invoke-InputMethod 'SetCurrentIM' @($originalMethod) | Out-Null
        }
    } finally {
        if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id; $process.WaitForExit() }
        foreach ($key in $oldEnvironment.Keys) { [Environment]::SetEnvironmentVariable($key, $oldEnvironment[$key]) }
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
