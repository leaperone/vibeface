# Installs the latest published Vibeface release for Windows x64.
#
# No account, provider, node data, background service, or automatic update is configured. An
# existing installation is left alone: use vibeface update apply instead. Nothing here needs
# administrator rights or developer mode.

[CmdletBinding()]
param(
    [ValidateSet('stable', 'hourly')]
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
    $wanted = $Channel -eq 'hourly'
    $ordered = @()
    foreach ($release in $Releases) {
        if ($release.draft -ne $false) { continue }
        if ($release.prerelease -ne $wanted) { continue }
        $tag = [string] $release.tag_name
        $shape = if ($wanted) {
            '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-hourly\.([0-9]{10})$'
        } else {
            '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
        }
        $found = [regex]::Match($tag, $shape)
        if (-not $found.Success) { continue }
        $hour = 0
        if ($wanted) {
            $stamp = $found.Groups[4].Value
            $parsed = [datetime]::MinValue
            if (-not [datetime]::TryParseExact($stamp, 'yyyyMMddHH', [cultureinfo]::InvariantCulture,
                    [System.Globalization.DateTimeStyles]::None, [ref] $parsed)) {
                continue
            }
            $hour = [int64] $stamp
        }
        $ordered += [pscustomobject]@{
            Tag = $tag
            Major = [int64] $found.Groups[1].Value
            Minor = [int64] $found.Groups[2].Value
            Patch = [int64] $found.Groups[3].Value
            Hour = $hour
        }
    }
    return ($ordered | Sort-Object Major, Minor, Patch, Hour)
}

