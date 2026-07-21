param(
    [Parameter(Mandatory = $false)]
    [ValidateSet("msi", "nsis")]
    [string]$InstallerType,

    [string]$BundleDir,
    [string]$ProductName = "Parler",
    [string]$BinaryName = "parler.exe",
    [int]$LaunchSeconds = 15,
    [int]$InstallTimeoutSeconds = 120,
    [int]$UninstallTimeoutSeconds = 120,
    [long]$MaxLaunchLogBytes = 10485760
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
    param([Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType)

    $base = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
    $dir = Join-Path (Join-Path $base "installer-lifecycle") $InstallerType
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    return $dir
}

function Test-MsiProductCode {
    param([string]$ProductCode)
    $guid = [guid]::Empty
    return [guid]::TryParse($ProductCode, [ref]$guid)
}

function Stop-ProcessTree {
    param([Parameter(Mandatory = $true)]$Process)

    try { $Process.Refresh() } catch { }
    if ($Process.HasExited) { return }
    try {
        $Process.Kill($true)
    } catch {
        Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
    }
    try { $Process.Refresh() } catch { }
    if (-not $Process.HasExited -and -not $Process.WaitForExit(5000)) {
        throw "Process tree $($Process.Id) did not terminate within 5 seconds after kill."
    }
}

function Invoke-BoundedProcess {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string]$ArgumentList = "",
        [Parameter(Mandatory = $true)][ValidateRange(1, 3600)][int]$TimeoutSeconds,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $process = Start-Process -FilePath $FilePath -ArgumentList $ArgumentList -PassThru
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        Stop-ProcessTree -Process $process
        throw "$Description timed out after $TimeoutSeconds seconds and was terminated."
    }
    return $process
}

function Stop-ProductProcesses {
    param([Parameter(Mandatory = $true)][string]$BinaryName)

    $processName = [System.IO.Path]::GetFileNameWithoutExtension($BinaryName)
    foreach ($candidate in @(Get-Process -Name $processName -ErrorAction SilentlyContinue)) {
        Stop-ProcessTree -Process $candidate
    }
    $lingering = @(Get-Process -Name $processName -ErrorAction SilentlyContinue)
    if ($lingering.Count -gt 0) {
        throw "$BinaryName process cleanup failed; $($lingering.Count) instance(s) still running."
    }
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
        [Parameter(Mandatory = $true)][string]$ProductName,
        [Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType
    )

    $matched = @($Entries | Where-Object {
        if ($_.DisplayName -ne $ProductName) { return $false }
        if ($InstallerType -eq "msi") {
            return ([int]$_.WindowsInstaller -eq 1) -and (Test-MsiProductCode -ProductCode ([string]$_.PSChildName))
        }
        return [int]$_.WindowsInstaller -ne 1
    })
    if ($matched.Count -eq 0) {
        throw "No typed $InstallerType uninstall entry with DisplayName '$ProductName' found across the scanned hives:`n  $($script:UninstallHives -join "`n  ")"
    }
    if ($matched.Count -gt 1) {
        throw "Ambiguous $InstallerType uninstall metadata: $($matched.Count) entries named '$ProductName'."
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
        if (
            (Test-Path $iconPath -PathType Leaf) -and
            ([System.IO.Path]::GetFileName($iconPath) -ieq $BinaryName)
        ) {
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
        if ([int]$Entry.WindowsInstaller -ne 1 -or -not (Test-MsiProductCode -ProductCode ([string]$Entry.PSChildName))) {
            throw "MSI uninstall metadata does not contain a valid Windows Installer ProductCode GUID."
        }
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

    $residue = @(Get-ChildItem -Path $InstallDir -Recurse -Force -ErrorAction SilentlyContinue)
    if ($residue.Count -gt 0) {
        throw "Uninstall left residue under '$InstallDir': $(($residue | ForEach-Object { $_.FullName }) -join ', ')"
    }
    Write-Host "Install directory is empty after uninstall: $InstallDir"
}

function Install-Installer {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType,
        [Parameter(Mandatory = $true)][string]$LogDir,
        [Parameter(Mandatory = $true)][string]$ProductName,
        [Parameter(Mandatory = $true)][ValidateRange(1, 3600)][int]$TimeoutSeconds
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    if ($InstallerType -eq "msi") {
        $log = Join-Path $LogDir "msi-install.log"
        $arguments = "/i `"$Path`" /qn /norestart /l*v `"$log`""
        $process = Invoke-BoundedProcess -FilePath "msiexec.exe" -ArgumentList $arguments -TimeoutSeconds $TimeoutSeconds -Description "MSI install"
        if ($process.ExitCode -ne 0) {
            throw "MSI install failed with exit code $($process.ExitCode). See $log"
        }
        return
    }

    $process = Invoke-BoundedProcess -FilePath $Path -ArgumentList "/S" -TimeoutSeconds $TimeoutSeconds -Description "NSIS install"
    if ($process.ExitCode -ne 0) {
        throw "NSIS install failed with exit code $($process.ExitCode)."
    }
    while ((Get-Date) -lt $deadline) {
        $found = @(Get-UninstallEntries | Where-Object { $_.DisplayName -eq $ProductName })
        if ($found.Count -ge 1) { return }
        Start-Sleep -Seconds 1
    }
    throw "NSIS install completed but no '$ProductName' uninstall entry appeared within $TimeoutSeconds seconds."
}

