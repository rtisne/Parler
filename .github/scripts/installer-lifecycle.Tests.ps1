BeforeAll {
    $script:previousLifecycleGuard = $env:PARLER_LIFECYCLE_NO_RUN
    $env:PARLER_LIFECYCLE_NO_RUN = "1"
    . (Join-Path $PSScriptRoot "installer-lifecycle.ps1")

    function New-TempDir {
        New-Item -ItemType Directory -Path (Join-Path ([System.IO.Path]::GetTempPath()) ([guid]::NewGuid())) |
            Select-Object -ExpandProperty FullName
    }

    function New-FakeProcess {
        param([bool]$Completes, [int]$ExitCode = 0)
        $fake = [PSCustomObject]@{
            Completes = $Completes
            ExitCode = $ExitCode
            HasExited = $Completes
            Killed = $false
            Id = 4242
        }
        $fake | Add-Member -MemberType ScriptMethod -Name Refresh -Value { }
        $fake | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
            param($TimeoutMilliseconds)
            if ($null -eq $TimeoutMilliseconds) { return $true }
            return $this.Completes
        }
        $fake | Add-Member -MemberType ScriptMethod -Name Kill -Value {
            param([bool]$EntireProcessTree)
            $this.Killed = $true
            $this.HasExited = $true
        }
        return $fake
    }
}

AfterAll {
    if ($null -eq $script:previousLifecycleGuard) {
        Remove-Item Env:PARLER_LIFECYCLE_NO_RUN -ErrorAction SilentlyContinue
    } else {
        $env:PARLER_LIFECYCLE_NO_RUN = $script:previousLifecycleGuard
    }
}

Describe "Stop-ProductProcesses" {
    It "stops only the process at the validated executable path" {
        $targetPath = [System.IO.Path]::GetFullPath((Join-Path ([System.IO.Path]::GetTempPath()) "trusted/parler.exe"))
        $otherPath = [System.IO.Path]::GetFullPath((Join-Path ([System.IO.Path]::GetTempPath()) "other/parler.exe"))
        $target = [PSCustomObject]@{ Path = $targetPath; Id = 1 }
        $other = [PSCustomObject]@{ Path = $otherPath; Id = 2 }
        $script:processReads = 0
        Mock Get-NamedProcessCandidates {
            $script:processReads++
            if ($script:processReads -eq 1) { return @($target, $other) }
            return @()
        }
        Mock Stop-ProcessTree {}

        Stop-ProductProcesses -ExePath $targetPath
        Should -Invoke Stop-ProcessTree -Times 1 -Exactly -ParameterFilter { $Process.Path -eq $targetPath }
    }

    It "fails when the exact target remains after cleanup" {
        $targetPath = [System.IO.Path]::GetFullPath((Join-Path ([System.IO.Path]::GetTempPath()) "trusted/parler.exe"))
        $target = [PSCustomObject]@{ Path = $targetPath; Id = 1 }
        Mock Get-NamedProcessCandidates { @($target) }
        Mock Stop-ProcessTree {}

        { Stop-ProductProcesses -ExePath $targetPath } | Should -Throw "*still running*"
    }

    It "fails closed when process enumeration errors" {
        Mock Get-NamedProcessCandidates { throw "enumeration denied" }
        { Stop-ProductProcesses -ExePath "C:\trusted\parler.exe" } | Should -Throw "*enumeration denied*"
    }

    It "fails closed when a same-name process path cannot be verified" {
        $candidate = [PSCustomObject]@{ Id = 99 }
        $candidate | Add-Member -MemberType ScriptProperty -Name Path -Value { throw "path denied" }
        Mock Get-NamedProcessCandidates { @($candidate) }

        { Stop-ProductProcesses -ExePath "C:\trusted\parler.exe" } | Should -Throw "*Could not verify executable path*"
    }
}

Describe "Get-InstallerFile" {
    BeforeEach { $script:testBundleDir = New-TempDir }
    AfterEach { Remove-Item -Recurse -Force $script:testBundleDir -ErrorAction SilentlyContinue }

    It "returns the single msi in the bundle" {
        $msiDir = New-Item -ItemType Directory -Path (Join-Path $testBundleDir "msi")
        New-Item -ItemType File -Path (Join-Path $msiDir "Parler_0.1.0_x64_en-US.msi") | Out-Null
        Get-InstallerFile -BundleDir $testBundleDir -InstallerType msi | Should -Match "Parler_0.1.0_x64_en-US.msi"
    }

    It "throws when there is no msi" {
        New-Item -ItemType Directory -Path (Join-Path $testBundleDir "msi") | Out-Null
        { Get-InstallerFile -BundleDir $testBundleDir -InstallerType msi } | Should -Throw
    }

    It "throws when there are two msi files (ambiguous)" {
        $msiDir = New-Item -ItemType Directory -Path (Join-Path $testBundleDir "msi")
        New-Item -ItemType File -Path (Join-Path $msiDir "a.msi") | Out-Null
        New-Item -ItemType File -Path (Join-Path $msiDir "b.msi") | Out-Null
        { Get-InstallerFile -BundleDir $testBundleDir -InstallerType msi } | Should -Throw
    }

    It "resolves the nsis setup exe" {
        $nsisDir = New-Item -ItemType Directory -Path (Join-Path $testBundleDir "nsis")
        New-Item -ItemType File -Path (Join-Path $nsisDir "Parler_0.1.0_x64-setup.exe") | Out-Null
        Get-InstallerFile -BundleDir $testBundleDir -InstallerType nsis | Should -Match "setup.exe"
    }
}

