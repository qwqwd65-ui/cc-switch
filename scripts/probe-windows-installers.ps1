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
    $process = [Diagnostics.Process]::Start($info)
    if (-not $process.WaitForExit(180000)) {
        $process.Kill($true)
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
    if ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Digest) {
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
    Invoke-WebRequest -Uri $oldUrl -OutFile $oldSetup
    if ((Get-FileHash -LiteralPath $oldSetup -Algorithm SHA256).Hash.ToLowerInvariant() -ne $oldDigest) {
        throw 'Historical production Setup differs from the pinned GitHub asset digest.'
    }
    $candidateDigest = (Get-FileHash -LiteralPath $CandidateBinary -Algorithm SHA256).Hash.ToLowerInvariant()

    Run-Setup $oldSetup '/S' 'old fresh silent installation without /R'
    $oldBinaryDigest = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
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
