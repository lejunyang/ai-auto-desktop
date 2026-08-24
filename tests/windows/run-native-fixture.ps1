param(
    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string] $OutputPath = "artifacts/windows-native-fixture-result.json"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
# A failing unittest command must still reach the report-writing finally block.
$PSNativeCommandUseErrorActionPreference = $false

function Get-ValueOrUnknown {
    param([AllowNull()][AllowEmptyString()][string] $Value)

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return "unknown"
    }
    return $Value
}

function Get-UtcTimestamp {
    return [DateTimeOffset]::UtcNow.ToString(
        "yyyy-MM-ddTHH:mm:ss.fffZ",
        [Globalization.CultureInfo]::InvariantCulture
    )
}

function Get-CommitSha {
    $workflowSha = Get-ValueOrUnknown $env:GITHUB_SHA
    if ($workflowSha -ne "unknown") {
        return $workflowSha
    }

    try {
        $gitSha = (& git rev-parse HEAD 2>$null | Out-String).Trim()
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($gitSha)) {
            return $gitSha
        }
    }
    catch {
        # Local source archives may not have git. The report remains usable.
    }
    return "unknown"
}

function Get-PythonVersion {
    try {
        $versionOutput = (& python --version 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($versionOutput)) {
            return $versionOutput
        }
    }
    catch {
        # The test invocation below records the command failure as an error.
    }
    return "unavailable"
}

$startedAt = Get-UtcTimestamp
$testCommand = "python -m unittest discover -s tests -v"
$testArguments = @("-m", "unittest", "discover", "-s", "tests", "-v")
$testResult = [ordered]@{
    command = $testCommand
    result = "not_run"
    exit_code = $null
    error_type = $null
}
$status = "error"
$scriptExitCode = 1

$runnerInfo = [ordered]@{
    name = Get-ValueOrUnknown $env:RUNNER_NAME
    os = Get-ValueOrUnknown $env:RUNNER_OS
    architecture = Get-ValueOrUnknown $env:RUNNER_ARCH
    image_os = Get-ValueOrUnknown $env:ImageOS
    image_version = Get-ValueOrUnknown $env:ImageVersion
}
$osInfo = [ordered]@{
    description = [Runtime.InteropServices.RuntimeInformation]::OSDescription
    version = [Environment]::OSVersion.VersionString
    architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
}
$pythonInfo = [ordered]@{
    command = "python"
    version = Get-PythonVersion
}

try {
    if (-not [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [Runtime.InteropServices.OSPlatform]::Windows
    )) {
        throw "The native UIA fixture runner requires Windows."
    }

    Write-Host "Running: $testCommand"
    & python @testArguments
    if ($null -eq $LASTEXITCODE) {
        $testExitCode = 1
    }
    else {
        $testExitCode = [int] $LASTEXITCODE
    }

    $testResult.exit_code = $testExitCode
    $scriptExitCode = $testExitCode
    if ($testExitCode -eq 0) {
        $testResult.result = "passed"
        $status = "passed"
    }
    else {
        $testResult.result = "failed"
        $status = "failed"
    }
}
catch {
    $testResult.result = "error"
    $testResult.error_type = $_.Exception.GetType().FullName
    $status = "error"
    $scriptExitCode = 1
    Write-Warning "Windows native fixture runner error: $($_.Exception.Message)"
}
finally {
    # Keep this report allowlist-only. Never serialize the environment, event
    # payload, command output, or credentials into the uploaded artifact.
    $report = [ordered]@{
        schema_version = 1
        commit_sha = Get-CommitSha
        runner = $runnerInfo
        os = $osInfo
        python = $pythonInfo
        test = $testResult
        started_at = $startedAt
        timestamp = Get-UtcTimestamp
        status = $status
    }

    $absoluteOutputPath = [IO.Path]::GetFullPath($OutputPath)
    $outputDirectory = [IO.Path]::GetDirectoryName($absoluteOutputPath)
    [IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
    $json = $report | ConvertTo-Json -Depth 6
    [IO.File]::WriteAllText(
        $absoluteOutputPath,
        $json + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false)
    )
    Write-Host "Windows native fixture report: $absoluteOutputPath"
}

exit $scriptExitCode
