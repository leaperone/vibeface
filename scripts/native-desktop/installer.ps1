param(
  [Parameter(Mandatory=$true)][string]$Installer,
  [Parameter(Mandatory=$true)][string]$Runtime,
  [Parameter(Mandatory=$true)][string]$PortableApp,
  [Parameter(Mandatory=$true)][string]$Output
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') { throw 'Installer acceptance only changes a disposable GitHub-hosted account' }
$registration = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface'
$machineRegistration = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface'
if ((Test-Path $registration) -or (Test-Path $machineRegistration)) { throw 'A pre-existing product registration cannot be changed by this fixture' }
$destination = Join-Path $env:RUNNER_TEMP ('Vibeface native 安装 ' + [guid]::NewGuid().ToString('N'))
$unowned = Join-Path $env:RUNNER_TEMP ('Vibeface unowned ' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $unowned | Out-Null
$beforeSentinel = Join-Path $unowned 'user-before.txt'
$beforeProgram = Join-Path $unowned 'Vibeface.exe'
[System.IO.File]::WriteAllText($beforeSentinel, 'keep-before-install')
[System.IO.File]::WriteAllText($beforeProgram, 'existing-user-program')
$refused = Start-Process -FilePath $Installer -ArgumentList ('/S /D=' + $unowned) -Wait -PassThru
if ($refused.ExitCode -eq 0) { throw 'Installer accepted a nonempty unowned directory' }
if ([System.IO.File]::ReadAllText($beforeSentinel) -ne 'keep-before-install' -or [System.IO.File]::ReadAllText($beforeProgram) -ne 'existing-user-program') { throw 'Refused installation changed existing user files' }
if (@(Get-ChildItem -Force $unowned).Count -ne 2 -or (Test-Path $registration)) { throw 'Refused installation copied files or created registration' }
$userAfter = Join-Path $destination 'user-after.txt'
$userNested = Join-Path $destination 'user-files\keep.txt'
$ownedPaths = @()
$installed = $false
try {
  $process = Start-Process -FilePath $Installer -ArgumentList ('/S /D=' + $destination) -Wait -PassThru
  if ($process.ExitCode -ne 0) { throw 'The per-user installer failed' }
  $installed = $true
  $manifest = Get-Content -Raw (Join-Path $destination '.vibeface-installed-files.json') | ConvertFrom-Json
  if ($manifest.schema -ne 1 -or $manifest.app -ne 'vibeface-native-desktop' -or $manifest.files.Count -lt 2) { throw 'Installed ownership manifest is invalid' }
  $ownedPaths = @($manifest.files)
  foreach ($relative in $ownedPaths) {
    if ($relative -notmatch '^[a-zA-Z0-9._/-]+$' -or $relative.Split('/') -contains '..') { throw 'Installed ownership manifest has an unsafe path' }
  }
  [System.IO.File]::WriteAllText($userAfter, 'keep-after-install')
  New-Item -ItemType Directory (Split-Path $userNested) | Out-Null
  [System.IO.File]::WriteAllText($userNested, 'keep-nested-user-file')
  $copiedRoot = Join-Path $env:RUNNER_TEMP ('Vibeface copied uninstaller ' + [guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory $copiedRoot | Out-Null
  $copiedSentinel = Join-Path $copiedRoot 'Vibeface.exe'
  [System.IO.File]::WriteAllText($copiedSentinel, 'unowned-copy-sentinel')
  $copiedUninstaller = Join-Path $copiedRoot 'Uninstall.exe'
  Copy-Item (Join-Path $destination 'Uninstall.exe') $copiedUninstaller
  $refused = Start-Process -FilePath $copiedUninstaller -ArgumentList ('/S _?=' + $copiedRoot) -Wait -PassThru
  if ($refused.ExitCode -eq 0 -or [System.IO.File]::ReadAllText($copiedSentinel) -ne 'unowned-copy-sentinel') { throw 'A copied uninstaller touched an unowned directory' }
  if (-not (Test-Path (Join-Path $destination 'Vibeface.exe'))) { throw 'A copied uninstaller modified the real installation' }
  $runtimeDirectory = Join-Path $destination 'resources\runtime'
  $heldRuntime = Join-Path $env:RUNNER_TEMP ('Vibeface held runtime ' + [guid]::NewGuid().ToString('N'))
  $outside = Join-Path $env:RUNNER_TEMP ('Vibeface junction target ' + [guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory $outside | Out-Null
  $outsideSentinel = Join-Path $outside 'outside-user-file.txt'
  [System.IO.File]::WriteAllText($outsideSentinel, 'never-traverse-junction')
  Move-Item $runtimeDirectory $heldRuntime
  $junctionCreated = $false
  try {
    New-Item -ItemType Junction -Path $runtimeDirectory -Target $outside | Out-Null
    $junctionCreated = $true
    $refused = Start-Process -FilePath (Join-Path $destination 'Uninstall.exe') -ArgumentList ('/S _?=' + $destination) -Wait -PassThru
    if ($refused.ExitCode -eq 0) { throw 'Uninstaller accepted a reparse point in an owned payload path' }
    $refused = Start-Process -FilePath $Installer -ArgumentList ('/S /D=' + $destination) -Wait -PassThru
    if ($refused.ExitCode -eq 0) { throw 'Installer accepted a reparse point in an owned payload path' }
    if ([System.IO.File]::ReadAllText($outsideSentinel) -ne 'never-traverse-junction' -or -not (Test-Path (Join-Path $destination 'Vibeface.exe'))) { throw 'Refused junction traversal changed user or installed files' }
  } finally {
    if ($junctionCreated) { [System.IO.Directory]::Delete($runtimeDirectory) }
    Move-Item $heldRuntime $runtimeDirectory
  }
  $upgraded = Start-Process -FilePath $Installer -ArgumentList ('/S /D=' + $destination) -Wait -PassThru
  if ($upgraded.ExitCode -ne 0) { throw 'An owned upgrade failed' }
  if ([System.IO.File]::ReadAllText($userAfter) -ne 'keep-after-install' -or [System.IO.File]::ReadAllText($userNested) -ne 'keep-nested-user-file') { throw 'Owned upgrade changed extra user files' }
  if (Test-Path $machineRegistration) { throw 'Installer unexpectedly wrote machine-wide registration' }
  $receipt = Get-ItemProperty $registration
  if (-not $receipt.UninstallString.Contains($destination)) { throw 'Uninstall registration is not bound to the task-owned installation' }
  $activation = (Get-ItemProperty 'HKCU:\Software\Classes\vibeface\shell\open\command').'(default)'
  $expectedActivation = '"' + (Join-Path $destination 'Vibeface.exe') + '" "%1"'
  if ($activation -ne $expectedActivation) { throw 'Per-user deep-link registration does not forward the URL to this installation' }
  foreach ($program in @('vibeface.exe', 'vibeface-core.exe')) {
    $original = Get-FileHash -Algorithm SHA256 (Join-Path $Runtime $program)
    $copy = Get-FileHash -Algorithm SHA256 (Join-Path $destination ('resources\runtime\' + $program))
    if ($original.Hash -ne $copy.Hash) { throw 'Installer changed the sealed native runtime' }
  }
  if ((Get-FileHash -Algorithm SHA256 $PortableApp).Hash -ne (Get-FileHash -Algorithm SHA256 (Join-Path $destination 'Vibeface.exe')).Hash) { throw 'Installer native executable differs from portable ZIP' }
  foreach ($line in Get-Content (Join-Path $Runtime 'FILES.sha256')) {
    if ($line -notmatch '^([a-f0-9]{64})  ([a-zA-Z0-9._/-]+)$') { throw 'Invalid original runtime seal' }
    $expected = $Matches[1]
    $relative = $Matches[2]
    if ($relative.Split('/') -contains '..') { throw 'Unsafe original runtime seal' }
    $copy = Join-Path $destination ('resources\runtime\' + $relative)
    if ((Get-FileHash -Algorithm SHA256 $copy).Hash.ToLowerInvariant() -ne $expected) { throw 'Installer changed a sealed runtime file' }
  }
  $uninstaller = Join-Path $destination 'Uninstall.exe'
  $signature = Get-AuthenticodeSignature $Installer
  if ($signature.Status -eq 'Valid') {
    node scripts/windows-signing.mjs verify $uninstaller
    if ($LASTEXITCODE -ne 0) { throw 'Signed installer has no verified signed uninstaller' }
  }
  $previousApp = $env:VIBEFACE_E2E_DESKTOP_APP
  $previousRuntime = $env:VIBEFACE_E2E_DESKTOP_RUNTIME
  $previousOutput = $env:VIBEFACE_E2E_DESKTOP_OUTPUT
  $previousTelemetry = $env:E2E_TELEMETRY_DISABLED
  try {
    $env:VIBEFACE_E2E_DESKTOP_APP = Join-Path $destination 'Vibeface.exe'
    $env:VIBEFACE_E2E_DESKTOP_RUNTIME = $Runtime
    $env:VIBEFACE_E2E_DESKTOP_OUTPUT = $Output
    $env:E2E_TELEMETRY_DISABLED = '1'
    pnpm exec e2e run --config tests/e2e/e2e.native-desktop.config.ts --output (Join-Path $PWD 'tests/e2e/.artifacts/native-desktop/installed-runner')
    if ($LASTEXITCODE -ne 0) { throw 'The installed native application failed its real-window e2e checks' }
  } finally {
    $env:VIBEFACE_E2E_DESKTOP_APP = $previousApp
    $env:VIBEFACE_E2E_DESKTOP_RUNTIME = $previousRuntime
    $env:VIBEFACE_E2E_DESKTOP_OUTPUT = $previousOutput
    $env:E2E_TELEMETRY_DISABLED = $previousTelemetry
  }
} finally {
  if ($installed) {
    $uninstaller = Join-Path $destination 'Uninstall.exe'
    if (-not (Test-Path $uninstaller)) { throw 'Task-owned installation has no uninstaller' }
    $removed = Start-Process -FilePath $uninstaller -ArgumentList '/S' -Wait -PassThru
    if ($removed.ExitCode -ne 0) { throw 'Task-owned uninstaller failed' }
    if (Test-Path $registration) { throw 'Task-owned per-user registration survived uninstall' }
    foreach ($relative in $ownedPaths) {
      if (Test-Path (Join-Path $destination $relative)) { throw 'An owned payload file survived uninstall' }
    }
    if ([System.IO.File]::ReadAllText($userAfter) -ne 'keep-after-install' -or [System.IO.File]::ReadAllText($userNested) -ne 'keep-nested-user-file') { throw 'Uninstall changed extra user files' }
    if (Test-Path (Join-Path $destination '.vibeface-install-owned')) { throw 'Ownership marker survived uninstall' }
  }
}
Write-Output 'Current-user install, unowned-root refusal, copied-uninstaller refusal, junction refusal, owned upgrade, sealed runtime, native window and user-file-preserving uninstall checks passed'
