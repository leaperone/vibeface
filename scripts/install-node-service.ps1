# Runs this account's Vibeface Node in the background through the Windows task scheduler.
#
# The tasks run as the account that installs them, with its own interactive token, so nothing
# here needs administrator rights and nothing runs for another account. Uninstall keeps all
# node data.

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$NodeTask = '\Vibeface\Node'
$UpdateTask = '\Vibeface\Update'

function Show-Usage {
    @'
Usage: install-node-service.ps1 install --binary C:\absolute\vibeface.exe --data-dir C:\absolute\data
       install-node-service.ps1 install-updater --binary C:\install-root\current\vibeface.exe --data-dir C:\absolute\data
       install-node-service.ps1 start|stop|restart [--data-dir C:\absolute\data]
       install-node-service.ps1 status
       install-node-service.ps1 uninstall-updater|uninstall --data-dir C:\absolute\data

Installs scheduled tasks for the account that runs this script. Initialize the node first.
'@
}

function Fail {
    param([string] $Message)
    [Console]::Error.WriteLine("Vibeface node service: $Message")
    exit 1
}

function Fail-Usage {
    param([string] $Message)
    [Console]::Error.WriteLine("Vibeface node service: $Message")
    [Console]::Error.WriteLine((Show-Usage))
    exit 2
}

function Get-Scheduler {
    $root = $env:SystemRoot
    if (-not $root) { Fail 'SystemRoot is required to reach the task scheduler.' }
    $path = Join-Path $root 'System32\schtasks.exe'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        Fail "the task scheduler is required: $path"
    }
    return $path
}

function Invoke-Scheduler {
    param([string[]] $Arguments)
    $output = & (Get-Scheduler) @Arguments 2>&1
    return [pscustomobject]@{ Code = $LASTEXITCODE; Output = ($output | Out-String) }
}

# Return the registered task, treating only the scheduler's missing-task/path codes as absent.
function Get-RegisteredTask {
    param([string] $Name)
    try {
        $scheduler = New-Object -ComObject 'Schedule.Service'
        $scheduler.Connect()
        return $scheduler.GetFolder('\').GetTask($Name)
    } catch {
        $failure = $_.Exception
        while ($failure.InnerException) { $failure = $failure.InnerException }
        # The local Task Scheduler API distinguishes a missing task/path from query denial.
        if ($failure.HResult -eq -2147024894 -or $failure.HResult -eq -2147024893) { return $null }
        Fail "could not inspect task $Name (HRESULT $($failure.HResult)); it was preserved."
    }
}

function Get-TaskCommandLine {
    param([string] $Name)
    $task = Get-RegisteredTask -Name $Name
    if ($null -eq $task) { return $null }
    try { $document = [xml] $task.Xml }
    catch { Fail "could not read task $Name; it was preserved." }
    $actions = @($document.Task.Actions.ChildNodes)
    if ($actions.Count -ne 1 -or $actions[0].LocalName -ne 'Exec') {
        Fail "task $Name has unsupported actions; it was preserved."
    }
    $exec = $document.Task.Actions.Exec
    if (-not $exec) { Fail "the registered task $Name runs no program." }
    $line = [string] $exec.Command
    if ($exec.Arguments) { $line = "$line " + [string] $exec.Arguments }
    return $line
}

function Confirm-TaskData {
    param([string] $Name, [string] $DataDir, [string] $Verb, [bool] $Anchored)
    if (-not $DataDir) { return }
    $line = Get-TaskCommandLine -Name $Name
    if ($null -eq $line) { return }
    $quoted = "--data-dir `"$DataDir`""
    $place = $line.IndexOf($quoted)
    $matched = $place -ge 0 -and
        (([regex]::Matches($line, [regex]::Escape('--data-dir'))).Count -eq 1) -and
        $line.Substring(0, $place).Contains(" $Verb ") -and
        ((-not $Anchored) -or $line.EndsWith($quoted))
    if (-not $matched) {
        Fail 'installed service uses a different data directory or unsupported arguments; no service was changed.'
    }
}

function Resolve-DataDir {
    param([string] $Value)
    if (-not [System.IO.Path]::IsPathRooted($Value)) { Fail-Usage 'An initialized absolute --data-dir is required.' }
    $resolved = $null
    try { $resolved = (Get-Item -LiteralPath $Value -Force).FullName } catch { }
    if (-not $resolved) { Fail-Usage 'An initialized absolute --data-dir is required.' }
    foreach ($name in @('config.json', 'identity.json')) {
        if (-not (Test-Path -LiteralPath (Join-Path $resolved $name) -PathType Leaf)) {
            Fail-Usage 'An initialized absolute --data-dir is required.'
        }
    }
    return $resolved.TrimEnd('\')
}

# A task definition the scheduler accepts, holding one action and one trigger.
function New-Definition {
    param(
        [string] $Description,
        [string] $Command,
        [string] $Arguments,
        [string] $WorkingDirectory,
        [string] $Trigger
    )
    $escape = { param($text) [System.Security.SecurityElement]::Escape($text) }
    @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>$(& $escape $Description)</Description>
  </RegistrationInfo>
  <Triggers>
$Trigger
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>$(& $escape $Command)</Command>
      <Arguments>$(& $escape $Arguments)</Arguments>
      <WorkingDirectory>$(& $escape $WorkingDirectory)</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"@
}

function Register-Definition {
    param([string] $Name, [string] $Definition)
    $file = Join-Path ([System.IO.Path]::GetTempPath()) ("vibeface-task-" + [guid]::NewGuid().ToString('N') + '.xml')
    try {
        # The scheduler reads a definition as UTF-16, which is also what it writes.
        [System.IO.File]::WriteAllText($file, $Definition, [System.Text.Encoding]::Unicode)
        $result = Invoke-Scheduler @('/Create', '/TN', $Name, '/XML', $file, '/F')
        if ($result.Code -ne 0) { Fail "could not register $Name`n$($result.Output)" }
    } finally {
        Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
    }
}

