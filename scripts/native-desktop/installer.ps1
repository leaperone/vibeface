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
$installed = $false
try {
  $process = Start-Process -FilePath $Installer -ArgumentList ('/S /D=' + $destination) -Wait -PassThru
  if ($process.ExitCode -ne 0) { throw 'The per-user installer failed' }
  $installed = $true
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
  node scripts/e2e-desktop.mjs --app (Join-Path $destination 'Vibeface.exe') --runtime $Runtime --output $Output
  if ($LASTEXITCODE -ne 0) { throw 'The installed native application failed its real-window checks' }
} finally {
  if ($installed) {
    $uninstaller = Join-Path $destination 'Uninstall.exe'
    if (-not (Test-Path $uninstaller)) { throw 'Task-owned installation has no uninstaller' }
    $removed = Start-Process -FilePath $uninstaller -ArgumentList '/S' -Wait -PassThru
    if ($removed.ExitCode -ne 0) { throw 'Task-owned uninstaller failed' }
    if (Test-Path $registration) { throw 'Task-owned per-user registration survived uninstall' }
  }
}
Write-Output 'Current-user install, sealed runtime, native window and uninstall checks passed'