function Get-OptionalDownload {
    param([string] $Url, [string] $Path, [int64] $Limit)
    try {
        Invoke-WebRequest -Uri $Url -OutFile $Path -UseBasicParsing -MaximumRedirection 5 `
            -UserAgent 'vibeface-installer' -TimeoutSec 180
    } catch {
        $status = 0
        if ($_.Exception.Response) { $status = [int]$_.Exception.Response.StatusCode }
        if ($status -eq 404) { return $false }
        Fail 'completion manifest download failed (network or GitHub API limit)'
    }
    if ((Get-Item -LiteralPath $Path).Length -gt $Limit) { Fail 'release completion manifest exceeds limit' }
    return $true
}

function Confirm-Completion {
    param([object] $Release, [string] $Tag, [string] $Version, [string] $Archive, [string] $Target)
    $marker = @($Release.assets | Where-Object { $_.name -eq 'release.json' })
    if ($marker.Count -eq 0) { return $false }
    if ($marker.Count -ne 1) { Fail 'release contains duplicate completion markers' }
    if ($marker[0].size -gt 1MB) { Fail 'release completion manifest exceeds limit' }
    $manifestPath = Join-Path $script:Scratch 'completion.json'
    if (-not (Get-OptionalDownload -Url "$Downloads/$Tag/release.json" -Path $manifestPath -Limit 1MB)) {
        return $false
    }
    if ((Get-Item -LiteralPath $manifestPath).Length -gt 1MB) { Fail 'release completion manifest exceeds limit' }
    try { $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json -ErrorAction Stop }
    catch { Fail 'invalid release completion manifest JSON' }
    $props = @($manifest.PSObject.Properties.Name | Sort-Object)
    if (($props -join ',') -ne 'app,assets,channel,schema,source_commit,tag,version') { Fail 'invalid release completion manifest fields' }
    $channel = if ($Tag -match '-hourly\.') { 'hourly' } else { 'stable' }
    if ($manifest.app -ne 'product' -or
        ($manifest.schema -isnot [int] -and $manifest.schema -isnot [long]) -or $manifest.schema -ne 1 -or
        $manifest.tag -cne $Tag -or $manifest.version -cne $Version -or $manifest.channel -cne $channel -or
        $manifest.source_commit -notmatch '^[0-9a-f]{40}$') { Fail 'release completion manifest identity mismatch' }
    $targetItems = @($manifest.assets | Where-Object { $_.name -eq $Archive -or $_.name -eq "$Archive.sha256" })
    if ($targetItems.Count -ne 2 -or @($targetItems | Where-Object { $_.name -eq $Archive }).Count -ne 1 -or
        @($targetItems | Where-Object { $_.name -eq "$Archive.sha256" }).Count -ne 1) { Fail 'completion manifest target assets are missing or duplicated' }
    foreach ($entry in $targetItems) {
        if ($entry.sha256 -cnotmatch '^[0-9a-f]{64}$' -or
            ($entry.size -isnot [long] -and $entry.size -isnot [int])) {
            Fail 'invalid completion manifest target asset metadata'
        }
    }
    $asset = @($Release.assets | Where-Object { $_.name -eq $Archive })
    $checksum = @($Release.assets | Where-Object { $_.name -eq "$Archive.sha256" })
    if ($asset.Count -ne 1 -or $checksum.Count -ne 1) { Fail 'complete release is missing or duplicates its target assets' }
    $archiveEntry = $targetItems | Where-Object { $_.name -eq $Archive }
    $checksumEntry = $targetItems | Where-Object { $_.name -eq "$Archive.sha256" }
    if ([int64]$archiveEntry.size -ne [int64]$asset[0].size -or
        [int64]$checksumEntry.size -ne [int64]$checksum[0].size) { Fail 'completion manifest target asset sizes mismatch' }
    return $manifest
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
            Fail 'no published Stable release; pass -Channel hourly explicitly to install a preview'
        }
        Fail 'no published hourly release'
    }
    $chosen = $null
    for ($index = $selected.Count - 1; $index -ge 0; $index--) {
        $candidateTag = $selected[$index].Tag
        $candidateVersion = $candidateTag.Substring(1)
        $candidatePackage = "vibeface-$candidateVersion-$Target"
        $candidateManifest = Confirm-Completion -Release ($releases | Where-Object tag_name -CEQ $candidateTag | Select-Object -First 1) `
            -Tag $candidateTag -Version $candidateVersion -Archive "$candidatePackage.tar.gz" -Target $Target
        if ($candidateManifest) {
            $tag = $candidateTag; $version = $candidateVersion; $package = $candidatePackage
            $archive = "$package.tar.gz"; $manifest = $candidateManifest; $chosen = $true; break
        }
    }
    if (-not $chosen) { Fail 'no complete published release in the selected channel' }
    $commit = [string] (Get-Json -Url "$Api/commits/$tag").sha
    if ($commit -notmatch '^[0-9a-f]{40}$') { Fail 'release tag has no valid source commit' }
    if ($manifest.source_commit -cne $commit) { Fail 'release completion manifest source does not match the tag' }

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
    $checksumEntry = @($manifest.assets | Where-Object { $_.name -eq "$archive.sha256" })[0]
    $archiveEntry = @($manifest.assets | Where-Object { $_.name -eq $archive })[0]
    if ((Get-Sha256 -Path $checksumPath) -cne $checksumEntry.sha256 -or
        [int64]$archiveEntry.size -ne [int64](Get-Item -LiteralPath $archivePath).Length -or
        [int64]$checksumEntry.size -ne [int64](Get-Item -LiteralPath $checksumPath).Length -or
        $found.Groups[1].Value.ToLowerInvariant() -cne $archiveEntry.sha256) {
        Fail 'release completion manifest digest does not match the target payload'
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
        [pscustomobject]@{ tag_name = 'v0.0.1-hourly.2026101702'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'v0.0.1-hourly.2026101710'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'v0.0.1-hourly.2026022901'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'v0.0.1-hourly.2026101724'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'v0.0.1-nightly.20261018.1'; draft = $false; prerelease = $true },
        [pscustomobject]@{ tag_name = 'www-v999.0.0-hourly.2026101723'; draft = $false; prerelease = $true }
    )
    $picked = @(Select-Release -Releases $stable -Channel 'stable')
    if ($picked[-1].Tag -ne 'v1.10.0') { Fail "self-test: stable selection picked $($picked[-1].Tag)" }
    $picked = @(Select-Release -Releases $stable -Channel 'hourly')
    if ($picked[-1].Tag -ne 'v0.0.1-hourly.2026101710') {
        Fail "self-test: hourly selection picked $($picked[-1].Tag)"
    }
    foreach ($entry in $picked) {
        if ($entry.Tag -in @('v0.0.1-hourly.2026022901', 'v0.0.1-hourly.2026101724')) { Fail 'self-test: accepted an hour that does not exist' }
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

    # Mock the HTTP command only inside this self-test; production downloads stay unchanged.
    function Fail { param([string] $Message) throw $Message }
    Add-Type -TypeDefinition @'
public class VibefaceInstallerTestResponse : System.Net.WebResponse {
    public System.Net.HttpStatusCode StatusCode { get; private set; }
    public VibefaceInstallerTestResponse(int code) { StatusCode = (System.Net.HttpStatusCode)code; }
}
'@
    $responses = @{}
    $requests = New-Object 'System.Collections.Generic.List[string]'
    function Invoke-WebRequest {
        param([string] $Uri, [string] $OutFile, [switch] $UseBasicParsing,
              [int] $MaximumRedirection, [hashtable] $Headers, [string] $UserAgent, [int] $TimeoutSec)
        $requests.Add($Uri)
        if (-not $responses.ContainsKey($Uri)) { throw [System.Net.WebException]::new('fixture transport failure') }
        $response = $responses[$Uri]
        if ($response.Code -ne 200) {
            throw [System.Net.WebException]::new('fixture HTTP error', $null,
                [System.Net.WebExceptionStatus]::ProtocolError, [VibefaceInstallerTestResponse]::new($response.Code))
        }
        [System.IO.File]::WriteAllText($OutFile, $response.Body, (New-Object System.Text.UTF8Encoding($false)))
    }
    function Assert-InstallerError {
        param([scriptblock] $Action, [string] $Message)
        $observed = $null
        try { & $Action | Out-Null } catch { $observed = $_.Exception.Message }
        if (-not $observed -or -not $observed.Contains($Message)) {
            throw "self-test: expected '$Message', observed '$observed'"
        }
    }
    $manifest = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'fixtures/product-release-completion.json') -Raw | ConvertFrom-Json
    $tag = $manifest.tag
    $archive = "vibeface-$($manifest.version)-$Target.tar.gz"
    $complete = [pscustomobject]@{
        tag_name = $tag; draft = $false; prerelease = $true
        assets = @($manifest.assets) + @([pscustomobject]@{ name = 'release.json'; size = 10000 })
    }
    $building = [pscustomobject]@{
        tag_name = 'v0.0.1-hourly.2026110822'; draft = $false; prerelease = $true
        assets = @([pscustomobject]@{ name = 'release.json'; size = 10000 })
    }
    $markerUrl = "$Downloads/$tag/release.json"
    $buildingUrl = "$Downloads/$($building.tag_name)/release.json"
    $responses[$markerUrl] = @{ Code = 200; Body = ($manifest | ConvertTo-Json -Depth 10 -Compress) }
    $accepted = Confirm-Completion -Release $complete -Tag $tag -Version $manifest.version -Archive $archive -Target $Target
    if ($accepted.source_commit -cne '7845e46d9c5b04b66eb7a66bcda31572e1ef7f4a') {
        throw 'self-test: canonical manifest source mismatch'
    }
    $responses[$buildingUrl] = @{ Code = 404; Body = '' }
    if (Confirm-Completion -Release $building -Tag $building.tag_name -Version '0.0.1-hourly.2026110822' `
            -Archive "vibeface-0.0.1-hourly.2026110822-$Target.tar.gz" -Target $Target) {
        throw 'self-test: HTTP 404 marker was treated as complete'
    }
    $responses["$Api/releases?per_page=100&page=1"] = @{
        Code = 200; Body = (@($building, $complete) | ConvertTo-Json -Depth 10 -Compress)
    }
    $responses["$Api/commits/$tag"] = @{ Code = 200; Body = '{"sha":"7845e46d9c5b04b66eb7a66bcda31572e1ef7f4a"}' }
    $install = Join-Path $script:Scratch 'install'
    $bin = Join-Path $script:Scratch 'bin'
    $requests.Clear()
    # Stop at the chosen archive's mocked transport boundary, before any installation.
    Assert-InstallerError { Install-Release -Channel hourly -InstallRoot $install -BinDir $bin } 'download failed'
    if ($requests[$requests.Count - 1] -cne "$Downloads/$tag/$archive") {
        throw 'self-test: newest Building did not select the older complete archive'
    }
    foreach ($code in @(403, 503)) {
        $responses[$buildingUrl] = @{ Code = $code; Body = '' }
        $requests.Clear()
        Assert-InstallerError { Install-Release -Channel hourly -InstallRoot $install -BinDir $bin } 'completion manifest download failed'
        if ($requests.Contains($markerUrl)) { throw 'self-test: HTTP failure silently selected an older release' }
    }
    $responses.Remove($buildingUrl)
    Assert-InstallerError { Install-Release -Channel hourly -InstallRoot $install -BinDir $bin } 'completion manifest download failed'
    $responses[$buildingUrl] = @{ Code = 200; Body = '{bad-json' }
    Assert-InstallerError { Install-Release -Channel hourly -InstallRoot $install -BinDir $bin } 'invalid release completion manifest JSON'
    $responses[$buildingUrl] = @{ Code = 200; Body = ($manifest | ConvertTo-Json -Depth 10 -Compress) }
    Assert-InstallerError { Install-Release -Channel hourly -InstallRoot $install -BinDir $bin } 'release completion manifest identity mismatch'
    $responses[$buildingUrl] = @{ Code = 404; Body = '' }
    $responses["$Api/commits/$tag"] = @{ Code = 200; Body = '{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}' }
    Assert-InstallerError { Install-Release -Channel hourly -InstallRoot $install -BinDir $bin } 'release completion manifest source does not match the tag'
    if (Test-Path -LiteralPath (Join-Path $install 'current')) { throw 'self-test: failed validation installed a release' }
    New-Item -ItemType Directory -Path (Join-Path $install 'current') -Force | Out-Null
    $sentinel = Join-Path $install 'current/retained.txt'
    Set-Content -LiteralPath $sentinel -Value 'retained'
    $requests.Clear()
    Assert-InstallerError { Install-Release -Channel hourly -InstallRoot $install -BinDir $bin } 'current already exists'
    if ($requests.Count -ne 0 -or (Get-Content -LiteralPath $sentinel) -cne 'retained') {
        throw 'self-test: an existing installation was changed'
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
