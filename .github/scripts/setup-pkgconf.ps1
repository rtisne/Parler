param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")]
    [string]$Target,

    [string]$Destination = (Join-Path $PSScriptRoot "../../src-tauri/pkgconf")
)

$ErrorActionPreference = "Stop"
$Version = "3.0.3"

if ($Target -eq "aarch64-pc-windows-msvc") {
    $Architecture = "arm64"
    $MsiSha256 = "d222346eb52f90f867e0523229d840de25ea36c9f9be5f3e2f2629e05d3740c5"
} else {
    $Architecture = "x64"
    $MsiSha256 = "ff43c52a54f8e3cfb7d6716f4db0f88a233bfd27970f6ceb666a4f5dcbb43eb3"
}

$MsiName = "pkgconf-$Architecture-$Version.msi"
$MsiUrl = "https://github.com/pkgconf/pkgconf/releases/download/pkgconf-$Version/$MsiName"
$TempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$TempRoot = Join-Path $TempBase "parler-pkgconf-$Target"
$MsiPath = Join-Path $TempRoot $MsiName
$ExtractPath = Join-Path $TempRoot "extract"
$Destination = [System.IO.Path]::GetFullPath($Destination)
$DestinationParent = Split-Path -Parent $Destination
$DestinationName = Split-Path -Leaf $Destination
$CandidatePath = Join-Path $DestinationParent ".$DestinationName.new-$PID"
$BackupPath = Join-Path $DestinationParent ".$DestinationName.backup-$PID"

Remove-Item -Recurse -Force $TempRoot -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $CandidatePath -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $BackupPath -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $TempRoot, $ExtractPath, $DestinationParent, $CandidatePath | Out-Null

Write-Host "Downloading pkgconf $Version for $Target"
Invoke-WebRequest -Uri $MsiUrl -OutFile $MsiPath
$ActualSha256 = (Get-FileHash -Algorithm SHA256 $MsiPath).Hash.ToLowerInvariant()
if ($ActualSha256 -ne $MsiSha256) {
    throw "pkgconf MSI checksum mismatch: expected $MsiSha256, got $ActualSha256"
}

$MsiArguments = "/a `"$MsiPath`" /qn TARGETDIR=`"$ExtractPath`""
$MsiProcess = Start-Process -FilePath "msiexec.exe" -ArgumentList $MsiArguments -Wait -PassThru
if ($MsiProcess.ExitCode -ne 0) {
    throw "pkgconf administrative extraction failed with exit code $($MsiProcess.ExitCode)"
}

$PkgconfExecutable = Get-ChildItem $ExtractPath -Recurse -Filter "pkgconf.exe" -File | Select-Object -First 1
if (-not $PkgconfExecutable) {
    throw "pkgconf MSI layout changed: pkgconf.exe was not found"
}
Copy-Item $PkgconfExecutable.FullName (Join-Path $CandidatePath "pkgconf.exe")

$CandidateExecutable = Join-Path $CandidatePath "pkgconf.exe"
$InstalledVersion = (& $CandidateExecutable --version).Trim()
if ($LASTEXITCODE -ne 0 -or $InstalledVersion -ne $Version) {
    throw "pkgconf verification failed: expected $Version, got $InstalledVersion"
}

$BlasVersion = (& $CandidateExecutable --modversion blas).Trim()
if ($LASTEXITCODE -ne 0 -or $BlasVersion -ne "0.3.31") {
    throw "pkgconf could not resolve the pinned blas module: expected 0.3.31, got $BlasVersion"
}
$BlasLibraries = (& $CandidateExecutable --libs blas).Trim()
if ($LASTEXITCODE -ne 0 -or $BlasLibraries -notmatch "openblas") {
    throw "pkgconf did not resolve the OpenBLAS linker flags: $BlasLibraries"
}
$BlasCflags = (& $CandidateExecutable --cflags blas).Trim()
if ($LASTEXITCODE -ne 0 -or $BlasCflags -notmatch "include") {
    throw "pkgconf did not resolve the OpenBLAS include flags: $BlasCflags"
}

$HadExistingDestination = Test-Path $Destination
if ($HadExistingDestination) {
    Move-Item $Destination $BackupPath
}
try {
    Move-Item $CandidatePath $Destination
} catch {
    if ($HadExistingDestination -and (Test-Path $BackupPath)) {
        Move-Item $BackupPath $Destination
    }
    throw
}
if (Test-Path $BackupPath) {
    Remove-Item -Recurse -Force $BackupPath
}

$InstalledExecutable = Join-Path $Destination "pkgconf.exe"
$env:PATH = "$Destination$([System.IO.Path]::PathSeparator)$env:PATH"
if ($env:GITHUB_PATH) {
    Add-Content -Path $env:GITHUB_PATH -Value $Destination
}
Write-Host "pkgconf $InstalledVersion available at $InstalledExecutable"
