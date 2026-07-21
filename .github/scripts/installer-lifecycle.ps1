param(
    [Parameter(Mandatory = $false)]
    [ValidateSet("msi", "nsis")]
    [string]$InstallerType,

    [string]$BundleDir,
    [string]$ProductName = "Parler",
    [string]$BinaryName = "parler.exe",
    [int]$LaunchSeconds = 15,
    [int]$UninstallTimeoutSeconds = 120
)

$ErrorActionPreference = "Stop"

# Uninstall metadata for a per-machine MSI lands in the HKLM hives; a per-user
# NSIS install lands in the HKCU hives. Scan all four (including WOW6432Node) so
# the entry is found regardless of installer type / bitness.
$script:UninstallHives = @(
    "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*",
    "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*",
    "HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*",
    "HKCU:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*"
)

function Get-DiagnosticsDir {
    $base = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
    $dir = Join-Path $base "installer-lifecycle"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    return $dir
}

function Get-InstallerFile {
    param(
        [Parameter(Mandatory = $true)][string]$BundleDir,
        [Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType
    )

    $pattern = if ($InstallerType -eq "msi") {
        Join-Path $BundleDir "msi/*.msi"
    } else {
        Join-Path $BundleDir "nsis/*-setup.exe"
    }

    $files = @(Get-ChildItem -Path $pattern -File -ErrorAction SilentlyContinue)
    if ($files.Count -ne 1) {
        throw "Expected exactly one $InstallerType installer matching '$pattern', found $($files.Count)."
    }
    return $files[0].FullName
}

function Get-UninstallEntries {
    $entries = foreach ($hive in $script:UninstallHives) {
        Get-ItemProperty -Path $hive -ErrorAction SilentlyContinue
    }
    return @($entries)
}

function Select-UninstallEntry {
    param(
        [Parameter(Mandatory = $true)]$Entries,
        [Parameter(Mandatory = $true)][string]$ProductName
    )

    $matched = @($Entries | Where-Object { $_.DisplayName -eq $ProductName })
    if ($matched.Count -eq 0) {
        throw "No uninstall entry with DisplayName '$ProductName' found across the scanned hives:`n  $($script:UninstallHives -join "`n  ")"
    }
    if ($matched.Count -gt 1) {
        throw "Ambiguous uninstall metadata: $($matched.Count) entries named '$ProductName'."
    }
    return $matched[0]
}

function Resolve-InstalledExecutable {
    param(
        [Parameter(Mandatory = $true)]$Entry,
        [Parameter(Mandatory = $true)][string]$BinaryName
    )

    $installLocation = $Entry.InstallLocation
    if ($installLocation -and (Test-Path $installLocation)) {
        $hits = @(Get-ChildItem -Path $installLocation -Recurse -Filter $BinaryName -File -ErrorAction SilentlyContinue)
        if ($hits.Count -eq 1) {
            return $hits[0].FullName
        }
        if ($hits.Count -gt 1) {
            throw "Multiple '$BinaryName' found under InstallLocation '$installLocation'."
        }
    }

    # Fall back to DisplayIcon (commonly "C:\...\parler.exe,0").
    $icon = $Entry.DisplayIcon
    if ($icon) {
        $iconPath = $icon.Trim('"')
        $iconPath = ($iconPath -replace ',\d+$', '').Trim('"')
        if (Test-Path $iconPath -PathType Leaf) {
            return (Resolve-Path $iconPath).Path
        }
    }

    throw "Could not resolve installed '$BinaryName' (InstallLocation='$installLocation', DisplayIcon='$($Entry.DisplayIcon)')."
}

function Split-CommandLine {
    param([Parameter(Mandatory = $true)][string]$CommandLine)

    $trimmed = $CommandLine.Trim()
    if ($trimmed.StartsWith('"')) {
        $end = $trimmed.IndexOf('"', 1)
        $file = $trimmed.Substring(1, $end - 1)
        $arguments = $trimmed.Substring($end + 1).Trim()
    } else {
        $spaceIndex = $trimmed.IndexOf(' ')
        if ($spaceIndex -lt 0) {
            $file = $trimmed
            $arguments = ""
        } else {
            $file = $trimmed.Substring(0, $spaceIndex)
            $arguments = $trimmed.Substring($spaceIndex + 1).Trim()
        }
    }
    return @{ File = $file; Args = $arguments }
}

function Get-UninstallCommand {
    param(
        [Parameter(Mandatory = $true)]$Entry,
        [Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType,
        [Parameter(Mandatory = $true)][string]$LogDir
    )

    if ($InstallerType -eq "msi") {
        # For an MSI, PSChildName is the ProductCode GUID.
        $log = Join-Path $LogDir "msi-uninstall.log"
        return @{
            File = "msiexec.exe"
            Args = "/x $($Entry.PSChildName) /qn /norestart /l*v `"$log`""
        }
    }

    $command = if ($Entry.QuietUninstallString) { $Entry.QuietUninstallString } else { $Entry.UninstallString }
    if (-not $command) {
        throw "NSIS uninstall entry has neither QuietUninstallString nor UninstallString."
    }
    $parsed = Split-CommandLine -CommandLine $command
    $arguments = $parsed.Args
    if ($arguments -notmatch '(^|\s)/S(\s|$)') {
        $arguments = ($arguments + " /S").Trim()
    }
    return @{ File = $parsed.File; Args = $arguments }
}

