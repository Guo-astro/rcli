$ErrorActionPreference = 'Stop'

$Repo = 'RunanywhereAI/wally'
# There is no Homebrew here, so this installer does the whole job itself rather
# than handing off to a package manager: download the release zip, check it,
# unpack it, put it on PATH.
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\wally'

# Windows PowerShell picks the older protocols on some builds and api.github.com
# refuses anything below TLS 1.2.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
# Invoke-WebRequest spends most of a large download redrawing its progress bar.
$ProgressPreference = 'SilentlyContinue'

# A file:// mirror (WALLY_INSTALL_BASE_URL in release tests) is copied rather
# than fetched: PowerShell 7's Invoke-WebRequest refuses the file scheme,
# which Windows PowerShell 5.1 accepted.
function Save-Url([string]$Url, [string]$OutFile) {
    $Uri = [Uri]$Url
    if ($Uri.IsFile) {
        Copy-Item -LiteralPath $Uri.LocalPath -Destination $OutFile -ErrorAction Stop
    } else {
        Invoke-WebRequest -Uri $Url -OutFile $OutFile
    }
}

function Write-Info([string]$Message) {
    Write-Host '==> ' -ForegroundColor Blue -NoNewline
    Write-Host $Message
}
function Write-Ok([string]$Message) {
    Write-Host '==> ' -ForegroundColor Green -NoNewline
    Write-Host $Message
}
function Write-Warn([string]$Message) {
    Write-Host 'Warning: ' -ForegroundColor Yellow -NoNewline
    Write-Host $Message
}
# throw rather than exit, because the documented way to run this is
# `irm ... | iex`: an exit there closes the user's console, taking the error
# message with it.
function Fail([string]$Message) {
    throw "Error: $Message"
}