# TASK_STATE_RUNNING is the scheduler's current state, independent of display language and
# the result code of an earlier run.
function Test-TaskRunning {
    param([string] $Name)
    $task = Get-RegisteredTask -Name $Name
    return $null -ne $task -and [int] $task.State -eq 4
}

function Stop-Task {
    param([string] $Name)
    if (Test-TaskRunning -Name $Name) {
        $result = Invoke-Scheduler @('/End', '/TN', $Name)
        if ($result.Code -ne 0) { Fail "could not stop $Name`n$($result.Output)" }
    }
}

function Start-Task {
    param([string] $Name)
    if (Test-TaskRunning -Name $Name) { return }
    $result = Invoke-Scheduler @('/Run', '/TN', $Name)
    if ($result.Code -ne 0) { Fail "could not start $Name`n$($result.Output)" }
}

function Remove-Task {
    param([string] $Name)
    if ($null -eq (Get-RegisteredTask -Name $Name)) { return }
    Stop-Task -Name $Name
    $result = Invoke-Scheduler @('/Delete', '/TN', $Name, '/F')
    if ($result.Code -ne 0) { Fail "could not remove $Name`n$($result.Output)" }
    if ($null -ne (Get-RegisteredTask -Name $Name)) {
        Fail "task $Name is still registered after removal."
    }
}

# A private log this account alone can read, for the updater's own output.
function New-PrivateLog {
    param([string] $Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        New-Item -ItemType File -Path $Path -Force | Out-Null
    }
    $account = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $list = Get-Acl -LiteralPath $Path
    $list.SetAccessRuleProtection($true, $false)
    foreach ($rule in @($list.Access)) { $list.RemoveAccessRuleSpecific($rule) }
    $list.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        $account, 'FullControl', 'None', 'None', 'Allow')))
    Set-Acl -LiteralPath $Path -AclObject $list
}

$arguments = @($args)
if ($arguments.Count -eq 0) { Show-Usage; exit 0 }
$action = $arguments[0]
if ($action -eq '--help' -or $action -eq '-h') { Show-Usage; exit 0 }
if (@('install', 'install-updater', 'start', 'stop', 'restart', 'status', 'uninstall-updater', 'uninstall') -notcontains $action) {
    Fail-Usage "unknown action: $action"
}
$binary = ''
$dataDir = ''
$seen = @{}
$index = 1
while ($index -lt $arguments.Count) {
    $name = $arguments[$index]
    if (($name -ne '--binary' -and $name -ne '--data-dir') -or ($index + 1) -ge $arguments.Count) {
        Fail-Usage "unknown argument: $name"
    }
    if ($seen.ContainsKey($name)) { Fail-Usage "duplicate argument: $name" }
    $seen[$name] = $true
    if ($name -eq '--binary') { $binary = $arguments[$index + 1] } else { $dataDir = $arguments[$index + 1] }
    $index += 2
}
if ($dataDir) { $dataDir = Resolve-DataDir -Value $dataDir }
if ($binary -and $action -notin @('install', 'install-updater')) {
    Fail-Usage '--binary is only valid when installing a task.'
}
if ($action -ne 'status' -and -not $dataDir) {
    Fail-Usage 'An initialized absolute --data-dir is required for this action.'
}