Describe "Get-UninstallEntries" {
    It "ignores only missing registry paths" {
        Mock Get-ItemProperty { throw [System.Management.Automation.ItemNotFoundException]::new("missing") }
        @(Get-UninstallEntries).Count | Should -Be 0
    }

    It "propagates registry provider and access failures" {
        Mock Get-ItemProperty { throw [System.UnauthorizedAccessException]::new("access denied") }
        { Get-UninstallEntries } | Should -Throw "*access denied*"
    }
}

Describe "Windows registry provider integration" {
    It "returns a real HKCU wildcard entry, ignores a missing key, and propagates an invalid drive" -Skip:(-not $IsWindows) {
        $previousHives = $script:UninstallHives
        $base = "HKCU:\Software\ParlerLifecycleTests\$([guid]::NewGuid().ToString('N'))"
        try {
            $child = New-Item -Path (Join-Path $base "Entry") -Force -ErrorAction Stop
            New-ItemProperty -Path $child.PSPath -Name DisplayName -Value "Parler Test" -Force -ErrorAction Stop | Out-Null

            $script:UninstallHives = @("$base\*")
            @(Get-UninstallEntries).DisplayName | Should -Be @("Parler Test")

            $script:UninstallHives = @("$base\Missing\*")
            @(Get-UninstallEntries).Count | Should -Be 0

            $script:UninstallHives = @("NoSuchRegistryDrive:\*")
            { Get-UninstallEntries } | Should -Throw
        } finally {
            $script:UninstallHives = $previousHives
            Remove-Item -LiteralPath $base -Recurse -Force -ErrorAction Stop
        }
    }
}