function Assert-UninstallResidue {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [Parameter(Mandatory = $true)][string]$BinaryName
    )

    if (-not $InstallDir -or -not (Test-Path $InstallDir)) {
        Write-Host "Install directory removed: $InstallDir"
        return
    }

    $residue = @(
        Get-ChildItem -Path $InstallDir -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -eq $BinaryName -or $_.Extension -eq ".dll" }
    )
    if ($residue.Count -gt 0) {
        throw "Uninstall left executable residue: $(($residue | ForEach-Object { $_.FullName }) -join ', ')"
    }
    Write-Host "Install directory present but free of executable payload: $InstallDir"
    Get-ChildItem -Path $InstallDir -Recurse -File -ErrorAction SilentlyContinue |
        ForEach-Object { Write-Host "  leftover: $($_.FullName)" }
}

function Install-Installer {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType,
        [Parameter(Mandatory = $true)][string]$LogDir,
        [Parameter(Mandatory = $true)][string]$ProductName
    )

    if ($InstallerType -eq "msi") {
        $log = Join-Path $LogDir "msi-install.log"
        $arguments = "/i `"$Path`" /qn /norestart /l*v `"$log`""
        $process = Start-Process -FilePath "msiexec.exe" -ArgumentList $arguments -Wait -PassThru
        if ($process.ExitCode -ne 0) {
            throw "MSI install failed with exit code $($process.ExitCode). See $log"
        }
        return
    }

    # NSIS silent install uses uppercase /S; it can return before the uninstall
    # registry entry is fully written, so poll briefly for it to appear.
    $process = Start-Process -FilePath $Path -ArgumentList "/S" -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw "NSIS install failed with exit code $($process.ExitCode)."
    }
    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        $found = @(Get-UninstallEntries | Where-Object { $_.DisplayName -eq $ProductName })
        if ($found.Count -ge 1) { return }
        Start-Sleep -Seconds 3
    }
    throw "NSIS install completed but no '$ProductName' uninstall entry appeared within 60 seconds."
}

