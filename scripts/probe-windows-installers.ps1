# Run only on an ephemeral GitHub-hosted runner. Never use a user's installation.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$CandidateSetup,
    [Parameter(Mandatory)][string]$CandidateBinary,
    [string]$CandidateVersion = '3.20.4-fork.4-probe.1',
    [Parameter(Mandatory)][string]$EvidenceDirectory
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_OS -ne 'Windows' -or -not $env:RUNNER_TEMP) {
    throw 'Installer probes are restricted to ephemeral Windows GitHub Actions runners.'
}

$runnerRoot = [IO.Path]::GetFullPath($env:RUNNER_TEMP)
$testRoot = [IO.Path]::GetFullPath((Join-Path $runnerRoot 'cc-switch-installer-probe'))
if (-not $testRoot.StartsWith($runnerRoot.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Probe directory is outside RUNNER_TEMP.'
}
if (Test-Path -LiteralPath $testRoot) { throw 'Probe directory must be fresh.' }
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\CC Switch'
$machineKey = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\CC Switch'
if ((Test-Path $uninstallKey) -or (Test-Path $machineKey) -or (Get-Process cc-switch -ErrorAction SilentlyContinue)) {
    throw 'Runner already has CC Switch installed or running.'
}

New-Item -ItemType Directory -Path $testRoot, $EvidenceDirectory -Force | Out-Null
$installDir = Join-Path $testRoot '中文 空格 & 安装目录'
$oldSetup = Join-Path $testRoot 'old-setup.exe'
$oldUrl = 'https://github.com/qwqwd65-ui/cc-switch/releases/download/v3.20.4-fork.3/CC-Switch-v3.20.4-fork.3-Windows-Setup.exe'
$oldDigest = '27329df67ca6d6b783c76444dad0d99f2c27701ccdac37afebab619a98295a8b'
$oldVersion = '3.20.4-fork.3'
$report = [Collections.Generic.List[object]]::new()
$exe = Join-Path $installDir 'cc-switch.exe'
$eventName = 'CCSwitchInstallerProbeProcessStart'
$watcher = $null
$blockedAcl = $false
$originalSddl = $null

function Save-Report {
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $EvidenceDirectory 'installer-results.json') -Encoding utf8
}

function Write-ProbeAnnotation([string]$Level, [string]$Title, [string]$Message) {
    $escaped = $Message.Replace('%', '%25').Replace("`r", '%0D').Replace("`n", '%0A')
    Write-Output "::${Level} title=${Title}::$escaped"
}

function Get-InstallerWindowState([int]$ProcessId) {
    # Compiled and invoked only on the isolated GA runner. This reads window
    # labels; it does not click buttons or dismiss an installer error.
    if (-not ('RollbackProbe.Windows' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
namespace RollbackProbe {
  public static class Windows {
    private delegate bool EnumProc(IntPtr hwnd, IntPtr param);
    [DllImport("user32.dll")] private static extern bool EnumWindows(EnumProc proc, IntPtr param);
    [DllImport("user32.dll")] private static extern bool EnumChildWindows(IntPtr parent, EnumProc proc, IntPtr param);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int max);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetClassName(IntPtr hwnd, StringBuilder text, int max);
    private static string Label(IntPtr hwnd) {
      var text = new StringBuilder(2048);
      var cls = new StringBuilder(128);
      GetWindowText(hwnd, text, text.Capacity);
      GetClassName(hwnd, cls, cls.Capacity);
      return cls.ToString() + ": " + text.ToString();
    }
    public static string[] Read(int processId) {
      var labels = new List<string>();
      EnumWindows((hwnd, param) => {
        uint pid;
        GetWindowThreadProcessId(hwnd, out pid);
        if (pid == (uint)processId) {
          labels.Add(Label(hwnd));
          EnumChildWindows(hwnd, (child, unused) => {
            labels.Add(Label(child));
            return true;
          }, IntPtr.Zero);
        }
        return true;
      }, IntPtr.Zero);
      return labels.ToArray();
    }
  }
}

'@
    }
    return [RollbackProbe.Windows]::Read($ProcessId)
}

