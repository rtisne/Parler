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

function Initialize-ProcessJobApi {
    if (-not $IsWindows -or ("ParlerProcessJob" -as [type])) { return }

    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;

public static class ParlerProcessJob
{
    [StructLayout(LayoutKind.Sequential)]
    private struct IO_COUNTERS
    {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
        public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass, SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(IntPtr job, int infoClass, IntPtr info, uint length);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateJobObject(IntPtr job, uint exitCode);
    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr handle);

    public static IntPtr CreateKillOnClose()
    {
        IntPtr job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
        info.BasicLimitInformation.LimitFlags = 0x00002000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        int size = Marshal.SizeOf(info);
        IntPtr pointer = Marshal.AllocHGlobal(size);
        try
        {
            Marshal.StructureToPtr(info, pointer, false);
            if (!SetInformationJobObject(job, 9, pointer, (uint)size))
                throw new Win32Exception(Marshal.GetLastWin32Error());
            return job;
        }
        catch
        {
            CloseHandle(job);
            throw;
        }
        finally { Marshal.FreeHGlobal(pointer); }
    }

    public static void Assign(IntPtr job, int processId)
    {
        using (Process process = Process.GetProcessById(processId))
        {
            if (!AssignProcessToJobObject(job, process.Handle))
                throw new Win32Exception(Marshal.GetLastWin32Error());
        }
    }

    public static void TerminateAndClose(IntPtr job)
    {
        try
        {
            if (!TerminateJobObject(job, 1))
                throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        finally { CloseHandle(job); }
    }
}
'@
}

function New-ProcessJobHandle {
    Initialize-ProcessJobApi
    return [ParlerProcessJob]::CreateKillOnClose()
}

function Add-ProcessToJob {
    param(
        [Parameter(Mandatory = $true)][IntPtr]$JobHandle,
        [Parameter(Mandatory = $true)][int]$ProcessId
    )
    [ParlerProcessJob]::Assign($JobHandle, $ProcessId)
}

function Stop-NativeProcessJob {
    param([Parameter(Mandatory = $true)][IntPtr]$JobHandle)
    [ParlerProcessJob]::TerminateAndClose($JobHandle)
}

function Register-ProcessJob {
    param(
        [Parameter(Mandatory = $true)]$Process,
        [switch]$ForceWindowsSemantics
    )

    if (-not $IsWindows -and -not $ForceWindowsSemantics) { return $Process }
    $job = New-ProcessJobHandle
    try {
        Add-ProcessToJob -JobHandle $job -ProcessId ([int]$Process.Id)
        $Process | Add-Member -MemberType NoteProperty -Name ParlerJobHandle -Value $job -Force
        return $Process
    } catch {
        $assignmentError = $_.Exception.Message
        $cleanupErrors = [System.Collections.Generic.List[string]]::new()
        try { Stop-NativeProcessJob -JobHandle $job } catch { $cleanupErrors.Add("job cleanup failed: $($_.Exception.Message)") }

        $hasExited = $false
        try {
            $Process.Refresh()
            $hasExited = [bool]$Process.HasExited
        } catch {
            $cleanupErrors.Add("process state refresh failed: $($_.Exception.Message)")
        }
        if (-not $hasExited) {
            try { $Process.Kill($true) } catch { $cleanupErrors.Add("process kill failed: $($_.Exception.Message)") }
            try {
                $Process.Refresh()
                $hasExited = [bool]$Process.HasExited
                if (-not $hasExited) { $hasExited = [bool]$Process.WaitForExit(5000) }
            } catch {
                $cleanupErrors.Add("process exit verification failed: $($_.Exception.Message)")
            }
        }
        if (-not $hasExited) { $cleanupErrors.Add("process is still running after cleanup") }

        $suffix = if ($cleanupErrors.Count -gt 0) { "; cleanup failures: $($cleanupErrors -join '; ')" } else { "" }
        throw "Could not place process $($Process.Id) in a kill-on-close Windows Job Object: $assignmentError$suffix"
    }
}

# Compile the native bridge before the first process is launched. The tracked
# wrapper remains blocked until assignment succeeds, so its target cannot race
# ahead of AssignProcessToJobObject.
if ($IsWindows) { Initialize-ProcessJobApi }

function Stop-ProcessTree {
    param([Parameter(Mandatory = $true)]$Process)

    $cleanupErrors = [System.Collections.Generic.List[string]]::new()
    $jobProperty = $Process.PSObject.Properties["ParlerJobHandle"]
    if ($jobProperty -and [IntPtr]$jobProperty.Value -ne [IntPtr]::Zero) {
        $jobHandle = [IntPtr]$jobProperty.Value
        # Atomically relinquish ownership before a native call that always
        # closes the handle, even when TerminateJobObject itself fails.
        $Process.ParlerJobHandle = [IntPtr]::Zero
        try { Stop-NativeProcessJob -JobHandle $jobHandle } catch {
            $cleanupErrors.Add("Windows Job Object cleanup failed for process $($Process.Id): $($_.Exception.Message)")
        }
    }

    $hasExited = $false
    try {
        $Process.Refresh()
        $hasExited = [bool]$Process.HasExited
    } catch {
        $cleanupErrors.Add("Process state refresh failed for $($Process.Id): $($_.Exception.Message)")
    }
    if (-not $hasExited) {
        try { $Process.Kill($true) } catch {
            try { Stop-Process -Id $Process.Id -Force -ErrorAction Stop } catch {
                $cleanupErrors.Add("Process kill failed for $($Process.Id): $($_.Exception.Message)")
            }
        }
        try {
            $Process.Refresh()
            $hasExited = [bool]$Process.HasExited
            if (-not $hasExited) { $hasExited = [bool]$Process.WaitForExit(5000) }
        } catch {
            $cleanupErrors.Add("Process exit verification failed for $($Process.Id): $($_.Exception.Message)")
        }
        if (-not $hasExited) { $cleanupErrors.Add("Process tree $($Process.Id) did not terminate within 5 seconds after kill.") }
    }

    $tempProperty = $Process.PSObject.Properties["ParlerProcessTempDir"]
    if ($tempProperty -and $tempProperty.Value) {
        try { Remove-Item -LiteralPath ([string]$tempProperty.Value) -Recurse -Force -ErrorAction Stop } catch {
            $cleanupErrors.Add("Process wrapper cleanup failed: $($_.Exception.Message)")
        }
        $Process.ParlerProcessTempDir = ""
    }
    if ($cleanupErrors.Count -gt 0) { throw ($cleanupErrors -join "; ") }
}

function Start-TrackedProcess {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string]$ArgumentList = "",
        [string]$RedirectStandardOutput = "",
        [string]$RedirectStandardError = ""
    )

    if (-not $IsWindows) {
        $start = @{ FilePath = $FilePath; ArgumentList = $ArgumentList; PassThru = $true }
        if ($RedirectStandardOutput) { $start.RedirectStandardOutput = $RedirectStandardOutput }
        if ($RedirectStandardError) { $start.RedirectStandardError = $RedirectStandardError }
        return Start-Process @start
    }

    $tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
    $tempDir = Join-Path $tempRoot ("parler-process-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tempDir -ErrorAction Stop | Out-Null
    $payloadPath = Join-Path $tempDir "payload.json"
    $readyPath = Join-Path $tempDir "assigned.ready"
    $payload = @{
        FilePath = $FilePath
        ArgumentList = $ArgumentList
        RedirectStandardOutput = $RedirectStandardOutput
        RedirectStandardError = $RedirectStandardError
    } | ConvertTo-Json -Depth 4
    [System.IO.File]::WriteAllText($payloadPath, $payload, [System.Text.UTF8Encoding]::new($false))

    $wrapperPath = Join-Path $PSScriptRoot "process-job-wrapper.ps1"
    $wrapper = $null
    try {
        $wrapperArguments = "-NoLogo -NoProfile -NonInteractive -File `"$wrapperPath`" -PayloadPath `"$payloadPath`" -ReadyPath `"$readyPath`""
        $wrapper = Start-Process -FilePath "pwsh" -ArgumentList $wrapperArguments -PassThru
        $wrapper = Register-ProcessJob -Process $wrapper
        $wrapper | Add-Member -MemberType NoteProperty -Name ParlerProcessTempDir -Value $tempDir -Force
        [System.IO.File]::WriteAllText($readyPath, "assigned", [System.Text.UTF8Encoding]::new($false))
        return $wrapper
    } catch {
        $startError = $_.Exception.Message
        if ($wrapper) {
            try { Stop-ProcessTree -Process $wrapper } catch { $startError += "; wrapper cleanup failed: $($_.Exception.Message)" }
        } else {
            Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
        }
        throw "Could not start tracked process '$FilePath': $startError"
    }
}