function Invoke-LaunchGate {
    param(
        [Parameter(Mandatory = $true)][string]$ExePath,
        [Parameter(Mandatory = $true)][int]$LaunchSeconds,
        [Parameter(Mandatory = $true)][string]$LogDir,
        [string]$BinaryName = "parler.exe"
    )

    $stdoutPath = Join-Path $LogDir "parler-launch-stdout.log"
    $stderrPath = Join-Path $LogDir "parler-launch-stderr.log"
    Remove-Item $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue

    Write-Host "Launching installed $ExePath for a $LaunchSeconds second survival check"
    $process = $null
    try {
        $process = Start-Process `
            -FilePath $ExePath `
            -ArgumentList "--no-tray" `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath `
            -PassThru

        Start-Sleep -Seconds $LaunchSeconds
        $process.Refresh()

        $stdout = if (Test-Path $stdoutPath) { Get-Content $stdoutPath -Raw } else { "" }
        $stderr = if (Test-Path $stderrPath) { Get-Content $stderrPath -Raw } else { "" }

        if ($process.HasExited) {
            Write-Host "--- parler stdout ---"
            Write-Host $stdout
            Write-Host "--- parler stderr ---"
            Write-Host $stderr
            throw "Installed Parler exited during launch gate with code $($process.ExitCode)"
        }

        if ($stderr -match "panicked at|PluginInitialization|error while building tauri application") {
            Write-Host "--- parler stderr ---"
            Write-Host $stderr
            throw "Installed Parler emitted a startup panic during the launch gate"
        }

        Write-Host "Installed Parler remained alive for $LaunchSeconds seconds without a startup panic"
    }
    finally {
        if ($process -and -not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
            $process.WaitForExit()
        }
    }

    $lingering = @(Get-Process -Name ([System.IO.Path]::GetFileNameWithoutExtension($BinaryName)) -ErrorAction SilentlyContinue)
    if ($lingering.Count -gt 0) {
        throw "Parler process survived the launch gate kill; $($lingering.Count) instance(s) still running."
    }
}

function Invoke-Uninstall {
    param(
        [Parameter(Mandatory = $true)]$Command,
        [Parameter(Mandatory = $true)][string]$ProductName,
        [Parameter(Mandatory = $true)][string]$ExePath,
        [Parameter(Mandatory = $true)][int]$TimeoutSeconds
    )

    $process = Start-Process -FilePath $Command.File -ArgumentList $Command.Args -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw "Uninstall command '$($Command.File) $($Command.Args)' failed with exit code $($process.ExitCode)."
    }

    # NSIS uninstallers copy themselves to %TEMP% and return before deletion
    # finishes, so poll until BOTH the registry entry and the exe are gone.
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $registryGone = @(Get-UninstallEntries | Where-Object { $_.DisplayName -eq $ProductName }).Count -eq 0
        $exeGone = -not (Test-Path $ExePath -PathType Leaf)
        if ($registryGone -and $exeGone) {
            Write-Host "Uninstall complete: registry entry and executable removed."
            return
        }
        Start-Sleep -Seconds 5
    } while ((Get-Date) -lt $deadline)

    throw "Uninstall did not complete within $TimeoutSeconds seconds (registryGone=$registryGone, exeGone=$exeGone)."
}

# --- Entry point ---------------------------------------------------------
# Guarded so the Pester spec can dot-source this file for pure-function tests.
if ($env:PARLER_LIFECYCLE_NO_RUN -eq "1") { return }

if (-not $InstallerType) { throw "-InstallerType is required when running the lifecycle sequence." }
if (-not $BundleDir) { throw "-BundleDir is required when running the lifecycle sequence." }

$logDir = Get-DiagnosticsDir
try {
    $installer = Get-InstallerFile -BundleDir $BundleDir -InstallerType $InstallerType
    Write-Host "Resolved $InstallerType installer: $installer"

    Install-Installer -Path $installer -InstallerType $InstallerType -LogDir $logDir -ProductName $ProductName

    $entry = Select-UninstallEntry -Entries (Get-UninstallEntries) -ProductName $ProductName
    $exePath = Resolve-InstalledExecutable -Entry $entry -BinaryName $BinaryName
    Write-Host "Installed executable: $exePath"
    $installDir = Split-Path -Parent $exePath

    Invoke-LaunchGate -ExePath $exePath -LaunchSeconds $LaunchSeconds -LogDir $logDir -BinaryName $BinaryName

    $command = Get-UninstallCommand -Entry $entry -InstallerType $InstallerType -LogDir $logDir
    Invoke-Uninstall -Command $command -ProductName $ProductName -ExePath $exePath -TimeoutSeconds $UninstallTimeoutSeconds

    Assert-UninstallResidue -InstallDir $installDir -BinaryName $BinaryName

    Write-Host "✅ $InstallerType installer lifecycle passed"
}
catch {
    Write-Host "❌ $InstallerType installer lifecycle failed: $($_.Exception.Message)"
    throw
}