# Replaces an existing install file by file. Windows will not move a folder
# whose wally.exe or DLLs are in use (`wally update` runs this installer while
# wally itself is running, and another terminal may be running wally too), but
# it will rename each of those files. Replace only names present in the verified
# release, preserving other files and legacy .old backups. A unique backup name
# avoids collisions with user backups. Only a committed receipt authorizes a
# later startup to delete backups; failed/interrupted updates keep recovery files.
function Update-InPlace([string]$Source, [string]$Target) {
    # Serialize replacements so another installer cannot invalidate rollback.
    $Lock = [IO.File]::Open((Join-Path $Target '.wally-update.lock'),
        [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $Id = [Guid]::NewGuid().ToString('N')
    $Pending = Join-Path $Target ".wally-update-$Id.pending"
    $Receipt = Join-Path $Target ".wally-update-$Id.json"
    $Retired = @()
    $Placed = @()
    try {
        try {
            $Items = @(Get-ChildItem -LiteralPath $Source -Force)
            # Windows release bin/ is flat. Refuse directories and links rather
            # than recursively retiring unknown contents under a matching name.
            foreach ($Item in $Items) {
                if ($Item.PSIsContainer -or ($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                    $Item.Name.StartsWith('.wally-', [StringComparison]::OrdinalIgnoreCase)) {
                    throw "Unsupported release entry: $($Item.Name)"
                }
                $Destination = Join-Path $Target $Item.Name
                if (Test-Path -LiteralPath $Destination) {
                    $Existing = Get-Item -LiteralPath $Destination -Force
                    if ($Existing.PSIsContainer -or ($Existing.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                        throw "Cannot replace a directory or link: $($Item.Name)"
                    }
                    if (Test-Path -LiteralPath "$Destination.wally-backup-$Id") { throw "Backup already exists: $Destination.wally-backup-$Id" }
                }
            }
            foreach ($Item in $Items) {
                $Destination = Join-Path $Target $Item.Name
                if (Test-Path -LiteralPath $Destination) {
                    $Aside = "$Destination.wally-backup-$Id"
                    if (Test-Path -LiteralPath $Aside) { throw "Backup already exists: $Aside" }
                    $Hash = (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant()
                    Rename-Item -LiteralPath $Destination -NewName (Split-Path $Aside -Leaf) -ErrorAction Stop
                    $Retired += [pscustomobject]@{ Original = $Destination; Aside = $Aside; Hash = $Hash }
                }
                Move-Item -LiteralPath $Item.FullName -Destination $Destination -ErrorAction Stop
                $Placed += $Destination
            }
            $Backups = @($Retired | ForEach-Object {
                @{ name = (Split-Path $_.Aside -Leaf); sha256 = $_.Hash }
            })
            $Json = @{ version = 1; backups = $Backups } | ConvertTo-Json -Depth 3 -Compress
            $Bytes = [Text.UTF8Encoding]::new($false).GetBytes($Json)
            $Stream = [IO.File]::Open($Pending, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try { $Stream.Write($Bytes, 0, $Bytes.Length); $Stream.Flush($true) } finally { $Stream.Dispose() }
            # Publication is the commit point: startup never sees a receipt for
            # files which might still need to be restored by this transaction.
            Rename-Item -LiteralPath $Pending -NewName (Split-Path $Receipt -Leaf) -ErrorAction Stop
        } catch {
            $Reason = $_.Exception.Message
            $RestoreFailed = $false
            foreach ($Path in $Placed) {
                try { Remove-Item -LiteralPath $Path -Force -ErrorAction Stop } catch { $RestoreFailed = $true }
            }
            foreach ($Entry in $Retired) {
                try {
                    Rename-Item -LiteralPath $Entry.Aside -NewName (Split-Path $Entry.Original -Leaf) -ErrorAction Stop
                } catch { $RestoreFailed = $true }
            }
            Remove-Item -LiteralPath $Pending -Force -ErrorAction SilentlyContinue
            if ($RestoreFailed) { Fail "Could not replace the installed files ($Reason); some files could not be restored. Recovery backups remain in $Target." }
            Fail "Could not replace the installed files ($Reason); the existing installation was restored."
        }
        # Locked backups and their receipt survive until a later wally startup.
        foreach ($Entry in $Retired) {
            Remove-Item -LiteralPath $Entry.Aside -Force -ErrorAction SilentlyContinue
        }
    } finally { $Lock.Dispose() }
}

# Check mode: print the latest release version and nothing else, then stop
# without installing. `wally update` and its update-available notice read it.
# An environment variable rather than a flag, because `irm ... | iex` passes
# no arguments.
$CheckOnly = $env:WALLY_INSTALL_CHECK -eq '1'

# Two overrides, for release tests and mirrors rather than everyday use --
# the same shape and names as install.sh's:
#   WALLY_INSTALL_VERSION      install that release instead of the latest
#   WALLY_INSTALL_BASE_URL     fetch the zip and its .sha256 from <url>/
#                              instead of the GitHub release (http, https or
#                              file); needs WALLY_INSTALL_VERSION
#
# $Release stays $null on this path: there is no releases-API lookup to ask
# "does this asset exist" of, so an asset that is not actually at the given
# version/URL is discovered the same way it would be anyway -- the download
# below fails.
if ($env:WALLY_INSTALL_VERSION) {
    $Version = $env:WALLY_INSTALL_VERSION -replace '^v', ''
    if ($Version -notmatch '^\d+\.\d+\.\d+$') {
        Fail "WALLY_INSTALL_VERSION must look like 1.2.3, not '$($env:WALLY_INSTALL_VERSION)'"
    }
    $Release = $null
    if (-not $CheckOnly) { Write-Info "Installing v$Version" }
} else {
    if ($env:WALLY_INSTALL_BASE_URL) {
        Fail 'WALLY_INSTALL_BASE_URL needs WALLY_INSTALL_VERSION: a mirror has no latest-release lookup'
    }
    if (-not $CheckOnly) { Write-Info 'Checking latest Wally release...' }
    try {
        $Release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    } catch {
        Fail 'Could not determine latest release version. Check your internet connection.'
    }
    $Version = "$($Release.tag_name)" -replace '^v', ''
    if (-not $Version) { Fail 'Could not determine latest release version. Check your internet connection.' }
    if (-not $CheckOnly) { Write-Info "Latest version: v$Version" }
}

if ($CheckOnly) {
    Write-Output $Version
    return
}

# PROCESSOR_ARCHITECTURE reports the process, not the machine, so a 32-bit host
# under WOW64 says x86 while ARCHITEW6432 names what is really underneath.
$Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
# Prism on Windows ARM64 runs the x64 zip. Native arm64 zips are preferred when
# $Release can say one exists; an explicit version/mirror is trusted to have
# what it says, the same as install.sh trusts WALLY_INSTALL_VERSION's platform.
$AssetName = "wally-$Version-windows-x86_64.zip"
if ($Arch -eq 'ARM64') {
    $ArmAssetName = "wally-$Version-windows-arm64.zip"
    $ArmAvailable = if ($Release) {
        [bool]($Release.assets | Where-Object { $_.name -eq $ArmAssetName } | Select-Object -First 1)
    } else {
        $true
    }
    if ($ArmAvailable) {
        $AssetName = $ArmAssetName
    } else {
        Write-Warn "No native ARM64 zip; installing the x64 build (Windows on ARM can run it)."
    }
} elseif ($Arch -ne 'AMD64') {
    Fail "Wally requires 64-bit Windows. Detected: $Arch"
}

# Where the asset and its checksum come from: the release's own asset list
# when $Release was looked up, or WALLY_INSTALL_BASE_URL (falling back to the
# normal GitHub release URL for the given version) when it was not.
function Resolve-AssetUrl([string]$Name) {
    if ($Release) {
        $Found = $Release.assets | Where-Object { $_.name -eq $Name } | Select-Object -First 1
        if (-not $Found) { return $null }
        return $Found.browser_download_url
    }
    $Base = if ($env:WALLY_INSTALL_BASE_URL) { $env:WALLY_INSTALL_BASE_URL.TrimEnd('/') } else { "https://github.com/$Repo/releases/download/v$Version" }
    return "$Base/$Name"
}

$AssetUrl = Resolve-AssetUrl $AssetName
if (-not $AssetUrl) {
    Fail "v$Version does not publish $AssetName. Open an issue at https://github.com/$Repo/issues"
}
$ShaAssetName = "$AssetName.sha256"
$ShaUrl = Resolve-AssetUrl $ShaAssetName
if (-not $ShaUrl) {
    Fail "v$Version does not publish $ShaAssetName. Refusing an unverified download."
}

$Temp = Join-Path ([IO.Path]::GetTempPath()) ('wally-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Temp -Force | Out-Null
try {
    $Zip = Join-Path $Temp $AssetName
    Write-Info "Downloading $AssetName..."
    try {
        Save-Url $AssetUrl $Zip
    } catch {
        Fail "Could not download $AssetUrl"
    }

    # Through a file rather than straight into a variable: the asset is served
    # as octet-stream and the web cmdlets hand back bytes rather than text.
    $ShaFile = Join-Path $Temp $ShaAssetName
    Save-Url $ShaUrl $ShaFile
    $ShaLine = (Get-Content -Raw -LiteralPath $ShaFile).Trim()
    if ($ShaLine -notmatch '^([0-9A-Fa-f]{64})\s+\*?([^\r\n]+)$') {
        Fail "$ShaAssetName is not a valid SHA-256 sidecar."
    }
    $Expected = $Matches[1].ToLowerInvariant()
    $ListedAsset = $Matches[2].Trim()
    if ($ListedAsset -ne $AssetName) {
        Fail "$ShaAssetName names $ListedAsset instead of $AssetName."
    }
    $Actual = (Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Fail "Checksum mismatch on $AssetName. Expected $Expected, got $Actual. Do not use this download."
    }
    Write-Ok 'Checksum verified'

    Write-Info "Installing Wally v$Version to $InstallDir..."
    Expand-Archive -LiteralPath $Zip -DestinationPath $Temp -Force
    # The packager stages the tree as wally-<platform>, without the version
    # (scripts/build/package-wally-windows.ps1, and install.sh expects the same),
    # so the folder is not the asset name minus .zip. The versioned spelling is
    # still accepted in case a future archive adopts it.
    $Platform = $AssetName -replace "^wally-$([Regex]::Escape($Version))-", '' -replace '(-dev)?\.zip$', ''
    $Unpacked = @(
        (Join-Path $Temp "wally-$Platform\bin"),
        (Join-Path $Temp ([IO.Path]::GetFileNameWithoutExtension($AssetName) + '\bin'))
    ) | Where-Object { Test-Path -LiteralPath (Join-Path $_ 'wally.exe') } | Select-Object -First 1
    if (-not $Unpacked) {
        Fail "$AssetName does not have the layout this installer expects. Open an issue at https://github.com/$Repo/issues"
    }

    # Validate a complete candidate before replacing a working installation.
    # wally.exe and its DLLs stay together exactly as they are in archive bin/.
    $Candidate = Join-Path $Temp 'install-candidate'
    New-Item -ItemType Directory -Path $Candidate -Force | Out-Null
    Copy-Item -Path (Join-Path $Unpacked '*') -Destination $Candidate -Recurse -Force
    $CandidateExe = Join-Path $Candidate 'wally.exe'
    if (-not (Test-Path -LiteralPath $CandidateExe)) {
        Fail "$AssetName is missing bin\wally.exe."
    }
    $VersionOutput = @(& $CandidateExe --version 2>&1)
    if ($LASTEXITCODE -ne 0) {
        Fail 'The downloaded wally.exe does not run; the existing installation was left unchanged.'
    }
    $EscapedVersion = [Regex]::Escape($Version)
    if (($VersionOutput -join "`n") -notmatch "(?m)^wally\s+$EscapedVersion(?:\s|$)") {
        Fail "The downloaded executable does not report Wally v$Version; the existing installation was left unchanged."
    }

    $InstallParent = Split-Path $InstallDir -Parent
    New-Item -ItemType Directory -Path $InstallParent -Force | Out-Null
    if (Test-Path -LiteralPath $InstallDir) {
        Update-InPlace $Candidate $InstallDir
    } else {
        Move-Item -LiteralPath $Candidate -Destination $InstallDir
    }
} finally {
    Remove-Item -LiteralPath $Temp -Recurse -Force -ErrorAction SilentlyContinue
}

$Exe = Join-Path $InstallDir 'wally.exe'
if (-not (Test-Path -LiteralPath $Exe)) { Fail "Installation failed. wally.exe is not in $InstallDir." }
& $Exe --version | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "Installation failed. wally.exe is installed but does not run." }

# The user's own PATH, never the machine's: this installs under LOCALAPPDATA for
# one account and needs no administrator to do it.
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$Entries = @()
if ($UserPath) { $Entries = @($UserPath -split ';' | Where-Object { $_ }) }
if ($Entries -contains $InstallDir) {
    Write-Ok "$InstallDir is already on your PATH"
    $PathChanged = $false
} else {
    [Environment]::SetEnvironmentVariable('Path', (($Entries + $InstallDir) -join ';'), 'User')
    Write-Ok "Added $InstallDir to your PATH"
    $PathChanged = $true
}

Write-Ok "Wally v$Version installed successfully"
Write-Host ''
if ($PathChanged) {
    Write-Warn 'Open a new terminal before running wally. This one was started with the old PATH.'
}
Write-Host ''
Write-Info 'Getting started:'
Write-Host '    wally models list --all       every model in the catalog'
Write-Host '    wally models pull qwen3-0.6b  download one'
Write-Host '    wally run qwen3-0.6b          talk to it, /? for commands'
Write-Host '    wally backends                which engines this build linked'
Write-Host ''
Write-Host '  Models download on demand into %LOCALAPPDATA%\RunAnywhere'
