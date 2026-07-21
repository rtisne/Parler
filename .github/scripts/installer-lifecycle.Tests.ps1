BeforeAll {
    $env:PARLER_LIFECYCLE_NO_RUN = "1"
    . (Join-Path $PSScriptRoot "installer-lifecycle.ps1")
}

function New-TempDir {
    New-Item -ItemType Directory -Path (Join-Path ([System.IO.Path]::GetTempPath()) ([guid]::NewGuid())) |
        Select-Object -ExpandProperty FullName
}

Describe "Get-InstallerFile" {
    BeforeEach { $script:bundleDir = New-TempDir }
    AfterEach { Remove-Item -Recurse -Force $script:bundleDir -ErrorAction SilentlyContinue }

    It "returns the single msi in the bundle" {
        $msiDir = New-Item -ItemType Directory -Path (Join-Path $bundleDir "msi")
        New-Item -ItemType File -Path (Join-Path $msiDir "Parler_0.1.0_x64_en-US.msi") | Out-Null
        Get-InstallerFile -BundleDir $bundleDir -InstallerType msi | Should -Match "Parler_0.1.0_x64_en-US.msi"
    }

    It "throws when there is no msi" {
        New-Item -ItemType Directory -Path (Join-Path $bundleDir "msi") | Out-Null
        { Get-InstallerFile -BundleDir $bundleDir -InstallerType msi } | Should -Throw
    }

    It "throws when there are two msi files (ambiguous)" {
        $msiDir = New-Item -ItemType Directory -Path (Join-Path $bundleDir "msi")
        New-Item -ItemType File -Path (Join-Path $msiDir "a.msi") | Out-Null
        New-Item -ItemType File -Path (Join-Path $msiDir "b.msi") | Out-Null
        { Get-InstallerFile -BundleDir $bundleDir -InstallerType msi } | Should -Throw
    }

    It "resolves the nsis setup exe" {
        $nsisDir = New-Item -ItemType Directory -Path (Join-Path $bundleDir "nsis")
        New-Item -ItemType File -Path (Join-Path $nsisDir "Parler_0.1.0_x64-setup.exe") | Out-Null
        Get-InstallerFile -BundleDir $bundleDir -InstallerType nsis | Should -Match "setup.exe"
    }
}

Describe "Select-UninstallEntry" {
    It "returns the single Parler entry" {
        $entries = @(
            [PSCustomObject]@{ DisplayName = "Other App"; PSChildName = "x" },
            [PSCustomObject]@{ DisplayName = "Parler"; PSChildName = "{PRODUCT-GUID}" }
        )
        (Select-UninstallEntry -Entries $entries -ProductName "Parler").PSChildName | Should -Be "{PRODUCT-GUID}"
    }

    It "throws when no entry matches" {
        $entries = @([PSCustomObject]@{ DisplayName = "Other App" })
        { Select-UninstallEntry -Entries $entries -ProductName "Parler" } | Should -Throw
    }

    It "throws when multiple entries match (ambiguous)" {
        $entries = @(
            [PSCustomObject]@{ DisplayName = "Parler"; PSChildName = "a" },
            [PSCustomObject]@{ DisplayName = "Parler"; PSChildName = "b" }
        )
        { Select-UninstallEntry -Entries $entries -ProductName "Parler" } | Should -Throw
    }
}

Describe "Get-UninstallCommand" {
    It "builds an msiexec /x command from the ProductCode GUID" {
        $entry = [PSCustomObject]@{ PSChildName = "{1234-5678}" }
        $cmd = Get-UninstallCommand -Entry $entry -InstallerType msi -LogDir "C:\logs"
        $cmd.File | Should -Be "msiexec.exe"
        $cmd.Args | Should -Match "/x \{1234-5678\}"
        $cmd.Args | Should -Match "/qn"
        $cmd.Args | Should -Match "/norestart"
        $cmd.Args | Should -Match "/l\*v"
    }

    It "parses QuietUninstallString for NSIS" {
        $entry = [PSCustomObject]@{ QuietUninstallString = '"C:\Users\me\AppData\Local\Parler\uninstall.exe" /S' }
        $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir "C:\logs"
        $cmd.File | Should -Be "C:\Users\me\AppData\Local\Parler\uninstall.exe"
        $cmd.Args | Should -Match "/S"
    }

    It "appends /S when only UninstallString is present" {
        $entry = [PSCustomObject]@{ UninstallString = '"C:\Users\me\AppData\Local\Parler\uninstall.exe"' }
        $cmd = Get-UninstallCommand -Entry $entry -InstallerType nsis -LogDir "C:\logs"
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

    It "passes when only non-executable leftovers remain" {
        New-Item -ItemType File -Path (Join-Path $installDir "install.log") | Out-Null
        { Assert-UninstallResidue -InstallDir $installDir -BinaryName "parler.exe" } | Should -Not -Throw
    }
}
