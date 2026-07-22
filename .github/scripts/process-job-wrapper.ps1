param(
    [Parameter(Mandatory = $true)][string]$PayloadPath,
    [Parameter(Mandatory = $true)][string]$ReadyPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$deadline = (Get-Date).AddSeconds(30)
while (-not (Test-Path -LiteralPath $ReadyPath -PathType Leaf)) {
    if ((Get-Date) -ge $deadline) {
        throw "Timed out waiting for the Job Object assignment signal."
    }
    Start-Sleep -Milliseconds 10
}

$payload = Get-Content -LiteralPath $PayloadPath -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = [string]$payload.FilePath
$startInfo.UseShellExecute = $false
foreach ($argument in @($payload.ArgumentList)) {
    $startInfo.ArgumentList.Add([string]$argument)
}

$stdoutStream = $null
$stderrStream = $null
$child = $null
$copyTasks = [System.Collections.Generic.List[System.Threading.Tasks.Task]]::new()
try {
    if ($payload.RedirectStandardOutput) {
        $startInfo.RedirectStandardOutput = $true
        $stdoutStream = [System.IO.File]::Open(
            [string]$payload.RedirectStandardOutput,
            [System.IO.FileMode]::Create,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::Read
        )
    }
    if ($payload.RedirectStandardError) {
        $startInfo.RedirectStandardError = $true
        $stderrStream = [System.IO.File]::Open(
            [string]$payload.RedirectStandardError,
            [System.IO.FileMode]::Create,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::Read
        )
    }

    $child = [System.Diagnostics.Process]::Start($startInfo)
    if ($stdoutStream) { $copyTasks.Add($child.StandardOutput.BaseStream.CopyToAsync($stdoutStream)) }
    if ($stderrStream) { $copyTasks.Add($child.StandardError.BaseStream.CopyToAsync($stderrStream)) }

    $child.WaitForExit()
    if ($copyTasks.Count -gt 0) {
        [System.Threading.Tasks.Task]::WhenAll($copyTasks).GetAwaiter().GetResult()
    }
    $child.Refresh()
    $exitCode = $child.ExitCode
} finally {
    if ($child) { $child.Dispose() }
    if ($stdoutStream) { $stdoutStream.Dispose() }
    if ($stderrStream) { $stderrStream.Dispose() }
}

exit $exitCode
