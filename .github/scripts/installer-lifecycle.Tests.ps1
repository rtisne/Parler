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

    It "parses QuietUninstallString for NSIS" {
        $entry = [PSCustomObject]@{ QuietUninstallString = '"C:\Users\me\AppData\Local\Parler\uninstall.exe" /S' }
        $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath())
        $cmd.File | Should -Be "C:\Users\me\AppData\Local\Parler\uninstall.exe"
        $cmd.Args | Should -Match "/S"
    }

    It "appends /S when only UninstallString is present" {
        $entry = [PSCustomObject]@{ UninstallString = '"C:\Users\me\AppData\Local\Parler\uninstall.exe"' }
        $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath())
        $cmd.File | Should -Be "C:\Users\me\AppData\Local\Parler\uninstall.exe"
        $cmd.Args | Should -Match "/S"
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
}

Describe "Invoke-BoundedProcess" {
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
}

Describe "bounded installer integration" {
    It "uses the bounded process helper for NSIS installation" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @([PSCustomObject]@{ DisplayName = "Parler" }) }

        Install-Installer -Path "setup.exe" -InstallerType nsis -LogDir ([System.IO.Path]::GetTempPath()) -ProductName "Parler" -TimeoutSeconds 1
        Should -Invoke Invoke-BoundedProcess -Times 1 -Exactly
    }

    It "uses the bounded process helper for uninstall" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @() }

        Invoke-Uninstall -Command @{ File = "uninstall.exe"; Args = "/S" } -ProductName "Parler" -ExePath "missing.exe" -TimeoutSeconds 1
        Should -Invoke Invoke-BoundedProcess -Times 1 -Exactly
    }

    It "cleans a detached uninstaller when completion polling times out" {
        $script:fakeProcess = New-FakeProcess -Completes $true -ExitCode 0
        Mock Invoke-BoundedProcess { return $script:fakeProcess }
        Mock Get-UninstallEntries { @([PSCustomObject]@{ DisplayName = "Parler" }) }
        Mock Stop-ProductProcesses {}

        { Invoke-Uninstall -Command @{ File = "uninstall.exe"; Args = "/S" } -ProductName "Parler" -ExePath "" -TimeoutSeconds 1 } |
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
            PSChildName = "{12345678-1234-1234-1234-1234567890ab}"
            WindowsInstaller = 1
            InstallLocation = "C:\Parler"
        }
        Mock Get-InstallerFile { "C:\bundle\Parler.msi" }
        Mock Install-Installer {}
        Mock Get-UninstallEntries { @($script:lifecycleEntry) }
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
}