if ($action -eq 'install' -or $action -eq 'install-updater') {
    if (-not $binary -or -not [System.IO.Path]::IsPathRooted($binary) -or
        -not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        Fail-Usage 'An absolute --binary that exists is required.'
    }
    if (-not $dataDir) { Fail-Usage 'An initialized absolute --data-dir is required.' }
    $binary = (Get-Item -LiteralPath $binary -Force).FullName
}

switch ($action) {
    'start' { Confirm-TaskData -Name $NodeTask -DataDir $dataDir -Verb 'serve' -Anchored $true }
    'stop' { Confirm-TaskData -Name $NodeTask -DataDir $dataDir -Verb 'serve' -Anchored $true }
    'restart' { Confirm-TaskData -Name $NodeTask -DataDir $dataDir -Verb 'serve' -Anchored $true }
    'uninstall-updater' { Confirm-TaskData -Name $UpdateTask -DataDir $dataDir -Verb 'apply' -Anchored $false }
    default {
        Confirm-TaskData -Name $NodeTask -DataDir $dataDir -Verb 'serve' -Anchored $true
        Confirm-TaskData -Name $UpdateTask -DataDir $dataDir -Verb 'apply' -Anchored $false
    }
}

switch ($action) {
    'install' {
        $trigger = @'
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
'@
        $definition = New-Definition `
            -Description 'Vibeface personal node' `
            -Command $binary `
            -Arguments "serve --data-dir `"$dataDir`"" `
            -WorkingDirectory (Split-Path -LiteralPath $binary) `
            -Trigger $trigger
        Register-Definition -Name $NodeTask -Definition $definition
        Stop-Task -Name $NodeTask
        Start-Task -Name $NodeTask
        "Installed $NodeTask"
    }
    'install-updater' {
        $root = $env:SystemRoot
        if (-not $root) { Fail 'SystemRoot is required to install the updater.' }
        $shell = Join-Path $root 'System32\cmd.exe'
        New-PrivateLog -Path (Join-Path $dataDir 'update.stdout.log')
        New-PrivateLog -Path (Join-Path $dataDir 'update.stderr.log')
        # The scheduler cannot redirect a task's output, so the updater's own output is
        # appended to the same private logs the other families write.
        $redirected = '/c ""' + $binary + '" update apply --automatic --data-dir "' + $dataDir +
            '" >> "' + (Join-Path $dataDir 'update.stdout.log') + '" 2>> "' +
            (Join-Path $dataDir 'update.stderr.log') + '""'
        $trigger = @'
    <TimeTrigger>
      <Repetition>
        <Interval>PT1H</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <StartBoundary>2026-01-01T00:00:00</StartBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
'@
        $definition = New-Definition `
            -Description 'Update Vibeface personal node' `
            -Command $shell `
            -Arguments $redirected `
            -WorkingDirectory (Split-Path -LiteralPath $binary) `
            -Trigger $trigger
        Register-Definition -Name $UpdateTask -Definition $definition
        "Installed $UpdateTask"
    }
    'start' {
        if ($null -eq (Get-TaskCommandLine -Name $NodeTask)) { Fail 'Installed node service is required.' }
        Start-Task -Name $NodeTask
    }
    'stop' {
        if ($null -eq (Get-TaskCommandLine -Name $NodeTask)) { Fail 'Installed node service is required.' }
        Stop-Task -Name $NodeTask
    }
    'restart' {
        if ($null -eq (Get-TaskCommandLine -Name $NodeTask)) { Fail 'Installed node service is required.' }
        Stop-Task -Name $NodeTask
        Start-Task -Name $NodeTask
    }
    'status' {
        $node = Invoke-Scheduler @('/Query', '/TN', $NodeTask, '/V', '/FO', 'LIST')
        $node.Output
        $updater = Invoke-Scheduler @('/Query', '/TN', $UpdateTask, '/V', '/FO', 'LIST')
        if ($updater.Code -eq 0) { $updater.Output } else { 'Updater is not installed.' }
        exit $node.Code
    }
    'uninstall' {
        Remove-Task -Name $UpdateTask
        Remove-Task -Name $NodeTask
        'Removed requested services; node data was kept.'
    }
    'uninstall-updater' {
        Remove-Task -Name $UpdateTask
        'Removed requested services; node data was kept.'
    }
}