function Invoke-BoundedProcess {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string]$ArgumentList = "",
        [Parameter(Mandatory = $true)][ValidateRange(1, 3600)][int]$TimeoutSeconds,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $process = Start-TrackedProcess -FilePath $FilePath -ArgumentList $ArgumentList
    try {
        $completed = $process.WaitForExit($TimeoutSeconds * 1000)
    } catch {
        $waitError = $_.Exception.Message
        try { Stop-ProcessTree -Process $process } catch {
            throw "$Description wait failed: $waitError; process cleanup also failed: $($_.Exception.Message)"
        }
        throw "$Description wait failed: $waitError"
    }
    if (-not $completed) {
        Stop-ProcessTree -Process $process
        throw "$Description timed out after $TimeoutSeconds seconds and was terminated."
    }
    return $process
}

function Stop-ProductProcesses {
    param([Parameter(Mandatory = $true)][string]$ExePath)

    if ([string]::IsNullOrWhiteSpace($ExePath)) { return }
    $expectedPath = [System.IO.Path]::GetFullPath($ExePath)
    $processName = [System.IO.Path]::GetFileNameWithoutExtension($expectedPath)
    foreach ($candidate in @(Get-Process -Name $processName -ErrorAction SilentlyContinue)) {
        $candidatePath = ""
        try { $candidatePath = [string]$candidate.Path } catch { continue }
        if ($candidatePath -and ([System.IO.Path]::GetFullPath($candidatePath) -ieq $expectedPath)) {
            Stop-ProcessTree -Process $candidate
        }
    }
    $lingering = @(Get-Process -Name $processName -ErrorAction SilentlyContinue | Where-Object {
        try { $_.Path -and ([System.IO.Path]::GetFullPath([string]$_.Path) -ieq $expectedPath) } catch { $false }
    })
    if ($lingering.Count -gt 0) {
        throw "Installed executable process cleanup failed for '$expectedPath'; $($lingering.Count) instance(s) still running."
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
        try {
            Get-ItemProperty -Path $hive -ErrorAction Stop
        } catch [System.Management.Automation.ItemNotFoundException] {
            continue
        }
    }
    return @($entries)
}

