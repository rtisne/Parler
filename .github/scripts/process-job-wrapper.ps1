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
$start = @{
    FilePath = [string]$payload.FilePath
    ArgumentList = [string]$payload.ArgumentList
    PassThru = $true
}
if ($payload.RedirectStandardOutput) {
    $start.RedirectStandardOutput = [string]$payload.RedirectStandardOutput
}
if ($payload.RedirectStandardError) {
    $start.RedirectStandardError = [string]$payload.RedirectStandardError
}

$child = Start-Process @start
$child.WaitForExit()
$child.Refresh()
exit $child.ExitCode