Describe "Select-UninstallEntry" {
    It "returns the single typed Parler entry" {
        $entries = @(
            [PSCustomObject]@{ DisplayName = "Other App"; PSChildName = "x" },
            [PSCustomObject]@{ DisplayName = "Parler"; PSChildName = "{12345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1 }
        )
        (Select-UninstallEntry -Entries $entries -ProductName "Parler" -InstallerType msi).PSChildName |
            Should -Be "{12345678-1234-1234-1234-1234567890ab}"
    }

    It "throws when no entry matches" {
        $entries = @([PSCustomObject]@{ DisplayName = "Other App" })
        { Select-UninstallEntry -Entries $entries -ProductName "Parler" -InstallerType msi } | Should -Throw
    }

    It "throws when multiple entries match (ambiguous)" {
        $entries = @(
            [PSCustomObject]@{ DisplayName = "Parler"; PSChildName = "{12345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1 },
            [PSCustomObject]@{ DisplayName = "Parler"; PSChildName = "{22345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1 }
        )
        { Select-UninstallEntry -Entries $entries -ProductName "Parler" -InstallerType msi } | Should -Throw
    }

    It "rejects a same-name non-MSI entry for an MSI lifecycle" {
        $entries = @([PSCustomObject]@{ DisplayName = "Parler"; PSChildName = "not-a-guid"; WindowsInstaller = 0 })
        { Select-UninstallEntry -Entries $entries -ProductName "Parler" -InstallerType msi } | Should -Throw
    }

    It "selects only the newly-created typed entry" {
        $old = [PSCustomObject]@{
            DisplayName = "Parler"; PSPath = "Registry::old"; PSChildName = "{12345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1
        }
        $new = [PSCustomObject]@{
            DisplayName = "Parler"; PSPath = "Registry::new"; PSChildName = "{22345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1
        }

        (Select-UninstallEntry -Entries @($old, $new) -ProductName "Parler" -InstallerType msi -ExcludedIdentities @("Registry::old")).PSPath |
            Should -Be "Registry::new"
    }
}

Describe "Get-UninstallCommand" {
    It "builds an msiexec /x command from the ProductCode GUID" {
        $logDir = New-TempDir
        try {
            $entry = [PSCustomObject]@{ PSChildName = "{12345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1 }
            $cmd = Get-UninstallCommand -Entry $entry -InstallerType msi -LogDir $logDir
            $cmd.File | Should -Be "msiexec.exe"
            $cmd.Args | Should -Be @(
                "/x", "{12345678-1234-1234-1234-1234567890ab}", "/qn", "/norestart", "/l*v",
                (Join-Path $logDir "msi-uninstall.log")
            )
        } finally {
            Remove-Item -Recurse -Force $logDir -ErrorAction SilentlyContinue
        }
    }

    It "rejects an invalid MSI ProductCode" {
        $entry = [PSCustomObject]@{ PSChildName = "{not-a-guid}"; WindowsInstaller = 1 }
        { Get-UninstallCommand -Entry $entry -InstallerType msi -LogDir ([System.IO.Path]::GetTempPath()) } | Should -Throw
    }

    It "infers the trusted NSIS install root from uninstall.exe when InstallLocation is absent" {
        $installDir = New-TempDir
        $logDir = New-TempDir
        try {
            $uninstaller = Join-Path $installDir "uninstall.exe"
            New-Item -ItemType File -Path $uninstaller | Out-Null
            $entry = [PSCustomObject]@{
                QuietUninstallString = "`"$uninstaller`" /S"
                InstallLocation = ""
            }
            $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir $logDir
            $cmd.InstallRoot | Should -Be (Resolve-Path $installDir).Path
        } finally {
            Remove-Item -Recurse -Force $installDir, $logDir -ErrorAction SilentlyContinue
        }
    }

    It "normalizes a quoted NSIS InstallLocation" {
        $installDir = New-TempDir
        try {
            $uninstaller = Join-Path $installDir "uninstall.exe"
            New-Item -ItemType File -Path $uninstaller | Out-Null
            $entry = [PSCustomObject]@{
                QuietUninstallString = "`"$uninstaller`" /S"
                InstallLocation = "`"$installDir`""
            }
            $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath())
            $cmd.InstallRoot | Should -Be (Resolve-Path $installDir).Path
        } finally {
            Remove-Item -Recurse -Force $installDir -ErrorAction SilentlyContinue
        }
    }

    It "rejects an unbalanced quote in NSIS InstallLocation" {
        $installDir = New-TempDir
        try {
            $uninstaller = Join-Path $installDir "uninstall.exe"
            New-Item -ItemType File -Path $uninstaller | Out-Null
            $entry = [PSCustomObject]@{
                QuietUninstallString = "`"$uninstaller`" /S"
                InstallLocation = "`"$installDir"
            }
            { Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath()) } |
                Should -Throw "*unmatched quote*"
        } finally {
            Remove-Item -Recurse -Force $installDir -ErrorAction SilentlyContinue
        }
    }

    It "normalizes whitespace and paired quotes but rejects either unmatched side" {
        Normalize-RegistryPath -Path '  "C:\Program Files\Parler"  ' | Should -Be "C:\Program Files\Parler"
        { Normalize-RegistryPath -Path '"C:\Program Files\Parler' } | Should -Throw "*unmatched quote*"
        { Normalize-RegistryPath -Path 'C:\Program Files\Parler"' } | Should -Throw "*unmatched quote*"
    }

    It "parses a trusted QuietUninstallString for NSIS" {
        $installDir = New-TempDir
        try {
            $uninstaller = Join-Path $installDir "uninstall.exe"
            New-Item -ItemType File -Path $uninstaller | Out-Null
            $entry = [PSCustomObject]@{ InstallLocation = $installDir; QuietUninstallString = "`"$uninstaller`" /S _?=C:\attacker /D=C:\elsewhere" }
            $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath())
            $cmd.File | Should -Be $uninstaller
            $cmd.Args | Should -Be "/S"
        } finally {
            Remove-Item -Recurse -Force $installDir -ErrorAction SilentlyContinue
        }
    }

    It "appends /S when a trusted UninstallString is present" {
        $installDir = New-TempDir
        try {
            $uninstaller = Join-Path $installDir "uninstall.exe"
            New-Item -ItemType File -Path $uninstaller | Out-Null
            $entry = [PSCustomObject]@{ InstallLocation = $installDir; UninstallString = "`"$uninstaller`"" }
            $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath())
            $cmd.File | Should -Be $uninstaller
            $cmd.Args | Should -Match "/S"
        } finally {
            Remove-Item -Recurse -Force $installDir -ErrorAction SilentlyContinue
        }
    }

    It "rejects an NSIS uninstaller outside InstallLocation" {
        $installDir = New-TempDir
        $outsideDir = New-TempDir
        try {
            $uninstaller = Join-Path $outsideDir "uninstall.exe"
            New-Item -ItemType File -Path $uninstaller | Out-Null
            $entry = [PSCustomObject]@{ InstallLocation = $installDir; QuietUninstallString = "`"$uninstaller`" /S" }
            { Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath()) } | Should -Throw
        } finally {
            Remove-Item -Recurse -Force $installDir, $outsideDir -ErrorAction SilentlyContinue
        }
    }

    It "rejects a missing NSIS uninstaller" {
        $installDir = New-TempDir
        try {
            $missing = Join-Path $installDir "uninstall.exe"
            $entry = [PSCustomObject]@{ InstallLocation = $installDir; QuietUninstallString = "`"$missing`" /S" }
            { Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath()) } | Should -Throw
        } finally {
            Remove-Item -Recurse -Force $installDir -ErrorAction SilentlyContinue
        }
    }
}

Describe "Resolve-InstalledExecutable" {
    BeforeEach { $script:installDir = New-TempDir }
    AfterEach { Remove-Item -Recurse -Force $script:installDir -ErrorAction SilentlyContinue }

    It "resolves the binary from InstallLocation" {
        $exe = Join-Path $installDir "parler.exe"
        New-Item -ItemType File -Path $exe | Out-Null
        $entry = [PSCustomObject]@{ InstallLocation = $installDir; DisplayIcon = "" }
        Resolve-InstalledExecutable -Entry $entry -BinaryName "parler.exe" | Should -Be $exe
    }

    It "throws when the executable is missing" {
        $entry = [PSCustomObject]@{ InstallLocation = $installDir; DisplayIcon = "" }
        { Resolve-InstalledExecutable -Entry $entry -BinaryName "parler.exe" } | Should -Throw
    }

    It "falls back to DisplayIcon when InstallLocation is empty" {
        $exe = Join-Path $installDir "parler.exe"
        New-Item -ItemType File -Path $exe | Out-Null
        $entry = [PSCustomObject]@{ InstallLocation = ""; DisplayIcon = "$exe,0" }
        Resolve-InstalledExecutable -Entry $entry -BinaryName "parler.exe" | Should -Be $exe
    }

    It "rejects a DisplayIcon that is not the requested binary" {
        $icon = Join-Path $installDir "other.exe"
        New-Item -ItemType File -Path $icon | Out-Null
        $entry = [PSCustomObject]@{ InstallLocation = ""; DisplayIcon = "$icon,0" }
        { Resolve-InstalledExecutable -Entry $entry -BinaryName "parler.exe" } | Should -Throw
    }

    It "rejects a same-name DisplayIcon outside a declared InstallLocation" {
        $outsideDir = New-TempDir
        try {
            $icon = Join-Path $outsideDir "parler.exe"
            New-Item -ItemType File -Path $icon | Out-Null
            $entry = [PSCustomObject]@{ InstallLocation = $installDir; DisplayIcon = "$icon,0" }
            { Resolve-InstalledExecutable -Entry $entry -BinaryName "parler.exe" } | Should -Throw
        } finally {
            Remove-Item -Recurse -Force $outsideDir -ErrorAction SilentlyContinue
        }
    }

    It "accepts DisplayIcon inside the inferred trusted NSIS root" {
        $icon = Join-Path $installDir "parler.exe"
        New-Item -ItemType File -Path $icon | Out-Null
        $entry = [PSCustomObject]@{ InstallLocation = ""; DisplayIcon = "$icon,0" }
        (Resolve-InstalledExecutable -Entry $entry -BinaryName "parler.exe" -TrustedInstallRoot $installDir) |
            Should -Be (Resolve-Path $icon).Path
    }

    It "binds DisplayIcon to the inferred trusted NSIS root" {
        $outsideDir = New-TempDir
        try {
            $icon = Join-Path $outsideDir "parler.exe"
            New-Item -ItemType File -Path $icon | Out-Null
            $entry = [PSCustomObject]@{ InstallLocation = ""; DisplayIcon = "$icon,0" }
            { Resolve-InstalledExecutable -Entry $entry -BinaryName "parler.exe" -TrustedInstallRoot $installDir } | Should -Throw
        } finally {
            Remove-Item -Recurse -Force $outsideDir -ErrorAction SilentlyContinue
        }
    }
}

Describe "Assert-UninstallResidue" {
    BeforeEach { $script:installDir = New-TempDir }
    AfterEach { Remove-Item -Recurse -Force $script:installDir -ErrorAction SilentlyContinue }

    It "throws when the binary remains" {
        New-Item -ItemType File -Path (Join-Path $installDir "parler.exe") | Out-Null
        { Assert-UninstallResidue -InstallDir $installDir -BinaryName "parler.exe" } | Should -Throw
    }

    It "throws when a dll remains" {
        New-Item -ItemType File -Path (Join-Path $installDir "libopenblas.dll") | Out-Null
        { Assert-UninstallResidue -InstallDir $installDir -BinaryName "parler.exe" } | Should -Throw
    }

    It "passes when the directory is gone" {
        Remove-Item -Recurse -Force $installDir
        { Assert-UninstallResidue -InstallDir $installDir -BinaryName "parler.exe" } | Should -Not -Throw
    }

    It "throws when any installed payload remains" {
        New-Item -ItemType File -Path (Join-Path $installDir "install.log") | Out-Null
        { Assert-UninstallResidue -InstallDir $installDir -BinaryName "parler.exe" } | Should -Throw
    }

    It "passes when the directory exists but is empty" {
        { Assert-UninstallResidue -InstallDir $installDir -BinaryName "parler.exe" } | Should -Not -Throw
    }

    It "fails closed when residue enumeration errors" {
        Mock Get-ChildItem { throw "access denied" }
        { Assert-UninstallResidue -InstallDir $installDir -BinaryName "parler.exe" } | Should -Throw "*access denied*"
    }
}

Describe "Get-BoundedLogContent" {
    It "retries a transient sharing violation after process termination" {
        $path = Join-Path (New-TempDir) "locked.log"
        try {
            Set-Content -LiteralPath $path -Value "placeholder"
            $script:openAttempts = 0
            Mock Open-LaunchLogForRead {
                $script:openAttempts++
                if ($script:openAttempts -eq 1) { throw [System.IO.IOException]::new("file is in use") }
                return [System.IO.MemoryStream]::new([System.Text.Encoding]::UTF8.GetBytes("startup ok"))
            }
            Mock Test-IsSharingViolation { $true }
            Mock Wait-LifecycleRetry {}

            Get-BoundedLogContent -Path $path -MaxBytes 16 -OpenRetrySeconds 1 | Should -Be "startup ok"
            Should -Invoke Open-LaunchLogForRead -Times 2 -Exactly
            Should -Invoke Wait-LifecycleRetry -Times 1 -Exactly
        } finally {
            Remove-Item -LiteralPath (Split-Path -Parent $path) -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    It "throws immediately when a sharing violation retry deadline is exhausted" {
        $path = Join-Path (New-TempDir) "locked.log"
        try {
            Set-Content -LiteralPath $path -Value "placeholder"
            $script:clockReads = 0
            $origin = [datetime]"2026-01-01T00:00:00Z"
            Mock Get-LifecycleNow {
                $script:clockReads++
                if ($script:clockReads -eq 1) { return $origin }
                return $origin.AddSeconds(2)
            }
            Mock Open-LaunchLogForRead { throw [System.IO.IOException]::new("sharing deadline") }
            Mock Test-IsSharingViolation { $true }
            Mock Wait-LifecycleRetry {}

            { Get-BoundedLogContent -Path $path -MaxBytes 16 -OpenRetrySeconds 1 } | Should -Throw "*sharing deadline*"
            Should -Invoke Wait-LifecycleRetry -Times 0 -Exactly
        } finally {
            Remove-Item -LiteralPath (Split-Path -Parent $path) -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    It "does not retry a non-sharing I/O failure" {
        $path = Join-Path (New-TempDir) "broken.log"
        try {
            Set-Content -LiteralPath $path -Value "placeholder"
            Mock Open-LaunchLogForRead { throw [System.IO.IOException]::new("disk failure") }
            Mock Test-IsSharingViolation { $false }
            Mock Wait-LifecycleRetry {}

            { Get-BoundedLogContent -Path $path -MaxBytes 16 -OpenRetrySeconds 5 } | Should -Throw "*disk failure*"
            Should -Invoke Open-LaunchLogForRead -Times 1 -Exactly
            Should -Invoke Wait-LifecycleRetry -Times 0 -Exactly
        } finally {
            Remove-Item -LiteralPath (Split-Path -Parent $path) -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    It "disposes the opened stream after reading" {
        $path = Join-Path (New-TempDir) "stream.log"
        try {
            Set-Content -LiteralPath $path -Value "placeholder"
            $script:openedStream = [System.IO.MemoryStream]::new([System.Text.Encoding]::UTF8.GetBytes("ok"))
            Mock Open-LaunchLogForRead { return $script:openedStream }

            Get-BoundedLogContent -Path $path -MaxBytes 16 | Should -Be "ok"
            $script:openedStream.CanRead | Should -BeFalse
        } finally {
            Remove-Item -LiteralPath (Split-Path -Parent $path) -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    It "retries a real Windows FileShare.None lock until release" -Skip:(-not $IsWindows) {
        $dir = New-TempDir
        $job = $null
        try {
            $path = Join-Path $dir "native-locked.log"
            $ready = Join-Path $dir "lock-ready.txt"
            [System.IO.File]::WriteAllText($path, "native lock released")
            $job = Start-Job -ArgumentList $path, $ready -ScriptBlock {
                param($LockedPath, $ReadyPath)
                $stream = [System.IO.File]::Open($LockedPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
                try {
                    [System.IO.File]::WriteAllText($ReadyPath, "ready")
                    Start-Sleep -Milliseconds 750
                } finally { $stream.Dispose() }
            }
            $deadline = (Get-Date).AddSeconds(10)
            while (-not (Test-Path -LiteralPath $ready) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 50 }
            Test-Path -LiteralPath $ready | Should -BeTrue

            Get-BoundedLogContent -Path $path -MaxBytes 64 -OpenRetrySeconds 5 | Should -Be "native lock released"
            Wait-Job -Job $job -Timeout 10 | Should -Not -BeNullOrEmpty
            Receive-Job -Job $job -ErrorAction Stop | Out-Null
            Remove-Job -Job $job -Force
            $job = $null
        } finally {
            if ($job) {
                Stop-Job -Job $job -ErrorAction Stop
                Remove-Job -Job $job -Force
            }
            Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction Stop
        }
    }

    It "rejects a log larger than the byte cap" {
        $dir = New-TempDir
        try {
            $path = Join-Path $dir "stdout.log"
            [System.IO.File]::WriteAllBytes($path, [byte[]](1..32))
            { Get-BoundedLogContent -Path $path -MaxBytes 16 } | Should -Throw "*safety limit*"
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }

    It "reads a log at or below the byte cap" {
        $dir = New-TempDir
        try {
            $path = Join-Path $dir "stderr.log"
            [System.IO.File]::WriteAllText($path, "startup ok")
            Get-BoundedLogContent -Path $path -MaxBytes 16 | Should -Be "startup ok"
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "Windows Job Object ownership" {
    It "reports assignment, job cleanup, process kill, and liveness failures together" {
        $fake = New-FakeProcess -Completes $false
        $script:killAttempts = 0
        $fake | Add-Member -MemberType ScriptMethod -Name Kill -Force -Value {
            $script:killAttempts++
            throw "kill denied"
        }
        $job = [PSCustomObject]@{ IsClosed = $false; IsInvalid = $false }
        Mock New-ProcessJobHandle { $job }
        Mock Add-ProcessToJob { throw "assignment denied" }
        Mock Stop-NativeProcessJob { throw "job cleanup denied" }

        { Register-ProcessJob -Process $fake -ForceWindowsSemantics } |
            Should -Throw "*assignment denied*job cleanup denied*kill denied*still running*"
        Should -Invoke New-ProcessJobHandle -Times 1 -Exactly
        Should -Invoke Add-ProcessToJob -Times 1 -Exactly -ParameterFilter { $JobHandle -eq $job -and $ProcessId -eq $fake.Id }
        Should -Invoke Stop-NativeProcessJob -Times 1 -Exactly -ParameterFilter { $JobHandle -eq $job }
        $script:killAttempts | Should -Be 1
    }

    It "relinquishes SafeHandle ownership before native termination" {
        $fake = New-FakeProcess -Completes $true
        $handle = [PSCustomObject]@{ IsClosed = $false; IsInvalid = $false }
        $fake | Add-Member -MemberType NoteProperty -Name ParlerJobHandle -Value $handle -Force
        Mock Stop-NativeProcessJob { throw "terminate failed" }

        { Stop-ProcessTree -Process $fake } | Should -Throw "*terminate failed*"
        $fake.ParlerJobHandle | Should -BeNullOrEmpty
        Should -Invoke Stop-NativeProcessJob -Times 1 -Exactly -ParameterFilter { $JobHandle -eq $handle }
    }
}

Describe "Invoke-BoundedProcess" {
    BeforeEach {
        Mock Start-TrackedProcess { return $script:fakeProcess }
    }

    It "kills the process tree and throws when the timeout expires" {
        $script:fakeProcess = New-FakeProcess -Completes $false
        Mock Start-Process { return $script:fakeProcess }

        { Invoke-BoundedProcess -FilePath "installer.exe" -ArgumentList "/S" -TimeoutSeconds 1 -Description "installer" } |
            Should -Throw "*timed out*"
        $script:fakeProcess.Killed | Should -BeTrue
    }

    It "returns a successful bounded process" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Start-Process { return $script:fakeProcess }

        (Invoke-BoundedProcess -FilePath "installer.exe" -ArgumentList "/S" -TimeoutSeconds 1 -Description "installer").ExitCode |
            Should -Be 0
    }

    It "cleans the process tree when waiting throws" {
        $script:fakeProcess = New-FakeProcess -Completes $false
        $script:fakeProcess | Add-Member -MemberType ScriptMethod -Name WaitForExit -Force -Value { throw "wait failed" }
        Mock Start-Process { return $script:fakeProcess }

        { Invoke-BoundedProcess -FilePath "installer.exe" -ArgumentList "/S" -TimeoutSeconds 1 -Description "installer" } |
            Should -Throw "*wait failed*"
        $script:fakeProcess.Killed | Should -BeTrue
    }
}

Describe "Windows Job Object handshake integration" {
    It "does not launch the payload and cleans wrapper state when assignment fails" -Skip:(-not $IsWindows) {
        $root = New-TempDir
        $previousRunnerTemp = $env:RUNNER_TEMP
        $script:assignmentWrapper = $null
        try {
            $env:RUNNER_TEMP = $root
            $marker = Join-Path $root "payload-started.txt"
            $payloadScript = Join-Path $root "marker payload.ps1"
            Set-Content -LiteralPath $payloadScript -Value "Set-Content -LiteralPath '$($marker.Replace("'", "''"))' -Value started"
            $rejectAssignment = {
                param($Process)
                $script:assignmentWrapper = $Process
                throw "injected assignment failure"
            }

            { Start-TrackedProcess -FilePath "pwsh" -ArgumentList @("-NoProfile", "-File", $payloadScript) -RegisterProcess $rejectAssignment } |
                Should -Throw "*injected assignment failure*"
            $script:assignmentWrapper | Should -Not -BeNullOrEmpty
            $script:assignmentWrapper.WaitForExit(5000) | Should -BeTrue
            Test-Path -LiteralPath $marker | Should -BeFalse
            @(Get-ChildItem -LiteralPath $root -Directory -Filter "parler-process-*" -ErrorAction Stop).Count | Should -Be 0
        } finally {
            if ($script:assignmentWrapper -and -not $script:assignmentWrapper.HasExited) {
                $script:assignmentWrapper.Kill($true)
                $script:assignmentWrapper.WaitForExit(5000) | Out-Null
            }
            $env:RUNNER_TEMP = $previousRunnerTemp
            Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction Stop
        }
    }
}

Describe "Windows wrapper argument and redirection integration" {
    It "preserves argument boundaries, redirects both logs, and propagates exit code" -Skip:(-not $IsWindows) {
        $dir = New-TempDir
        $process = $null
        try {
            $scriptFile = Join-Path $dir "argument payload.ps1"
            $resultFile = Join-Path $dir "captured arguments.json"
            $stdoutFile = Join-Path $dir "wrapper stdout.log"
            $stderrFile = Join-Path $dir "wrapper stderr.log"
            Set-Content -LiteralPath $scriptFile -Value @'
param(
    [Parameter(Mandatory = $true)][string]$OutputPath,
    [Parameter(ValueFromRemainingArguments = $true)][AllowEmptyString()][string[]]$Captured
)
@($Captured) | ConvertTo-Json -Compress | Set-Content -LiteralPath $OutputPath
Write-Output "stdout marker"
[Console]::Error.WriteLine("stderr marker")
exit 7
'@
            $expected = @("value with spaces", 'embedded"quote', "")
            $process = Start-TrackedProcess -FilePath "pwsh" -ArgumentList (@(
                "-NoProfile", "-File", $scriptFile, $resultFile
            ) + $expected) -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
            $tempDir = [string]$process.ParlerProcessTempDir

            $process.WaitForExit(30000) | Should -BeTrue
            $process.Refresh()
            $process.ExitCode | Should -Be 7
            @((Get-Content -LiteralPath $resultFile -Raw | ConvertFrom-Json)) | Should -Be $expected
            Get-Content -LiteralPath $stdoutFile -Raw | Should -Match "stdout marker"
            Get-Content -LiteralPath $stderrFile -Raw | Should -Match "stderr marker"

            Stop-ProcessTree -Process $process
            $process = $null
            Test-Path -LiteralPath $tempDir | Should -BeFalse
        } finally {
            if ($process) { Stop-ProcessTree -Process $process }
            Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction Stop
        }
    }
}

Describe "Windows Job Object process-tree integration" {
    It "terminates a child after its tracked parent has already exited" -Skip:(-not $IsWindows) {
        $dir = New-TempDir
        $parent = $null
        $childId = $null
        try {
            $pidFile = Join-Path $dir "child.pid"
            $scriptFile = Join-Path $dir "spawn-child.ps1"
            $scriptText = @'
$child = Start-Process -FilePath pwsh -ArgumentList @("-NoProfile", "-Command", "Start-Sleep -Seconds 60") -PassThru
Set-Content -LiteralPath '__PID_FILE__' -Value $child.Id
'@
            $scriptText = $scriptText.Replace('__PID_FILE__', $pidFile.Replace("'", "''"))
            Set-Content -LiteralPath $scriptFile -Value $scriptText

            $parentArguments = @("-NoProfile", "-File", $scriptFile)
            $parent = Start-TrackedProcess -FilePath pwsh -ArgumentList $parentArguments
            $parent.WaitForExit(30000) | Should -BeTrue

            $deadline = (Get-Date).AddSeconds(30)
            while (-not (Test-Path $pidFile) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
            Test-Path $pidFile | Should -BeTrue
            $childId = [int](Get-Content $pidFile -Raw)
            Get-Process -Id $childId -ErrorAction Stop | Should -Not -BeNullOrEmpty

            $tempDir = [string]$parent.ParlerProcessTempDir
            Stop-ProcessTree -Process $parent
            $parent = $null
            $deadline = (Get-Date).AddSeconds(30)
            while ((Get-Process -Id $childId -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
            Get-Process -Id $childId -ErrorAction SilentlyContinue | Should -BeNullOrEmpty
            Test-Path -LiteralPath $tempDir | Should -BeFalse
        } finally {
            if ($parent) { Stop-ProcessTree -Process $parent }
            if ($childId -and (Get-Process -Id $childId -ErrorAction SilentlyContinue)) {
                Stop-Process -Id $childId -Force -ErrorAction Stop
            }
            Remove-Item -Recurse -Force $dir -ErrorAction Stop
        }
    }
}

Describe "bounded installer integration" {
    It "uses the bounded process helper for NSIS installation" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @([PSCustomObject]@{ DisplayName = "Parler" }) }
        Mock Get-UninstallCommand { @{ File = "uninstall.exe"; Args = "/S" } }

        Install-Installer -Path "setup.exe" -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath()) -ProductName "Parler" -TimeoutSeconds 1
        Should -Invoke Invoke-BoundedProcess -Times 1 -Exactly
    }

    It "uses the bounded process helper for uninstall" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @() }

        Invoke-Uninstall -Command @{ File = "uninstall.exe"; Args = "/S" } -EntryIdentity "Registry::target" -ExePath "missing.exe" -TimeoutSeconds 1
        Should -Invoke Invoke-BoundedProcess -Times 1 -Exactly
    }

    It "does not accept a selected registry key that remains without DisplayName" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @([PSCustomObject]@{ PSPath = "Registry::target"; DisplayName = "" }) }
        Mock Stop-ProductProcesses {}

        { Invoke-Uninstall -Command @{ File = "uninstall.exe"; Args = "/S" } -EntryIdentity "Registry::target" -ExePath "" -TimeoutSeconds 1 } |
            Should -Throw "*registryGone=False*"
    }

    It "ignores an unrelated same-name entry after the selected key disappears" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @([PSCustomObject]@{ PSPath = "Registry::other"; DisplayName = "Parler" }) }

        { Invoke-Uninstall -Command @{ File = "uninstall.exe"; Args = "/S" } -EntryIdentity "Registry::target" -ExePath "" -TimeoutSeconds 1 } |
            Should -Not -Throw
    }

    It "fails closed when uninstall registry enumeration errors" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { throw "registry access denied" }

        { Invoke-Uninstall -Command @{ File = "uninstall.exe"; Args = "/S" } -EntryIdentity "Registry::target" -ExePath "" -TimeoutSeconds 1 } |
            Should -Throw "*registry access denied*"
    }

    It "does not globally kill same-name uninstallers when completion polling times out" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @([PSCustomObject]@{ PSPath = "Registry::target"; DisplayName = "Parler" }) }
        Mock Stop-ProductProcesses {}

        { Invoke-Uninstall -Command @{ File = "uninstall.exe"; Args = "/S" } -EntryIdentity "Registry::target" -ExePath "" -TimeoutSeconds 1 } |
            Should -Throw "*did not complete*"
        Should -Invoke Stop-ProductProcesses -Times 0 -Exactly
    }
}

Describe "Get-DiagnosticsDir" {
    It "isolates diagnostics by installer type" {
        $msi = Get-DiagnosticsDir -InstallerType msi
        $nsis = Get-DiagnosticsDir -InstallerType nsis
        $msi | Should -Not -Be $nsis
        Split-Path -Leaf $msi | Should -Be "msi"
        Split-Path -Leaf $nsis | Should -Be "nsis"
    }
}

Describe "Invoke-InstallerLifecycle cleanup" {
    It "attempts uninstall when the launch gate fails" {
        $script:lifecycleEntry = [PSCustomObject]@{
            DisplayName = "Parler"
            PSPath = "Registry::target"
            PSChildName = "{12345678-1234-1234-1234-1234567890ab}"
            WindowsInstaller = 1
            InstallLocation = "C:\Parler"
        }
        $script:lifecycleInstalled = $false
        Mock Get-InstallerFile { "C:\bundle\Parler.msi" }
        Mock Install-Installer { $script:lifecycleInstalled = $true }
        Mock Get-UninstallEntries { if ($script:lifecycleInstalled) { @($script:lifecycleEntry) } else { @() } }
        Mock Select-UninstallEntry { $script:lifecycleEntry }
        Mock Resolve-InstalledExecutable { "C:\Parler\parler.exe" }
        Mock Get-UninstallCommand { @{ File = "msiexec.exe"; Args = "/x product" } }
        Mock Invoke-LaunchGate { throw "startup panic" }
        Mock Stop-ProductProcesses {}
        Mock Invoke-Uninstall {}
        Mock Assert-UninstallResidue {}

        { Invoke-InstallerLifecycle -InstallerType msi -BundleDir "C:\bundle" -ProductName "Parler" -BinaryName "parler.exe" -LaunchSeconds 1 -InstallTimeoutSeconds 1 -UninstallTimeoutSeconds 1 } |
            Should -Throw "*startup panic*"
        Should -Invoke Invoke-Uninstall -Times 1 -Exactly
    }

    It "refuses to install over a pre-existing same-name typed entry" {
        $preexisting = [PSCustomObject]@{
            DisplayName = "Parler"; PSPath = "Registry::old"; PSChildName = "{12345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1
        }
        Mock Get-InstallerFile { "C:\bundle\Parler.msi" }
        Mock Get-UninstallEntries { @($preexisting) }
        Mock Install-Installer {}
        Mock Stop-ProductProcesses {}

        { Invoke-InstallerLifecycle -InstallerType msi -BundleDir "C:\bundle" -ProductName "Parler" -BinaryName "parler.exe" -LaunchSeconds 1 -InstallTimeoutSeconds 1 -UninstallTimeoutSeconds 1 } |
            Should -Throw "*Pre-existing*"
        Should -Invoke Install-Installer -Times 0 -Exactly
        Should -Invoke Stop-ProductProcesses -Times 0 -Exactly
    }

    It "reports cleanup discovery failure instead of silently skipping uninstall" {
        Mock Get-InstallerFile { "C:\bundle\Parler.msi" }
        Mock Get-UninstallEntries { @() }
        Mock Install-Installer {}
        Mock Select-UninstallEntry { throw "ambiguous metadata" }
        Mock Stop-ProductProcesses {}
        Mock Invoke-Uninstall {}

        { Invoke-InstallerLifecycle -InstallerType msi -BundleDir "C:\bundle" -ProductName "Parler" -BinaryName "parler.exe" -LaunchSeconds 1 -InstallTimeoutSeconds 1 -UninstallTimeoutSeconds 1 } |
            Should -Throw "*Unable to identify installed entry for cleanup*"
        Should -Invoke Invoke-Uninstall -Times 0 -Exactly
    }

    It "reports post-install registry enumeration failure during cleanup discovery" {
        $script:registryReads = 0
        Mock Get-InstallerFile { "C:\bundle\Parler.msi" }
        Mock Get-UninstallEntries {
            $script:registryReads++
            if ($script:registryReads -eq 1) { return @() }
            throw "registry provider unavailable"
        }
        Mock Install-Installer {}
        Mock Stop-ProductProcesses {}
        Mock Invoke-Uninstall {}

        { Invoke-InstallerLifecycle -InstallerType msi -BundleDir "C:\bundle" -ProductName "Parler" -BinaryName "parler.exe" -LaunchSeconds 1 -InstallTimeoutSeconds 1 -UninstallTimeoutSeconds 1 } |
            Should -Throw "*registry provider unavailable*Unable to identify installed entry for cleanup*"
        Should -Invoke Invoke-Uninstall -Times 0 -Exactly
    }

    It "uninstalls from the validated entry when executable resolution fails" {
        $script:resolutionEntry = [PSCustomObject]@{
            DisplayName = "Parler"; PSPath = "Registry::target"; PSChildName = "{12345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1; InstallLocation = "C:\Parler"
        }
        $script:resolutionInstalled = $false
        Mock Get-InstallerFile { "C:\bundle\Parler.msi" }
        Mock Get-UninstallEntries { if ($script:resolutionInstalled) { @($script:resolutionEntry) } else { @() } }
        Mock Install-Installer { $script:resolutionInstalled = $true }
        Mock Select-UninstallEntry { $script:resolutionEntry }
        Mock Resolve-InstalledExecutable { throw "binary missing" }
        Mock Get-UninstallCommand { @{ File = "msiexec.exe"; Args = "/x product" } }
        Mock Stop-ProductProcesses {}
        Mock Invoke-Uninstall {}
        Mock Assert-UninstallResidue {}

        { Invoke-InstallerLifecycle -InstallerType msi -BundleDir "C:\bundle" -ProductName "Parler" -BinaryName "parler.exe" -LaunchSeconds 1 -InstallTimeoutSeconds 1 -UninstallTimeoutSeconds 1 } |
            Should -Throw "*binary missing*"
        Should -Invoke Invoke-Uninstall -Times 1 -Exactly
    }

    It "checks residue from InstallLocation rather than the executable parent" {
        $script:residueEntry = [PSCustomObject]@{
            DisplayName = "Parler"; PSPath = "Registry::target"; PSChildName = "{12345678-1234-1234-1234-1234567890ab}"; WindowsInstaller = 1; InstallLocation = "C:\Parler"
        }
        $script:residueInstalled = $false
        Mock Get-InstallerFile { "C:\bundle\Parler.msi" }
        Mock Get-UninstallEntries { if ($script:residueInstalled) { @($script:residueEntry) } else { @() } }
        Mock Install-Installer { $script:residueInstalled = $true }
        Mock Select-UninstallEntry { $script:residueEntry }
        Mock Resolve-InstalledExecutable { "C:\Parler\bin\parler.exe" }
        Mock Get-UninstallCommand { @{ File = "msiexec.exe"; Args = "/x product" } }
        Mock Invoke-LaunchGate {}
        Mock Stop-ProductProcesses {}
        Mock Invoke-Uninstall {}
        Mock Assert-UninstallResidue {}

        Invoke-InstallerLifecycle -InstallerType msi -BundleDir "C:\bundle" -ProductName "Parler" -BinaryName "parler.exe" -LaunchSeconds 1 -InstallTimeoutSeconds 1 -UninstallTimeoutSeconds 1
        Should -Invoke Assert-UninstallResidue -Times 1 -Exactly -ParameterFilter { $InstallDir -eq "C:\Parler" }
    }
}
