param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")]
    [string]$Target,

    [string]$Destination = (Join-Path $PSScriptRoot "../../src-tauri/openblas")
)

$ErrorActionPreference = "Stop"
$Version = "0.3.31"

if ($Target -eq "aarch64-pc-windows-msvc") {
    $ArchiveName = "OpenBLAS-$Version-woa64-dll.zip"
    $ArchiveSha256 = "a2f0dd10028c7ac799189f06e1e1d8163370c23d14d5a995ace85bd5fdb0d374"
    $SourceDirectory = "OpenBLAS-0331-dll"
} else {
    $ArchiveName = "OpenBLAS-$Version-x64.zip"
    $ArchiveSha256 = "e7595359700e8bb5a15c41af1920850b1be37078eb22813201b3d4bc5bd9227e"
    $SourceDirectory = "win64"
}

$ArchiveUrl = "https://github.com/OpenMathLib/OpenBLAS/releases/download/v$Version/$ArchiveName"
$TempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$TempRoot = Join-Path $TempBase "parler-openblas-$Target"
$ArchivePath = Join-Path $TempRoot $ArchiveName
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

Write-Host "Downloading OpenBLAS $Version for $Target"
Invoke-WebRequest -Uri $ArchiveUrl -OutFile $ArchivePath
$ActualSha256 = (Get-FileHash -Algorithm SHA256 $ArchivePath).Hash.ToLowerInvariant()
if ($ActualSha256 -ne $ArchiveSha256) {
    throw "OpenBLAS archive checksum mismatch: expected $ArchiveSha256, got $ActualSha256"
}

Expand-Archive -Path $ArchivePath -DestinationPath $ExtractPath -Force
$PackageRoot = Join-Path $ExtractPath $SourceDirectory
if (-not (Test-Path $PackageRoot -PathType Container)) {
    throw "OpenBLAS archive layout changed: missing $PackageRoot"
}
Get-ChildItem -Force $PackageRoot | Move-Item -Destination $CandidatePath

if ($Target -eq "aarch64-pc-windows-msvc") {
    $NestedInclude = Join-Path $CandidatePath "include/openblas"
    if (-not (Test-Path $NestedInclude -PathType Container)) {
        throw "OpenBLAS ARM64 archive layout changed: missing $NestedInclude"
    }
    Get-ChildItem -Force $NestedInclude | Move-Item -Destination (Join-Path $CandidatePath "include")
    Remove-Item -Force $NestedInclude

    $ImportLibrary = Join-Path $CandidatePath "lib/openblas.lib"
    if (-not (Test-Path $ImportLibrary -PathType Leaf)) {
        throw "OpenBLAS ARM64 import library is missing: $ImportLibrary"
    }
    Move-Item $ImportLibrary (Join-Path $CandidatePath "lib/libopenblas.lib")

    $RuntimeDll = Join-Path $CandidatePath "bin/openblas.dll"
    if (-not (Test-Path $RuntimeDll -PathType Leaf)) {
        throw "OpenBLAS ARM64 runtime is missing: $RuntimeDll"
    }
    Copy-Item $RuntimeDll (Join-Path $CandidatePath "bin/libopenblas.dll")
}

# whisper.cpp 1.7.3 uses CMake's Generic BLAS vendor on Windows. FindBLAS
# searches for blas.lib, while the final Rust link uses libopenblas.lib.
Copy-Item (Join-Path $CandidatePath "lib/libopenblas.lib") (Join-Path $CandidatePath "lib/blas.lib")
$BlasPkgConfig = @'
prefix=${pcfiledir}/../..
libdir=${prefix}/lib
includedir=${prefix}/include

Name: blas
Description: OpenBLAS compatibility module for whisper.cpp
Version: 0.3.31
Libs: -L${libdir} -lopenblas
Cflags: -I${includedir}
'@
Set-Content -Path (Join-Path $CandidatePath "lib/pkgconfig/blas.pc") -Value $BlasPkgConfig -Encoding utf8NoBOM

$RequiredFiles = @(
    (Join-Path $CandidatePath "include/cblas.h"),
    (Join-Path $CandidatePath "lib/libopenblas.lib"),
    (Join-Path $CandidatePath "lib/blas.lib"),
    (Join-Path $CandidatePath "lib/pkgconfig/blas.pc"),
    (Join-Path $CandidatePath "bin/libopenblas.dll")
)
foreach ($RequiredFile in $RequiredFiles) {
    if (-not (Test-Path $RequiredFile -PathType Leaf)) {
        throw "OpenBLAS setup is incomplete: missing $RequiredFile"
    }
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

$OpenBlasBin = Join-Path $Destination "bin"
$OpenBlasLib = Join-Path $Destination "lib"
$OpenBlasInclude = Join-Path $Destination "include"
$OpenBlasPkgConfig = Join-Path $OpenBlasLib "pkgconfig"
$CmakePrefixPath = "$Destination$(if ($env:CMAKE_PREFIX_PATH) { ";$env:CMAKE_PREFIX_PATH" })"
$CmakeLibraryPath = "$OpenBlasLib$(if ($env:CMAKE_LIBRARY_PATH) { ";$env:CMAKE_LIBRARY_PATH" })"
$CmakeIncludePath = "$OpenBlasInclude$(if ($env:CMAKE_INCLUDE_PATH) { ";$env:CMAKE_INCLUDE_PATH" })"
$PkgConfigPath = "$OpenBlasPkgConfig$(if ($env:PKG_CONFIG_PATH) { ";$env:PKG_CONFIG_PATH" })"

# Persist in the current PowerShell process for local Windows builds.
$env:OPENBLAS_PATH = $Destination
$env:CMAKE_PREFIX_PATH = $CmakePrefixPath
$env:CMAKE_LIBRARY_PATH = $CmakeLibraryPath
$env:CMAKE_INCLUDE_PATH = $CmakeIncludePath
$env:PKG_CONFIG_PATH = $PkgConfigPath
$env:PATH = "$OpenBlasBin$([System.IO.Path]::PathSeparator)$env:PATH"

# Persist across GitHub Actions steps when running in CI.
if ($env:GITHUB_ENV) {
    Add-Content -Path $env:GITHUB_ENV -Value "OPENBLAS_PATH=$Destination"
    Add-Content -Path $env:GITHUB_ENV -Value "CMAKE_PREFIX_PATH=$CmakePrefixPath"
    Add-Content -Path $env:GITHUB_ENV -Value "CMAKE_LIBRARY_PATH=$CmakeLibraryPath"
    Add-Content -Path $env:GITHUB_ENV -Value "CMAKE_INCLUDE_PATH=$CmakeIncludePath"
    Add-Content -Path $env:GITHUB_ENV -Value "PKG_CONFIG_PATH=$PkgConfigPath"
}
if ($env:GITHUB_PATH) {
    Add-Content -Path $env:GITHUB_PATH -Value $OpenBlasBin
}

Write-Host "OPENBLAS_PATH=$Destination"
Get-ChildItem (Join-Path $Destination "bin") -Filter "*.dll" | ForEach-Object {
    Write-Host "$($_.Name) $($_.Length) bytes"
}
