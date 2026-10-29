# Runs the real installer against a local release, without network or user PATH changes.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression.FileSystem
$installer = Join-Path $PSScriptRoot '..\install.ps1'
$root = Join-Path ([IO.Path]::GetTempPath()) ('rn-install-test-' + [guid]::NewGuid().ToString('n'))
[IO.Directory]::CreateDirectory($root) | Out-Null
$saved = @{}
foreach ($name in @('TEMP', 'TMP', 'REASON_NETWORK_BIN_DIR', 'REASON_NETWORK_DOWNLOAD_BASE', 'REASON_NETWORK_NO_PROMPT')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
function Assert($condition, $message) { if (-not $condition) { throw $message } }

# Only replace transport. All checksum, extraction and replacement logic is the actual installer.
function Invoke-WebRequest {
    param([string]$Uri, [string]$OutFile, [switch]$UseBasicParsing)
    if ($Uri.EndsWith('.sha256')) { $source = "$testArchive.sha256" } else { $source = $testArchive }
    [IO.File]::Copy($source, $OutFile)
}

# End after the installed executable has run, before the installer updates the user's PATH.
function Write-Host {
    param([Parameter(Position = 0)]$Object)
    if ("$Object".StartsWith('Installed ')) { throw 'INSTALL_TEST_COMPLETE' }
}

function Move-Item {
    param([Parameter(Position = 0)][string]$Path, [Parameter(Position = 1)][string]$Destination, [string]$LiteralPath)
    $source = if ($PSBoundParameters.ContainsKey('LiteralPath')) { $LiteralPath } else { $Path }
    if ($testFailInstall -and $Destination -eq $testExe -and $source -ne $testExe -and
        [IO.Path]::GetFileName($source) -eq 'reason-network.exe') {
        Assert (-not [IO.File]::Exists($testExe)) 'Old binary was not moved aside before replacement'
        Assert (@(Get-ChildItem -LiteralPath ([IO.Path]::GetDirectoryName($testExe)) -Filter '*.old').Count -eq 1) 'Old binary backup is missing'
        throw 'INSTALL_TEST_MOVE_FAILED'
    }
    if ($PSBoundParameters.ContainsKey('LiteralPath')) {
        Microsoft.PowerShell.Management\Move-Item -LiteralPath $LiteralPath -Destination $Destination
    } else {
        Microsoft.PowerShell.Management\Move-Item -Path $Path -Destination $Destination
    }
}

try {
    $fixture = Join-Path $root 'fixture.exe'
    # A harmless executable: the installer really launches it for both version probes.
    Add-Type -TypeDefinition 'public static class InstallerFixture { public static void Main() { System.Console.WriteLine("reason-network 1.0.1"); } }' -OutputAssembly $fixture -OutputType ConsoleApplication
    $release = Join-Path $root 'release'
    [IO.Directory]::CreateDirectory($release) | Out-Null
    [IO.File]::Copy($fixture, (Join-Path $release 'reason-network.exe'))
    $testArchive = Join-Path $root 'release.zip'
    [IO.Compression.ZipFile]::CreateFromDirectory($release, $testArchive)
    $checksum = (Get-FileHash -LiteralPath $testArchive -Algorithm SHA256).Hash.ToLowerInvariant()
    $newHash = (Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash
    # Build the Unicode portion without a source-encoding dependency in Windows PowerShell 5.1.
    $unicode = [string][char]0x9879 + [char]0x76ee
    $failures = @()
    foreach ($case in @('plain', 'fresh', 'upgrade', 'rollback', 'checksum', 'bin-only', 'temp-only')) {
        $caseRoot = Join-Path $root $case
        $bin = Join-Path $caseRoot "bin [stable] $unicode"
        if ($case -in @('plain', 'temp-only')) { $bin = Join-Path $caseRoot 'plain-bin' }
        $temp = Join-Path $caseRoot "temp [work] $unicode"
        if ($case -in @('plain', 'bin-only')) { $temp = Join-Path $caseRoot 'plain-temp' }
        # This sibling matches [stable] as a PowerShell wildcard; it must never be touched.
        $sibling = Join-Path $caseRoot "bin s $unicode"
        foreach ($dir in @($bin, $temp, $sibling)) { [IO.Directory]::CreateDirectory($dir) | Out-Null }
        $sentinel = Join-Path $sibling 'reason-network.keep.old'
        [IO.File]::WriteAllText($sentinel, 'unrelated file')
        $testExe = Join-Path $bin 'reason-network.exe'
        $stale = Join-Path $bin 'reason-network.stale.old'
        [IO.File]::WriteAllText($stale, 'stale backup')
        $previousHash = $null
        $lock = $null
        if ($case -in @('upgrade', 'rollback', 'checksum')) {
            [IO.File]::WriteAllBytes($testExe, ([IO.File]::ReadAllBytes($fixture) + [byte[]](1, 2, 3)))
            $previousHash = (Get-FileHash -LiteralPath $testExe -Algorithm SHA256).Hash
            # Running executables allow rename but not overwrite on Windows.
            $lock = [IO.File]::Open($testExe, 'Open', 'Read', ([IO.FileShare]::Read -bor [IO.FileShare]::Delete))
        }
        $testFailInstall = $case -eq 'rollback'
        $want = if ($case -eq 'checksum') { '0' * 64 } else { $checksum }
        [IO.File]::WriteAllText("$testArchive.sha256", "$want  reason-network.zip")
        $env:TEMP = $temp
        $env:TMP = $temp
        $env:REASON_NETWORK_BIN_DIR = $bin
        $env:REASON_NETWORK_DOWNLOAD_BASE = 'https://installer-test.invalid'
        $env:REASON_NETWORK_NO_PROMPT = '1'
        $failure = $null
        try {
            try { & $installer } catch { $failure = $_.Exception.Message }
            Assert ([Environment]::GetEnvironmentVariable('Path', 'User') -eq $userPath) 'Installer changed user PATH'
            Assert ([IO.File]::ReadAllText($sentinel) -eq 'unrelated file') 'Wildcard sibling was changed'
            Assert (@(Get-ChildItem -LiteralPath $temp -Force).Count -eq 0) "Installer left temporary files ($failure)"
            if ($case -eq 'rollback') {
                Assert ($failure -eq 'INSTALL_TEST_MOVE_FAILED') "Unexpected rollback error: $failure"
                Assert ((Get-FileHash -LiteralPath $testExe).Hash -eq $previousHash) 'Rollback did not restore old binary'
                Assert (@(Get-ChildItem -LiteralPath $bin -Filter '*.old').Count -eq 0) 'Rollback left a backup behind'
            } elseif ($case -eq 'checksum') {
                Assert ($failure -like '*checksum mismatch*') "Unexpected checksum error: $failure"
                Assert ((Get-FileHash -LiteralPath $testExe).Hash -eq $previousHash) 'Checksum failure changed binary'
                Assert ([IO.File]::Exists($stale)) 'Checksum failure touched existing backups'
            } else {
                Assert ($failure -eq 'INSTALL_TEST_COMPLETE') "Installer did not finish: $failure"
                Assert ((Get-FileHash -LiteralPath $testExe).Hash -eq $newHash) 'Installed binary is not the new version'
                Assert (-not [IO.File]::Exists($stale)) 'Stale backup was not cleaned'
                $backups = @(Get-ChildItem -LiteralPath $bin -Filter '*.old')
                if ($case -eq 'upgrade') {
                    Assert ($backups.Count -eq 1) 'Upgrade did not keep the running old binary'
                    Assert ((Get-FileHash -LiteralPath $backups[0].FullName).Hash -eq $previousHash) 'Wrong backup content'
                } else { Assert ($backups.Count -eq 0) 'Fresh install created a backup' }
            }
            Write-Output "PASS: $case"
        } catch {
            $failures += "${case}: $($_.Exception.Message)"
            Write-Output "FAIL: $($failures[-1])"
        } finally { if ($lock) { $lock.Dispose() } }
    }
    Assert ($failures.Count -eq 0) ($failures -join "`n")
} finally {
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
    # The sole deletion target is the GUID directory created by this test, never a caller-supplied path.
    Remove-Item -LiteralPath $root -Recurse -Force
}