function Initialize-BinaryProbe {
    # C# is compiled only on GA; report offsets/bytes, not executable contents.
    if (-not ('RollbackProbe.BinaryDiff' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Collections.Generic;
namespace RollbackProbe {
  public static class BinaryDiff {
    public static string[] Read(string expectedPath, string actualPath) {
      var expected = File.ReadAllBytes(expectedPath);
      var actual = File.ReadAllBytes(actualPath);
      var lines = new List<string>();
      long count = Math.Abs((long)expected.Length - actual.Length);
      for (int i = 0; i < Math.Min(expected.Length, actual.Length); i++) {
        if (expected[i] != actual[i]) {
          count++;
          if (lines.Count < 24) lines.Add(i.ToString("X8") + ": " + expected[i].ToString("X2") + " -> " + actual[i].ToString("X2"));
        }
      }
      lines.Insert(0, "Differing bytes: " + count);
      return lines.ToArray();
    }
    public static void WriteNsisReference(string sourcePath, string destinationPath) {
      var bytes = File.ReadAllBytes(sourcePath);
      var marker = System.Text.Encoding.ASCII.GetBytes("__TAURI_BUNDLE_TYPE_VAR_UNK");
      int found = -1;
      int count = 0;
      for (int i = 0; i <= bytes.Length - marker.Length; i++) {
        bool matches = true;
        for (int j = 0; j < marker.Length; j++) {
          if (bytes[i + j] != marker[j]) { matches = false; break; }
        }
        if (matches) { count++; found = i; }
      }
      if (count != 1) throw new InvalidDataException("Expected exactly one Tauri UNK bundle marker; found " + count);
      var nsis = System.Text.Encoding.ASCII.GetBytes("NSS");
      Array.Copy(nsis, 0, bytes, found + marker.Length - 3, 3);
      using (var output = new FileStream(destinationPath, FileMode.CreateNew, FileAccess.Write)) {
        output.Write(bytes, 0, bytes.Length);
        output.Flush(true);
      }
    }
  }
}
'@
    }
}

function Get-ExecutableByteDifferences([string]$Expected, [string]$Actual) {
    Initialize-BinaryProbe
    return [RollbackProbe.BinaryDiff]::Read((Resolve-Path -LiteralPath $Expected).Path, (Resolve-Path -LiteralPath $Actual).Path)
}

function Assert-NoLaunch([string]$Stage) {
    # Give the trace provider time to deliver short-lived process events too.
    Start-Sleep -Seconds 2
    $events = @(Get-Event -SourceIdentifier $eventName -ErrorAction SilentlyContinue)
    $running = @(Get-Process cc-switch -ErrorAction SilentlyContinue)
    if ($events.Count -gt 0 -or $running.Count -gt 0) {
        throw "$Stage started cc-switch.exe before data restoration."
    }
}

function Run-Setup([string]$Setup, [string]$Mode, [string]$Stage, [bool]$ExpectRunningRejection = $false) {
    # NSIS /D must be the last argument and is deliberately NOT quoted.
    # ProcessStartInfo avoids shell interpretation of spaces, Unicode and &.
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = (Resolve-Path -LiteralPath $Setup).Path
    $info.Arguments = "$Mode /NS /D=$installDir"
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    Write-ProbeAnnotation 'notice' 'Installer stage' $Stage
    $process = [Diagnostics.Process]::Start($info)
    if (-not $process.WaitForExit(180000)) {
        $registryState = if (Test-Path -LiteralPath $uninstallKey) {
            $entry = Get-ItemProperty -LiteralPath $uninstallKey
            [ordered]@{ version = $entry.DisplayVersion; path = $entry.InstallLocation }
        } else { $null }
        $timeoutState = [ordered]@{
            stage = $Stage; mainWindowTitle = $process.MainWindowTitle
            controls = @(Get-InstallerWindowState $process.Id)
            installed = $registryState
            executableSha256 = if (Test-Path -LiteralPath $exe) { (Get-FileHash -LiteralPath $exe).Hash } else { $null }
        }
        $report.Add($timeoutState)
        Save-Report
        Write-ProbeAnnotation 'error' 'Installer timeout state' ($timeoutState | ConvertTo-Json -Depth 6 -Compress)
        $process.Kill($true)
        $process.Dispose()
        throw "$Stage installer timed out."
    }
    $exitCode = $process.ExitCode
    $process.Dispose()
    $report.Add([ordered]@{ stage = $Stage; mode = $Mode; exitCode = $exitCode })
    Save-Report
    if ($ExpectRunningRejection) {
        if ($exitCode -eq 0) { throw "$Stage unexpectedly succeeded while a process was running." }
        return
    }
    if ($exitCode -ne 0) { throw "$Stage installer exited with $exitCode." }
    Assert-NoLaunch $Stage
}

function Assert-Installed([string]$Version, [string]$Digest, [string]$Stage) {
    $entry = Get-ItemProperty -LiteralPath $uninstallKey
    if ($entry.DisplayVersion -ne $Version) { throw "${Stage}: wrong installed version $($entry.DisplayVersion)." }
    if ($entry.InstallLocation.Trim('"') -ne $installDir) { throw "${Stage}: installer changed directories." }
    if ((Test-Path $machineKey)) { throw "${Stage}: installation changed user scope." }
    $actualDigest = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualDigest -ne $Digest) {
        $reference = if ($Digest -eq $candidateDigest) { $candidateReference } else { $oldReference }
        $mismatch = [ordered]@{
            stage = $Stage; expectedSha256 = $Digest; actualSha256 = $actualDigest
            installedVersion = $entry.DisplayVersion
            peProductVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion
            sourcePeProductVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo((Resolve-Path -LiteralPath $CandidateBinary).Path).ProductVersion
            installedBytes = (Get-Item -LiteralPath $exe).Length
            sourceBytes = (Get-Item -LiteralPath $CandidateBinary).Length
            differences = @(Get-ExecutableByteDifferences $reference $exe)
        }
        $report.Add($mismatch)
        Save-Report
        Write-ProbeAnnotation 'error' 'Installed executable mismatch' ($mismatch | ConvertTo-Json -Depth 4 -Compress)
        throw "${Stage}: installed executable does not match its package."
    }
    $report.Add([ordered]@{
        stage = $Stage; version = $entry.DisplayVersion; path = $entry.InstallLocation
        scope = 'currentUser'; executableSha256 = $Digest
        peProductVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion
    })
    Save-Report
}