function Get-UninstallEntryIdentity {
    param([Parameter(Mandatory = $true)]$Entry)

    $identity = [string]$Entry.PSPath
    if ([string]::IsNullOrWhiteSpace($identity)) {
        throw "Uninstall metadata is missing the exact registry PSPath identity."
    }
    return $identity
}

function Test-TypedUninstallEntry {
    param(
        [Parameter(Mandatory = $true)]$Entry,
        [Parameter(Mandatory = $true)][string]$ProductName,
        [Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType
    )

    if ($Entry.DisplayName -ne $ProductName) { return $false }
    if ($InstallerType -eq "msi") {
        return ([int]$Entry.WindowsInstaller -eq 1) -and (Test-MsiProductCode -ProductCode ([string]$Entry.PSChildName))
    }
    return [int]$Entry.WindowsInstaller -ne 1
}

function Select-UninstallEntry {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()]$Entries,
        [Parameter(Mandatory = $true)][string]$ProductName,
        [Parameter(Mandatory = $true)][ValidateSet("msi", "nsis")][string]$InstallerType,
        [string[]]$ExcludedIdentities = @()
    )

    $matched = @($Entries | Where-Object {
        if (-not (Test-TypedUninstallEntry -Entry $_ -ProductName $ProductName -InstallerType $InstallerType)) { return $false }
        $identity = [string]$_.PSPath
        return -not ($identity -and ($ExcludedIdentities -icontains $identity))
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
        [Parameter(Mandatory = $true)][string]$BinaryName,
        [string]$TrustedInstallRoot = ""
    )

    $installLocation = [string]$Entry.InstallLocation
    $rootCandidate = if ($TrustedInstallRoot) { $TrustedInstallRoot } else { $installLocation }
    $resolvedInstallLocation = ""
    if ($rootCandidate) {
        if (-not (Test-Path $rootCandidate -PathType Container)) {
            throw "Trusted install root does not exist: '$rootCandidate'."
        }
        $resolvedInstallLocation = (Resolve-Path $rootCandidate).Path
        $hits = @(Get-ChildItem -Path $resolvedInstallLocation -Recurse -Filter $BinaryName -File -ErrorAction Stop)
        if ($hits.Count -eq 1) {
            return $hits[0].FullName
        }
        if ($hits.Count -gt 1) {
            throw "Multiple '$BinaryName' found under trusted install root '$rootCandidate'."
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
            $resolvedIcon = (Resolve-Path $iconPath).Path
            if ($resolvedInstallLocation) {
                $relative = [System.IO.Path]::GetRelativePath($resolvedInstallLocation, $resolvedIcon)
                if ([System.IO.Path]::IsPathRooted($relative) -or $relative -eq ".." -or $relative.StartsWith("..$([System.IO.Path]::DirectorySeparatorChar)")) {
                    throw "DisplayIcon executable is outside InstallLocation: '$resolvedIcon'."
                }
            }
            return $resolvedIcon
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

function Normalize-RegistryPath {
    param([AllowEmptyString()][string]$Path)

    $normalized = $Path.Trim()
    if ($normalized.Length -ge 2 -and $normalized.StartsWith('"') -and $normalized.EndsWith('"')) {
        $normalized = $normalized.Substring(1, $normalized.Length - 2)
    }
    return [Environment]::ExpandEnvironmentVariables($normalized)
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
    if (-not (Test-Path $parsed.File -PathType Leaf)) {
        throw "NSIS uninstall executable does not exist: $($parsed.File)"
    }
    $uninstaller = (Resolve-Path $parsed.File).Path
    if ([System.IO.Path]::GetFileName($uninstaller) -ine "uninstall.exe") {
        throw "NSIS uninstall executable must be named uninstall.exe."
    }

    $rawInstallLocation = [string]$Entry.InstallLocation
    $installLocation = Normalize-RegistryPath -Path $rawInstallLocation
    if ([string]::IsNullOrWhiteSpace($installLocation)) {
        $installRoot = (Resolve-Path (Split-Path -Parent $uninstaller)).Path
    } else {
        if (-not (Test-Path $installLocation -PathType Container)) {
            throw "NSIS uninstall metadata declares a missing InstallLocation directory."
        }
        $installRoot = (Resolve-Path $installLocation).Path
        $relative = [System.IO.Path]::GetRelativePath($installRoot, $uninstaller)
        if ([System.IO.Path]::IsPathRooted($relative) -or $relative -eq ".." -or $relative.StartsWith("..$([System.IO.Path]::DirectorySeparatorChar)")) {
            throw "NSIS uninstall executable is outside InstallLocation: $uninstaller"
        }
    }

    return @{ File = $uninstaller; Args = "/S"; InstallRoot = $installRoot }
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

    $residue = @(Get-ChildItem -Path $InstallDir -Recurse -Force -ErrorAction Stop)
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
        try {
            if ($process.ExitCode -ne 0) {
                throw "MSI install failed with exit code $($process.ExitCode). See $log"
            }
            return
        } finally {
            Stop-ProcessTree -Process $process
        }
    }

    $process = Invoke-BoundedProcess -FilePath $Path -ArgumentList "/S" -TimeoutSeconds $TimeoutSeconds -Description "NSIS install"
    try {
        if ($process.ExitCode -ne 0) {
            throw "NSIS install failed with exit code $($process.ExitCode)."
        }
        $lastMetadataError = ""
        while ((Get-Date) -lt $deadline) {
            $found = @(Get-UninstallEntries | Where-Object {
                Test-TypedUninstallEntry -Entry $_ -ProductName $ProductName -InstallerType nsis
            })
            if ($found.Count -eq 1) {
                try {
                    Get-UninstallCommand -Entry $found[0] -InstallerType nsis -LogDir $LogDir | Out-Null
                    return
                } catch {
                    $lastMetadataError = $_.Exception.Message
                }
            } elseif ($found.Count -gt 1) {
                $lastMetadataError = "Ambiguous NSIS uninstall metadata: $($found.Count) entries."
            }
            Start-Sleep -Seconds 1
        }
        throw "NSIS install completed but no trusted '$ProductName' uninstall entry became ready within $TimeoutSeconds seconds. Last metadata error: $lastMetadataError"
    } finally {
        Stop-ProcessTree -Process $process
    }
}

function Open-LaunchLogForRead {
    param([Parameter(Mandatory = $true)][string]$Path)
    return [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
}

function Get-BoundedLogContent {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][ValidateRange(1, [long]::MaxValue)][long]$MaxBytes,
        [ValidateRange(0, 30)][int]$OpenRetrySeconds = 5
    )

    if (-not (Test-Path $Path -PathType Leaf)) { return "" }
    $deadline = (Get-Date).AddSeconds($OpenRetrySeconds)
    $stream = $null
    while (-not $stream) {
        try {
            $stream = Open-LaunchLogForRead -Path $Path
        } catch [System.IO.IOException] {
            if ((Get-Date) -ge $deadline) { throw }
            Start-Sleep -Milliseconds 100
        }
    }
    try {
        if ($stream.Length -gt $MaxBytes) {
            throw "Installed Parler launch log exceeded the $MaxBytes byte safety limit: $Path"
        }
        $bytes = [byte[]]::new([int]$stream.Length)
        $read = $stream.Read($bytes, 0, $bytes.Length)
        return [System.Text.Encoding]::UTF8.GetString($bytes, 0, $read)
    } finally {
        $stream.Dispose()
    }
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
        $process = Start-TrackedProcess `
            -FilePath $ExePath `
            -ArgumentList "--no-tray" `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath

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

        $process.Refresh()
        $exitedEarly = $process.HasExited
        $exitCode = if ($exitedEarly) { $process.ExitCode } else { $null }

        # Freeze all writers before the final size check and bounded read.
        Stop-ProcessTree -Process $process
        $process = $null
        $stdout = Get-BoundedLogContent -Path $stdoutPath -MaxBytes $MaxLogBytes
        $stderr = Get-BoundedLogContent -Path $stderrPath -MaxBytes $MaxLogBytes

        if ($exitedEarly) {
            Write-Host "--- parler stdout ---"
            Write-Host $stdout
            Write-Host "--- parler stderr ---"
            Write-Host $stderr
            throw "Installed Parler exited during launch gate with code $exitCode"
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
        [Parameter(Mandatory = $true)][string]$EntryIdentity,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$ExePath,
        [Parameter(Mandatory = $true)][ValidateRange(1, 3600)][int]$TimeoutSeconds
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $process = Invoke-BoundedProcess -FilePath $Command.File -ArgumentList $Command.Args -TimeoutSeconds $TimeoutSeconds -Description "Uninstall command"
    try {
        if ($process.ExitCode -ne 0) {
            throw "Uninstall command '$($Command.File) $($Command.Args)' failed with exit code $($process.ExitCode)."
        }

        do {
            $registryGone = @(Get-UninstallEntries | Where-Object { [string]$_.PSPath -ieq $EntryIdentity }).Count -eq 0
            $exeGone = (-not $ExePath) -or (-not (Test-Path $ExePath -PathType Leaf))
            if ($registryGone -and $exeGone) {
                Write-Host "Uninstall complete: registry entry and executable removed."
                return
            }
            Start-Sleep -Seconds 1
        } while ((Get-Date) -lt $deadline)

        throw "Uninstall did not complete within $TimeoutSeconds seconds (registryGone=$registryGone, exeGone=$exeGone)."
    } finally {
        Stop-ProcessTree -Process $process
    }
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
    $entryIdentity = ""
    $exePath = ""
    $installDir = ""
    $command = $null
    $beforeIdentities = @()
    $installationStarted = $false

    try {
        $installer = Get-InstallerFile -BundleDir $BundleDir -InstallerType $InstallerType
        Write-Host "Resolved $InstallerType installer: $installer"

        $beforeEntries = @(Get-UninstallEntries)
        $preExisting = @($beforeEntries | Where-Object {
            Test-TypedUninstallEntry -Entry $_ -ProductName $ProductName -InstallerType $InstallerType
        })
        if ($preExisting.Count -gt 0) {
            throw "Pre-existing $InstallerType uninstall metadata for '$ProductName' was found; refusing to overwrite or uninstall an untrusted instance."
        }
        $beforeIdentities = @($beforeEntries | ForEach-Object { [string]$_.PSPath } | Where-Object { $_ })

        $installationStarted = $true
        Install-Installer -Path $installer -InstallerType $InstallerType -LogDir $logDir -ProductName $ProductName -TimeoutSeconds $InstallTimeoutSeconds

        $entry = Select-UninstallEntry -Entries @(Get-UninstallEntries) -ProductName $ProductName -InstallerType $InstallerType -ExcludedIdentities $beforeIdentities
        $entryIdentity = Get-UninstallEntryIdentity -Entry $entry
        $command = Get-UninstallCommand -Entry $entry -InstallerType $InstallerType -LogDir $logDir
        $installDir = if ($command.InstallRoot) { [string]$command.InstallRoot } else { [string]$entry.InstallLocation }
        $exePath = Resolve-InstalledExecutable -Entry $entry -BinaryName $BinaryName -TrustedInstallRoot $installDir
        if (-not $installDir) { $installDir = Split-Path -Parent $exePath }
        Write-Host "Installed executable: $exePath"

        Invoke-LaunchGate -ExePath $exePath -LaunchSeconds $LaunchSeconds -LogDir $logDir -BinaryName $BinaryName -MaxLogBytes $MaxLaunchLogBytes
    }
    catch {
        $errors.Add($_.Exception.Message)
    }
    finally {
        if ($installationStarted -and $exePath) {
            try {
                Stop-ProductProcesses -ExePath $exePath
            } catch {
                $errors.Add("Process cleanup failed: $($_.Exception.Message)")
            }
        }

        if ($installationStarted -and -not $entry) {
            try {
                $entry = Select-UninstallEntry -Entries @(Get-UninstallEntries) -ProductName $ProductName -InstallerType $InstallerType -ExcludedIdentities $beforeIdentities
                $entryIdentity = Get-UninstallEntryIdentity -Entry $entry
            } catch {
                $errors.Add("Unable to identify installed entry for cleanup: $($_.Exception.Message)")
            }
        }

        if ($entry) {
            if (-not $installDir -and $entry.InstallLocation) {
                $installDir = [string]$entry.InstallLocation
            }
            try {
                if (-not $entryIdentity) {
                    $entryIdentity = Get-UninstallEntryIdentity -Entry $entry
                }
                if (-not $command) {
                    $command = Get-UninstallCommand -Entry $entry -InstallerType $InstallerType -LogDir $logDir
                }
                if (-not $installDir) {
                    $installDir = if ($command.InstallRoot) { [string]$command.InstallRoot } else { [string]$entry.InstallLocation }
                }
                Invoke-Uninstall -Command $command -EntryIdentity $entryIdentity -ExePath $exePath -TimeoutSeconds $UninstallTimeoutSeconds
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
