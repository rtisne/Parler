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
            $cmd.Args | Should -Match "/x \{12345678-1234-1234-1234-1234567890ab\}"
            $cmd.Args | Should -Match "/qn"
            $cmd.Args | Should -Match "/norestart"
            $cmd.Args | Should -Match "/l\*v"
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

    It "parses a trusted QuietUninstallString for NSIS" {
        $installDir = New-TempDir
        try {
            $uninstaller = Join-Path $installDir "uninstall.exe"
            New-Item -ItemType File -Path $uninstaller | Out-Null
            $entry = [PSCustomObject]@{ InstallLocation = $installDir; QuietUninstallString = "`"$uninstaller`" /S" }
            $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath())
            $cmd.File | Should -Be $uninstaller
            $cmd.Args | Should -Match "/S"
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

Describe "Invoke-BoundedProcess" {
    BeforeEach {
        Mock Register-ProcessJob { return $Process }
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

Describe "Windows Job Object process-tree integration" {
    It "terminates a child after its tracked parent has already exited" -Skip:(-not $IsWindows) {
        $dir = New-TempDir
        $parent = $null
        $childId = $null
        try {
            $pidFile = Join-Path $dir "child.pid"
            $scriptFile = Join-Path $dir "spawn-child.ps1"
            $scriptText = @'
Start-Sleep -Milliseconds 750
$child = Start-Process -FilePath pwsh -ArgumentList @("-NoProfile", "-Command", "Start-Sleep -Seconds 60") -PassThru
Set-Content -LiteralPath '__PID_FILE__' -Value $child.Id
'@
            $scriptText = $scriptText.Replace('__PID_FILE__', $pidFile.Replace("'", "''"))
            Set-Content -LiteralPath $scriptFile -Value $scriptText

            $parent = Start-Process -FilePath pwsh -ArgumentList @("-NoProfile", "-File", $scriptFile) -PassThru
            $parent = Register-ProcessJob -Process $parent
            $parent.WaitForExit(10000) | Should -BeTrue

            $deadline = (Get-Date).AddSeconds(10)
            while (-not (Test-Path $pidFile) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
            Test-Path $pidFile | Should -BeTrue
            $childId = [int](Get-Content $pidFile -Raw)
            Get-Process -Id $childId -ErrorAction Stop | Should -Not -BeNullOrEmpty

            Stop-ProcessTree -Process $parent
            $deadline = (Get-Date).AddSeconds(5)
            while ((Get-Process -Id $childId -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
            Get-Process -Id $childId -ErrorAction SilentlyContinue | Should -BeNullOrEmpty
        } finally {
            if ($parent) { try { Stop-ProcessTree -Process $parent } catch { } }
            if ($childId) { Stop-Process -Id $childId -Force -ErrorAction SilentlyContinue }
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
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

    It "cleans a detached uninstaller when completion polling times out" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @([PSCustomObject]@{ PSPath = "Registry::target"; DisplayName = "Parler" }) }
        Mock Stop-ProductProcesses {}

        { Invoke-Uninstall -Command @{ File = "uninstall.exe"; Args = "/S" } -EntryIdentity "Registry::target" -ExePath "" -TimeoutSeconds 1 } |
            Should -Throw "*did not complete*"
        Should -Invoke Stop-ProductProcesses -Times 1 -Exactly -ParameterFilter { $BinaryName -eq "uninstall.exe" }
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