try {
    $watcher = Register-CimIndicationEvent -Query "SELECT * FROM Win32_ProcessStartTrace WHERE ProcessName = 'cc-switch.exe'" -SourceIdentifier $eventName
    Invoke-WebRequest -Uri $oldUrl -OutFile $oldSetup -TimeoutSec 120
    if ((Get-FileHash -LiteralPath $oldSetup -Algorithm SHA256).Hash.ToLowerInvariant() -ne $oldDigest) {
        throw 'Historical production Setup differs from the pinned GitHub asset digest.'
    }
    # Locked Tauri CLI 2.11.5 stamps __TAURI_BUNDLE_TYPE_VAR_UNK to NSS before
    # NSIS packaging, then restores the unsigned/unpatched source executable.
    # See crates/tauri-bundler/src/bundle.rs at tag tauri-cli-v2.11.5.
    # This unsigned probe reproduces only that exact three-byte substitution
    # in a separate reference file, then still compares the ENTIRE SHA-256.
    $sourceVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo((Resolve-Path -LiteralPath $CandidateBinary).Path).ProductVersion
    if ($sourceVersion -ne $CandidateVersion) { throw 'Candidate PE version differs from the stamped probe version.' }
    $candidateReference = Join-Path $testRoot 'candidate-nsis-reference.exe'
    $oldReference = Join-Path $testRoot 'historical-nsis-reference.exe'
    Initialize-BinaryProbe
    [RollbackProbe.BinaryDiff]::WriteNsisReference((Resolve-Path -LiteralPath $CandidateBinary).Path, $candidateReference)
    $candidateDigest = (Get-FileHash -LiteralPath $candidateReference -Algorithm SHA256).Hash.ToLowerInvariant()

    Run-Setup $oldSetup '/S' 'old fresh silent installation without /R'
    $oldBinaryDigest = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
    Copy-Item -LiteralPath $exe -Destination $oldReference
    Assert-Installed $oldVersion $oldBinaryDigest 'old fresh install'

    # Exercise clean-exit rejection using a harmless process fixture, not a GUI
    # that could initialize a database or take over live client configuration.
    $fixtureDir = Join-Path $testRoot 'process-fixture'
    New-Item -ItemType Directory -Path $fixtureDir | Out-Null
    $fixtureExe = Join-Path $fixtureDir 'cc-switch.exe'
    Copy-Item -LiteralPath "$env:WINDIR\System32\ping.exe" -Destination $fixtureExe
    $fixtureInfo = [Diagnostics.ProcessStartInfo]::new()
    $fixtureInfo.FileName = $fixtureExe
    $fixtureInfo.Arguments = '-t 127.0.0.1'
    $fixtureInfo.UseShellExecute = $false
    $fixtureInfo.CreateNoWindow = $true
    $fixtureInfo.RedirectStandardOutput = $true
    $fixture = [Diagnostics.Process]::Start($fixtureInfo)
    try {
        Start-Sleep -Seconds 2
        Run-Setup $CandidateSetup '/S' 'running process rejects silent install without kill' $true
        if ($fixture.HasExited) { throw 'Installer killed the running process fixture.' }
        Assert-Installed $oldVersion $oldBinaryDigest 'running process rejection preserves installation'
    } finally {
        if (-not $fixture.HasExited) { $fixture.Kill($true); $fixture.WaitForExit() }
        $fixture.Dispose()
        Start-Sleep -Seconds 2
        Get-Event -SourceIdentifier $eventName -ErrorAction SilentlyContinue | Remove-Event -ErrorAction SilentlyContinue
    }

    # The ordinary uninstaller would remove this value. It must survive upgrade.
    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    New-Item -Path $runKey -Force | Out-Null
    $sentinel = 'installer-probe-no-start'
    New-ItemProperty -Path $runKey -Name 'CC Switch' -Value $sentinel -PropertyType String -Force | Out-Null
    Run-Setup $CandidateSetup '/P' 'passive manual upgrade without /UPDATE or /R'
    Assert-Installed $CandidateVersion $candidateDigest 'candidate manual upgrade'
    $preinstall = Get-Content -LiteralPath (Join-Path $installDir 'rollback-probe-preinstall.txt') -Raw
    $preinstall | Set-Content -LiteralPath (Join-Path $EvidenceDirectory 'upgrade-preinstall.txt')
    foreach ($expected in @("DisplayVersion=$oldVersion", "InstallLocation=`"$installDir`"", 'MainExecutable=present', 'Uninstaller=present')) {
        if (-not $preinstall.Contains($expected)) { throw "PREINSTALL did not observe intact old installation: $expected" }
    }
    if ((Get-ItemPropertyValue -Path $runKey -Name 'CC Switch') -ne $sentinel) {
        throw 'Manual upgrade ran the old uninstaller before capture.'
    }

    # Prove a preexisting per-file deny-execute ACL survives an old Setup copy.
    # The helper can use this to guard old shortcut launches until restore ends.
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $acl = Get-Acl -LiteralPath $exe
    $originalSddl = $acl.Sddl
    $rule = [Security.AccessControl.FileSystemAccessRule]::new($sid, [Security.AccessControl.FileSystemRights]::ExecuteFile, [Security.AccessControl.AccessControlType]::Deny)
    $acl.AddAccessRule($rule)
    Set-Acl -LiteralPath $exe -AclObject $acl
    $blockedAcl = $true
    Run-Setup $oldSetup '/P' 'historical passive downgrade under execute guard'
    Assert-Installed $oldVersion $oldBinaryDigest 'historical downgrade'
    $afterAcl = Get-Acl -LiteralPath $exe
    $deny = @($afterAcl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | Where-Object {
        $_.IdentityReference -eq $sid -and $_.AccessControlType -eq 'Deny' -and
        ($_.FileSystemRights -band [Security.AccessControl.FileSystemRights]::ExecuteFile)
    })
    if ($deny.Count -eq 0) { throw 'Old installer replaced the execute guard ACL.' }
    $launchBlocked = $false
    try {
        $start = [Diagnostics.ProcessStartInfo]::new()
        $start.FileName = $exe
        $start.UseShellExecute = $false
        $unexpected = [Diagnostics.Process]::Start($start)
        if ($unexpected) {
            $unexpected.Kill($true)
            $unexpected.Dispose()
        }
    } catch [ComponentModel.Win32Exception] {
        if ($_.Exception.NativeErrorCode -ne 5) { throw }
        $launchBlocked = $true
    }
    if (-not $launchBlocked) { throw 'Old executable launched while restore guard was held.' }
    $report.Add([ordered]@{ stage = 'old shortcut isolation'; denyExecuteSurvived = $true; launchBlocked = $true })
    Save-Report
    $afterAcl.SetSecurityDescriptorSddlForm($originalSddl)
    Set-Acl -LiteralPath $exe -AclObject $afterAcl
    $blockedAcl = $false

    Run-Setup $CandidateSetup '/S' 'candidate silent reinstall after old downgrade'
    Assert-Installed $CandidateVersion $candidateDigest 'candidate restored installation'
    Run-Setup $CandidateSetup '/P' 'candidate same-version passive reinstall'
    Assert-Installed $CandidateVersion $candidateDigest 'candidate same-version reinstall'

    # Explicitly prove legacy silent downgrade works without automatic launch.
    Run-Setup $oldSetup '/S' 'historical silent downgrade without /R'
    Assert-Installed $oldVersion $oldBinaryDigest 'historical silent downgrade'
    $report.Add([ordered]@{ stage = 'installer contract'; result = 'passed'; dataRollbackTested = $false })
    Save-Report
    Write-Output '::notice title=Windows installer contract::Production fork.3 and probe Setup completed silent/passive upgrade, downgrade and reinstall in the original Unicode path without launching the app. The deny-execute guard survived historical Setup and blocked CreateProcess. Data restoration is not yet tested.'
} catch {
    $report.Add([ordered]@{ stage = 'failure'; message = $_.Exception.Message; stack = $_.ScriptStackTrace })
    Save-Report
    Write-ProbeAnnotation 'notice' 'Completed installer evidence' ($report | ConvertTo-Json -Depth 8 -Compress)
    $annotation = ($_.Exception.Message + "`n" + $_.ScriptStackTrace).Replace('%', '%25').Replace("`r", '%0D').Replace("`n", '%0A')
    Write-Output "::error title=Windows installer probe::$annotation"
    throw
} finally {
    if ($blockedAcl -and $originalSddl -and (Test-Path -LiteralPath $exe)) {
        $acl = Get-Acl -LiteralPath $exe
        $acl.SetSecurityDescriptorSddlForm($originalSddl)
        Set-Acl -LiteralPath $exe -AclObject $acl
    }
    if ($watcher) { Unregister-Event -SourceIdentifier $eventName -ErrorAction SilentlyContinue }
    Get-Event -SourceIdentifier $eventName -ErrorAction SilentlyContinue | Remove-Event -ErrorAction SilentlyContinue
}