function Invoke-LaunchGate {
    param(
        [Parameter(Mandatory = $true)][string]$ExePath,
        [Parameter(Mandatory = $true)][int]$LaunchSeconds,
        [Parameter(Mandatory = $true)][string]$LogDir,
        [string]$BinaryName = "parler.exe",
        [long]$MaxLogBytes = 10485760
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

        $deadline = (Get-Date).AddSeconds($LaunchSeconds)
        while ((Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 250
            $process.Refresh()
            foreach ($path in @($stdoutPath, $stderrPath)) {
                if ((Test-Path $path -PathType Leaf) -and (Get-Item $path).Length -gt $MaxLogBytes) {
                    throw "Installed Parler launch log exceeded the $MaxLogBytes byte safety limit: $path"
                }
            }
            if ($process.HasExited) { break }
        }

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
        if ($process) { Stop-ProcessTree -Process $process }
    }
}

function Invoke-Uninstall {
    param(
        [Parameter(Mandatory = $true)]$Command,
        [Parameter(Mandatory = $true)][string]$ProductName,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$ExePath,
        [Parameter(Mandatory = $true)][ValidateRange(1, 3600)][int]$TimeoutSeconds
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $process = Invoke-BoundedProcess -FilePath $Command.File -ArgumentList $Command.Args -TimeoutSeconds $TimeoutSeconds -Description "Uninstall command"
    if ($process.ExitCode -ne 0) {
        throw "Uninstall command '$($Command.File) $($Command.Args)' failed with exit code $($process.ExitCode)."
    }

    do {
        $registryGone = @(Get-UninstallEntries | Where-Object { $_.DisplayName -eq $ProductName }).Count -eq 0
        $exeGone = (-not $ExePath) -or (-not (Test-Path $ExePath -PathType Leaf))
        if ($registryGone -and $exeGone) {
            Write-Host "Uninstall complete: registry entry and executable removed."
            return
        }
        Start-Sleep -Seconds 1
    } while ((Get-Date) -lt $deadline)

    $uninstallerName = [System.IO.Path]::GetFileName([string]$Command.File)
    try {
        Stop-ProductProcesses -BinaryName $uninstallerName
    } catch {
        throw "Uninstall did not complete within $TimeoutSeconds seconds (registryGone=$registryGone, exeGone=$exeGone); detached uninstaller cleanup also failed: $($_.Exception.Message)"
    }
    throw "Uninstall did not complete within $TimeoutSeconds seconds (registryGone=$registryGone, exeGone=$exeGone)."
}

function Invoke-InstallerLifecycle {
    param(
        [Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType,
        [Parameter(Mandatory = $true)][string]$BundleDir,
        [Parameter(Mandatory = $true)][string]$ProductName,
        [Parameter(Mandatory = $true)][string]$BinaryName,
        [Parameter(Mandatory = $true)][int]$LaunchSeconds,
        [Parameter(Mandatory = $true)][int]$InstallTimeoutSeconds,
        [Parameter(Mandatory = $true)][int]$UninstallTimeoutSeconds,
        [long]$MaxLaunchLogBytes = 10485760
    )

    $logDir = Get-DiagnosticsDir -InstallerType $InstallerType
    $errors = [System.Collections.Generic.List[string]]::new()
    $entry = $null
    $exePath = ""
    $installDir = ""
    $command = $null

    try {
        $installer = Get-InstallerFile -BundleDir $BundleDir -InstallerType $InstallerType
        Write-Host "Resolved $InstallerType installer: $installer"
        Install-Installer -Path $installer -InstallerType $InstallerType -LogDir $logDir -ProductName $ProductName -TimeoutSeconds $InstallTimeoutSeconds

        $entry = Select-UninstallEntry -Entries (Get-UninstallEntries) -ProductName $ProductName -InstallerType $InstallerType
        $exePath = Resolve-InstalledExecutable -Entry $entry -BinaryName $BinaryName
        $installDir = Split-Path -Parent $exePath
        Write-Host "Installed executable: $exePath"
        $command = Get-UninstallCommand -Entry $entry -InstallerType $InstallerType -LogDir $logDir

        Invoke-LaunchGate -ExePath $exePath -LaunchSeconds $LaunchSeconds -LogDir $logDir -BinaryName $BinaryName -MaxLogBytes $MaxLaunchLogBytes
    }
    catch {
        $errors.Add($_.Exception.Message)
    }
    finally {
        try {
            Stop-ProductProcesses -BinaryName $BinaryName
        } catch {
            $errors.Add("Process cleanup failed: $($_.Exception.Message)")
        }

        if (-not $entry) {
            try {
                $entry = Select-UninstallEntry -Entries (Get-UninstallEntries) -ProductName $ProductName -InstallerType $InstallerType
            } catch { }
        }

        if ($entry) {
            if (-not $installDir -and $entry.InstallLocation) {
                $installDir = [string]$entry.InstallLocation
            }
            try {
                if (-not $command) {
                    $command = Get-UninstallCommand -Entry $entry -InstallerType $InstallerType -LogDir $logDir
                }
                Invoke-Uninstall -Command $command -ProductName $ProductName -ExePath $exePath -TimeoutSeconds $UninstallTimeoutSeconds
            } catch {
                $errors.Add("Uninstall cleanup failed: $($_.Exception.Message)")
            }
            if ($installDir) {
                try {
                    Assert-UninstallResidue -InstallDir $installDir -BinaryName $BinaryName
                } catch {
                    $errors.Add("Residue cleanup failed: $($_.Exception.Message)")
                }
            }
        }
    }

    if ($errors.Count -gt 0) {
        throw ($errors -join " | ")
    }
    Write-Host "✅ $InstallerType installer lifecycle passed"
}

# --- Entry point ---------------------------------------------------------
# Guarded so the Pester spec can dot-source this file for pure-function tests.
if ($env:PARLER_LIFECYCLE_NO_RUN -eq "1") { return }

if (-not $InstallerType) { throw "-InstallerType is required when running the lifecycle sequence." }
if (-not $BundleDir) { throw "-BundleDir is required when running the lifecycle sequence." }

try {
    Invoke-InstallerLifecycle `
        -InstallerType $InstallerType `
        -BundleDir $BundleDir `
        -ProductName $ProductName `
        -BinaryName $BinaryName `
        -LaunchSeconds $LaunchSeconds `
        -InstallTimeoutSeconds $InstallTimeoutSeconds `
        -UninstallTimeoutSeconds $UninstallTimeoutSeconds `
        -MaxLaunchLogBytes $MaxLaunchLogBytes
}
catch {
    Write-Host "❌ $InstallerType installer lifecycle failed: $($_.Exception.Message)"
    throw
}
