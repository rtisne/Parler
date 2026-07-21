param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath,

    [int]$StartupSeconds = 15
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $BinaryPath -PathType Leaf)) {
    throw "Built Parler executable not found at the expected path: $BinaryPath"
}
$executable = (Resolve-Path $BinaryPath).Path

$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$stdoutPath = Join-Path $tempRoot "parler-startup-stdout.log"
$stderrPath = Join-Path $tempRoot "parler-startup-stderr.log"
Remove-Item $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue

Write-Host "Starting $executable for a $StartupSeconds second startup smoke test"
$process = $null
try {
    $process = Start-Process `
        -FilePath $executable `
        -ArgumentList "--no-tray" `
        -RedirectStandardOutput $stdoutPath `
        -RedirectStandardError $stderrPath `
        -PassThru

    Start-Sleep -Seconds $StartupSeconds
    $process.Refresh()

    $stdout = if (Test-Path $stdoutPath) { Get-Content $stdoutPath -Raw } else { "" }
    $stderr = if (Test-Path $stderrPath) { Get-Content $stderrPath -Raw } else { "" }

    if ($process.HasExited) {
        Write-Host "--- parler stdout ---"
        Write-Host $stdout
        Write-Host "--- parler stderr ---"
        Write-Host $stderr
        throw "Parler exited during startup smoke test with code $($process.ExitCode)"
    }

    if ($stderr -match "panicked at|PluginInitialization|error while building tauri application") {
        Write-Host "--- parler stderr ---"
        Write-Host $stderr
        throw "Parler emitted a startup panic during smoke test"
    }

    Write-Host "Parler remained alive for $StartupSeconds seconds without a startup panic"
}
finally {
    if ($process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id -Force
        $process.WaitForExit()
    }
}
