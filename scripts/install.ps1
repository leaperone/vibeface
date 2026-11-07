# Installs the latest published Vibeface release for Windows x64.
#
# No account, provider, node data, background service, or automatic update is configured. An
# existing installation is left alone: use vibeface update apply instead. Nothing here needs
# administrator rights or developer mode.

[CmdletBinding()]
param(
    [ValidateSet('stable', 'nightly')]
    [string] $Channel = 'stable',
    [string] $InstallRoot,
    [string] $BinDir,
    [switch] $SelfTest
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Target = 'x86_64-pc-windows-msvc'
$Api = 'https://api.github.com/repos/leaperone/vibeface'
$Downloads = 'https://github.com/leaperone/vibeface/releases/download'
$MaxJson = 4MB
$MaxArchive = 128MB
$MaxUnpacked = 512MB
$MaxEntries = 4096

function Fail {
    param([string] $Message)
    [Console]::Error.WriteLine("Vibeface installer: $Message")
    exit 1
}

# The release tags of one channel, newest last. A tag of another product, a draft, or a tag
# whose shape is not exactly this project's is left out rather than guessed at.
function Select-Release {
    param([object[]] $Releases, [string] $Channel)
    $wanted = $Channel -eq 'nightly'
    $ordered = @()
    foreach ($release in $Releases) {
        if ($release.draft -ne $false) { continue }
        if ($release.prerelease -ne $wanted) { continue }
        $tag = [string] $release.tag_name
        $shape = if ($wanted) {
            '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-nightly\.([0-9]{8})\.([1-9][0-9]*)$'
        } else {
            '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
        }
        $found = [regex]::Match($tag, $shape)
        if (-not $found.Success) { continue }
        $day = 0
        $build = 0
        if ($wanted) {
            $stamp = $found.Groups[4].Value
            $parsed = [datetime]::MinValue
            if (-not [datetime]::TryParseExact($stamp, 'yyyyMMdd', [cultureinfo]::InvariantCulture,
                    [System.Globalization.DateTimeStyles]::None, [ref] $parsed)) {
                continue
            }
            $day = [int64] $stamp
            $build = [int64] $found.Groups[5].Value
        }
        $ordered += [pscustomobject]@{
            Tag = $tag
            Major = [int64] $found.Groups[1].Value
            Minor = [int64] $found.Groups[2].Value
            Patch = [int64] $found.Groups[3].Value
            Day = $day
            Build = $build
        }
    }
    return ($ordered | Sort-Object Major, Minor, Patch, Day, Build)
}

function Get-Json {
    param([string] $Url)
    $file = Join-Path $script:Scratch ('json-' + [guid]::NewGuid().ToString('N'))
    Get-Download -Url $Url -Path $file -Limit $MaxJson -Accept 'application/vnd.github+json'
    return (Get-Content -LiteralPath $file -Raw | ConvertFrom-Json)
}

function Get-Download {
    param([string] $Url, [string] $Path, [int64] $Limit, [string] $Accept = '*/*')
    if (-not $Url.StartsWith('https://')) { Fail "refusing a download that is not over HTTPS: $Url" }
    try {
        Invoke-WebRequest -Uri $Url -OutFile $Path -UseBasicParsing -MaximumRedirection 5 `
            -Headers @{ Accept = $Accept } -UserAgent 'vibeface-installer' -TimeoutSec 180
    } catch {
        Fail "download failed (network or GitHub API limit): $Url"
    }
    if ((Get-Item -LiteralPath $Path).Length -gt $Limit) { Fail "download exceeds its size limit: $Url" }
}

function Get-Sha256 {
    param([string] $Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-Tar {
    $root = $env:SystemRoot
    if (-not $root) { Fail 'SystemRoot is required to unpack a release.' }
    $path = Join-Path $root 'System32\tar.exe'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        Fail "unpacking a release needs Windows tar: $path"
    }
    return $path
}

# Refuses an archive before any of it is written out: an unexpected root, a path that could
# leave it, a duplicate, a link, or more content than an installation may hold.
function Confirm-Archive {
    param([string] $Archive, [string] $Package)
    $listing = & (Get-Tar) '-tvzf' $Archive
    if ($LASTEXITCODE -ne 0) { Fail 'invalid tar archive' }
    $lines = @($listing)
    if ($lines.Count -eq 0 -or $lines.Count -gt $MaxEntries) { Fail 'archive is empty or has too many entries' }
    $seen = @{}
    $total = [int64] 0
    foreach ($line in $lines) {
        $kind = $line.Substring(0, 1)
        if ($kind -ne '-' -and $kind -ne 'd') { Fail 'archive contains links or special files' }
        # Windows carries one archive reader, which prints the size five fields from the end and
        # the name last. A listing shaped any other way is refused instead of guessed at.
        $fields = -split $line
        if ($fields.Count -lt 7) { Fail 'cannot inspect archive members' }
        $size = $fields[$fields.Count - 5]
        if ($size -notmatch '^[0-9]+$') { Fail 'cannot inspect archive members' }
        $total += [int64] $size
        $path = $fields[$fields.Count - 1].TrimEnd('/')
        if ($path -notmatch '^[a-zA-Z0-9._/-]+$') { Fail "unsafe archive path: $path" }
        if ($path -ne $Package -and -not $path.StartsWith("$Package/")) { Fail "unexpected archive root: $path" }
        foreach ($part in $path.Split('/')) {
            if ($part -eq '' -or $part -eq '.' -or $part -eq '..') { Fail "unsafe archive path: $path" }
        }
        if ($seen.ContainsKey($path)) { Fail "duplicate archive path: $path" }
        $seen[$path] = $true
    }
    if ($total -gt $MaxUnpacked) { Fail 'expanded archive exceeds its size limit' }
}

# Refuses a package that is not exactly the release it claims to be.
function Confirm-Package {
    param([string] $Package, [string] $Version, [string] $Commit)
    $required = @(
        'vibeface.exe', 'vibeface-core.exe', 'BUILD-INFO.json', 'README.md',
        'RUNTIME-REQUIREMENTS.md', 'scripts\install-node-service.ps1', 'docs\node.md',
        'docs\releasing.md', 'web\_shell.html', 'web\index.html', 'web\_redirects',
        'web\_headers', 'web\.vibeface-build.json', 'web\vibeface_secure.js',
        'web\vibeface_secure_bg.wasm'
    )
    foreach ($entry in $required) {
        $path = Join-Path $Package $entry
        if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or
            (Get-Item -LiteralPath $path).Length -eq 0) {
            Fail "package is missing $entry"
        }
    }
    $checksums = Join-Path $Package 'FILES.sha256'
    if (-not (Test-Path -LiteralPath $checksums -PathType Leaf)) { Fail 'package is missing FILES.sha256' }
    $declared = @{}
    foreach ($line in (Get-Content -LiteralPath $checksums)) {
        if (-not $line.Trim()) { continue }
        $found = [regex]::Match($line, '^([a-f0-9]{64})  ([a-zA-Z0-9._/-]+)$')
        if (-not $found.Success) { Fail 'invalid file checksum entry' }
        $name = $found.Groups[2].Value
        if ($name -eq 'FILES.sha256' -or $name.StartsWith('/') -or
            "/$name/".Contains('/../') -or "/$name/".Contains('/./') -or "/$name/".Contains('//')) {
            Fail 'unsafe file checksum path'
        }
        if ($declared.ContainsKey($name)) { Fail 'duplicate file checksums' }
        $declared[$name] = $found.Groups[1].Value
    }
    if ($declared.Count -eq 0) { Fail 'package declares no file checksums' }
    foreach ($name in $declared.Keys) {
        $path = Join-Path $Package ($name -replace '/', '\')
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { Fail "package is missing $name" }
        if ((Get-Sha256 -Path $path) -ne $declared[$name]) { Fail 'package file checksum mismatch' }
    }
    $present = @(Get-ChildItem -LiteralPath $Package -Recurse -File -Force |
        Where-Object { $_.Name -ne 'FILES.sha256' })
    if ($present.Count -ne $declared.Count) { Fail 'file checksum coverage mismatch' }
    $build = Get-Content -LiteralPath (Join-Path $Package 'BUILD-INFO.json') -Raw | ConvertFrom-Json
    $fields = @($build.PSObject.Properties.Name)
    $expected = @('version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
        'core_version', 'core_source_commit', 'app_version', 'app_source_commit')
    if ($fields.Count -ne $expected.Count) { Fail 'BUILD-INFO does not declare exactly this release format' }
    foreach ($name in $expected) {
        if ($fields -notcontains $name) { Fail "BUILD-INFO is missing $name" }
    }
    if ($build.version -ne $Version -or $build.target -ne $Target -or
        $build.source_commit -ne $Commit -or $build.update_protocol -ne 1 -or
        $build.data_epoch -ne 1 -or $build.core_version -ne $Version -or
        $build.app_version -ne $Version -or $build.core_source_commit -ne $Commit -or
        $build.app_source_commit -ne $Commit) {
        Fail 'BUILD-INFO does not match the release, platform, source commit, or data epoch'
    }
    $reported = & (Join-Path $Package 'vibeface.exe') '--version'
    if ($LASTEXITCODE -ne 0 -or "$reported".Trim() -ne "vibeface $Version") {
        Fail 'packaged CLI version does not match the release'
    }
    $reported = & (Join-Path $Package 'vibeface-core.exe') '--version'
    if ($LASTEXITCODE -ne 0 -or "$reported".Trim() -ne "vibeface-core $($build.core_version)") {
        Fail 'packaged Core version does not match the release'
    }
}

# The shim a person runs. It hands over to whatever `current` points at, so an update that
# repoints the link is picked up without rewriting anything on the search path.
function Write-Shim {
    param([string] $Path, [string] $Program)
    $body = "@echo off`r`n`"$Program`" %*`r`n"
    [System.IO.File]::WriteAllText($Path, $body, (New-Object System.Text.UTF8Encoding($false)))
}

function Install-Release {
    param([string] $Channel, [string] $InstallRoot, [string] $BinDir)
    if (-not $InstallRoot -or -not $BinDir) { Fail 'Specify -InstallRoot and -BinDir explicitly.' }
    foreach ($directory in @($InstallRoot, $BinDir)) {
        if (-not [System.IO.Path]::IsPathRooted($directory) -or $directory.Contains("`n") -or
            $directory.Contains("`r")) {
            Fail 'installation directories must be absolute paths without line breaks'
        }
    }
    $current = Join-Path $InstallRoot 'current'
    $shim = Join-Path $BinDir 'vibeface.cmd'
    if (Test-Path -LiteralPath $current) { Fail 'current already exists; use vibeface update apply' }
    if (Test-Path -LiteralPath $shim) {
        Fail 'vibeface already exists in the bin directory; use update apply for a managed installation'
    }

    $releases = @()
    $complete = $false
    for ($page = 1; $page -le 20; $page++) {
        $batch = @(Get-Json -Url "$Api/releases?per_page=100&page=$page")
        $releases += $batch
        if ($batch.Count -lt 100) { $complete = $true; break }
    }
    if (-not $complete) { Fail 'release list exceeds 20 pages; cannot select the latest version safely' }
    $selected = @(Select-Release -Releases $releases -Channel $Channel)
    if ($selected.Count -eq 0) {
        if ($Channel -eq 'stable') {
            Fail 'no published Stable release; pass -Channel nightly explicitly to install a preview'
        }
        Fail 'no published Nightly release'
    }
    $tag = $selected[-1].Tag
    $version = $tag.Substring(1)
    $package = "vibeface-$version-$Target"
    $archive = "$package.tar.gz"
    $commit = [string] (Get-Json -Url "$Api/commits/$tag").sha
    if ($commit -notmatch '^[0-9a-f]{40}$') { Fail 'release tag has no valid source commit' }

    "Downloading $tag ($Target)..."
    $archivePath = Join-Path $script:Scratch $archive
    Get-Download -Url "$Downloads/$tag/$archive" -Path $archivePath -Limit $MaxArchive
    $checksumPath = Join-Path $script:Scratch 'checksum'
    Get-Download -Url "$Downloads/$tag/$archive.sha256" -Path $checksumPath -Limit 4096
    $lines = @(Get-Content -LiteralPath $checksumPath | Where-Object { $_.Trim() })
    if ($lines.Count -ne 1) { Fail 'checksum must name exactly the downloaded archive' }
    $found = [regex]::Match($lines[0], '^([a-fA-F0-9]{64})  \*?(\S+)$')
    if (-not $found.Success -or $found.Groups[2].Value -ne $archive) {
        Fail 'checksum must name exactly the downloaded archive'
    }
    if ((Get-Sha256 -Path $archivePath) -ne $found.Groups[1].Value.ToLowerInvariant()) {
        Fail 'SHA256 mismatch; nothing was installed'
    }
    Confirm-Archive -Archive $archivePath -Package $package
    $unpack = Join-Path $script:Scratch 'unpack'
    New-Item -ItemType Directory -Path $unpack | Out-Null
    & (Get-Tar) '-xzf' $archivePath '-C' $unpack
    if ($LASTEXITCODE -ne 0) { Fail 'cannot unpack release' }
    $unpacked = Join-Path $unpack $package
    Confirm-Package -Package $unpacked -Version $version -Commit $commit

    $releasesDir = Join-Path $InstallRoot 'releases'
    New-Item -ItemType Directory -Path $releasesDir -Force | Out-Null
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    $InstallRoot = (Get-Item -LiteralPath $InstallRoot -Force).FullName
    $BinDir = (Get-Item -LiteralPath $BinDir -Force).FullName
    $current = Join-Path $InstallRoot 'current'
    $shim = Join-Path $BinDir 'vibeface.cmd'
    $lock = Join-Path $InstallRoot '.install-lock'
    try {
        New-Item -ItemType Directory -Path $lock -ErrorAction Stop | Out-Null
    } catch {
        Fail 'another installer is active; no changes made'
    }
    $destination = Join-Path $releasesDir $version
    $staged = $false
    $linked = $false
    try {
        if (Test-Path -LiteralPath $current) { Fail 'installation appeared during download; use update apply' }
        if (Test-Path -LiteralPath $shim) { Fail 'installation appeared during download; use update apply' }
        if (Test-Path -LiteralPath $destination) {
            Fail 'release directory already exists; it was preserved'
        }
        Move-Item -LiteralPath $unpacked -Destination $destination
        $staged = $true
        # A junction, not a symbolic link: this account may create one without developer mode
        # or administrator rights, and an update repoints it in place.
        New-Item -ItemType Junction -Path $current -Target $destination | Out-Null
        $linked = $true
        Write-Shim -Path $shim -Program (Join-Path $current 'vibeface.exe')
        $staged = $false
        $linked = $false
    } finally {
        if ($linked) { Remove-Item -LiteralPath $current -Force -Recurse -ErrorAction SilentlyContinue }
        if ($staged) { Remove-Item -LiteralPath $destination -Force -Recurse -ErrorAction SilentlyContinue }
        Remove-Item -LiteralPath $lock -Force -Recurse -ErrorAction SilentlyContinue
    }

    "Installed $tag at $shim"
    'No node data or services were created. Start your local node:'
    "  $shim"
    'After initialization, select its update channel:'
    "  $shim update configure --install-root `"$InstallRoot`" --channel $Channel --automatic false"
    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $path -or ($path -split ';' | Where-Object { $_.TrimEnd('\') -eq $BinDir.TrimEnd('\') }).Count -eq 0) {
        'Add your bin directory to this account''s search path, or use the full path above:'
        "  [Environment]::SetEnvironmentVariable('Path', `"$BinDir;`" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
    }
}

# Checks the parts that need no network: tag selection, archive refusal, and the shim.
function Invoke-SelfTest {
    $stable = @(
        [pscustomobject]@{ tag_name = 'v1.9.0'; draft = $false; prerelease = $false },
        [pscustomobject]@{ tag_name = 'v1.10.0'; draft = $false; prerelease = $false },
        [pscustomobject]@{ tag_name = 'v999.0.0'; draft = $true; prerelease = $false },
        [pscustomobject]@{ tag_name = 'cli-v999.0.0'; draft = $false; prerelease = $false },
        [pscustomobject]@{ tag_name = 'v01.2.3'; draft = $false; prerelease = $false },
        [pscustomobject]@{ tag_name = 'v999.0.0-rc.1'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'v0.0.1-nightly.20261017.2'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'v0.0.1-nightly.20261017.10'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'v0.0.1-nightly.20260229.1'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'www-v999.0.0-nightly.20261017.99'; draft = $false; prerelease = $true }
    )
    $picked = @(Select-Release -Releases $stable -Channel 'stable')
    if ($picked[-1].Tag -ne 'v1.10.0') { Fail "self-test: stable selection picked $($picked[-1].Tag)" }
    $picked = @(Select-Release -Releases $stable -Channel 'nightly')
    if ($picked[-1].Tag -ne 'v0.0.1-nightly.20261017.10') {
        Fail "self-test: nightly selection picked $($picked[-1].Tag)"
    }
    foreach ($entry in $picked) {
        if ($entry.Tag -eq 'v0.0.1-nightly.20260229.1') { Fail 'self-test: accepted a date that does not exist' }
    }
    $directory = Join-Path ([System.IO.Path]::GetTempPath()) ('vibeface-installer-test-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $directory | Out-Null
    try {
        $shim = Join-Path $directory 'vibeface.cmd'
        Write-Shim -Path $shim -Program 'C:\root\current\vibeface.exe'
        $body = Get-Content -LiteralPath $shim -Raw
        if (-not $body.Contains('"C:\root\current\vibeface.exe" %*')) { Fail 'self-test: shim body' }
        $junction = Join-Path $directory 'current'
        $pointed = Join-Path $directory 'releases'
        New-Item -ItemType Directory -Path $pointed | Out-Null
        New-Item -ItemType Junction -Path $junction -Target $pointed | Out-Null
        if (-not (Get-Item -LiteralPath $junction -Force).LinkType) {
            Fail 'self-test: this account cannot create a directory junction'
        }
    } finally {
        Remove-Item -LiteralPath $directory -Recurse -Force -ErrorAction SilentlyContinue
    }
    'Installer self-test passed.'
}

$script:Scratch = Join-Path ([System.IO.Path]::GetTempPath()) ('vibeface-install-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $script:Scratch | Out-Null
try {
    if ($SelfTest) {
        Invoke-SelfTest
    } else {
        Install-Release -Channel $Channel -InstallRoot $InstallRoot -BinDir $BinDir
    }
} finally {
    Remove-Item -LiteralPath $script:Scratch -Recurse -Force -ErrorAction SilentlyContinue
}
