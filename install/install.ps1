<#
.SYNOPSIS
    Installs azazel for the current user and puts it on PATH, or removes it again.

.DESCRIPTION
    One-line install of the latest release:

        irm https://raw.githubusercontent.com/sinnafuls/azazel-cli/main/install/install.ps1 | iex

    The binary comes from -From; else from azazel.exe next to this script (an unpacked release);
    else it is built from the source tree this script sits in (cargo build --release); else the
    latest GitHub release is downloaded and its SHA-256 checked. It is copied to -Dir (default
    %LOCALAPPDATA%\Programs\azazel, or $env:AZAZEL_INSTALL_DIR) together with this script, so the
    install can later be removed with:

        & "$env:LOCALAPPDATA\Programs\azazel\install.ps1" -Uninstall

    The user PATH is changed in the registry and new terminals see it; this one is updated too.
    Settings, logins and sessions live in ~/.azazel and are kept unless -Purge is given.

.EXAMPLE
    .\install\install.ps1
    .\install\install.ps1 -Dir D:\tools\azazel
    .\install\install.ps1 -Uninstall -Purge
#>
[CmdletBinding()]
param(
    [string]$From,
    [string]$Dir,
    [switch]$NoPath,
    [switch]$Uninstall,
    [switch]$Purge
)

# Everything runs in a child scope, so `irm ... | iex` leaves no settings behind in the shell.
& {
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is far slower with a progress bar

$releases = if ($env:AZAZEL_RELEASE_BASE) { $env:AZAZEL_RELEASE_BASE }
            else { 'https://github.com/sinnafuls/azazel-cli/releases/latest/download' }
if (-not $Dir) {
    $Dir = if ($env:AZAZEL_INSTALL_DIR) { $env:AZAZEL_INSTALL_DIR }
           else { Join-Path $env:LOCALAPPDATA 'Programs\azazel' }
}
if ($env:AZAZEL_NO_MODIFY_PATH) { $NoPath = $true }

function Get-UserPath {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        # Unexpanded, so entries such as %USERPROFILE%\bin survive the rewrite.
        return [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    } finally {
        $key.Close()
    }
}

function Set-UserPath([string]$Value) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        # Keep the kind the value already has; a new one may hold %VARIABLES%.
        $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        if ($key.GetValueNames() -contains 'Path') { $kind = $key.GetValueKind('Path') }
        $key.SetValue('Path', $Value, $kind)
    } finally {
        $key.Close()
    }
    # Setting any user variable through .NET tells running programs (Explorer, new terminals)
    # that the environment changed.
    [Environment]::SetEnvironmentVariable('AZAZEL_INSTALLER_REFRESH', '1', 'User')
    [Environment]::SetEnvironmentVariable('AZAZEL_INSTALLER_REFRESH', $null, 'User')
}

function Test-SameDir([string]$A, [string]$B) {
    $expanded = [Environment]::ExpandEnvironmentVariables($A).TrimEnd('\')
    return $expanded -ieq $B.TrimEnd('\')
}

# Every entry, empty ones included, so a PATH is written back exactly as it was apart from ours.
function Get-Entries([string]$PathValue) {
    if ($PathValue -eq '') { return @() }
    return @($PathValue -split ';')
}

$Dir = [IO.Path]::GetFullPath($Dir)
$home_dir = if ($env:AZAZEL_HOME) { $env:AZAZEL_HOME } else { Join-Path $HOME '.azazel' }

if ($Uninstall) {
    $entries = Get-Entries (Get-UserPath)
    $kept = @($entries | Where-Object { -not (Test-SameDir $_ $Dir) })
    if ($kept.Count -ne $entries.Count) {
        Set-UserPath ($kept -join ';')
        Write-Host "removed $Dir from the user PATH"
    }
    $env:Path = ((Get-Entries $env:Path) | Where-Object { -not (Test-SameDir $_ $Dir) }) -join ';'
    if (Test-Path $Dir) {
        # This script may be running from the install directory; Windows lets it be deleted.
        Remove-Item -Recurse -Force $Dir
        Write-Host "removed $Dir"
    }
    if ($Purge -and (Test-Path $home_dir)) {
        Remove-Item -Recurse -Force $home_dir
        Write-Host "removed $home_dir (settings, logins and sessions)"
    } elseif (Test-Path $home_dir) {
        Write-Host "kept $home_dir (settings, logins and sessions); -Purge removes it"
    }
    Write-Host 'azazel is uninstalled'
    return
}

# The script copy kept for uninstalling: this file, or the one inside a downloaded release.
# Run through `irm | iex` there is no file, and nothing next to it to use.
$installer = $PSCommandPath
$work = $null
try {
    if (-not $From -and $PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot 'azazel.exe'))) {
        $From = Join-Path $PSScriptRoot 'azazel.exe'
    } elseif (-not $From -and $PSScriptRoot -and
              (Test-Path (Join-Path $PSScriptRoot '..\crates\azazel-cli\Cargo.toml'))) {
        $repo = Split-Path $PSScriptRoot -Parent
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
            throw 'cargo is not on PATH: install Rust from https://rustup.rs or pass -From <azazel.exe>'
        }
        Write-Host 'building azazel (cargo build --release -p azazel-cli)...'
        Push-Location $repo
        try {
            & cargo build --release -p azazel-cli
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
        } finally {
            Pop-Location
        }
        $From = Join-Path $repo 'target\release\azazel.exe'
    } elseif (-not $From) {
        # Releases are built for x86_64; Windows on ARM runs them through emulation.
        $asset = 'azazel-windows-x86_64.zip'
        $work = Join-Path ([IO.Path]::GetTempPath()) ("azazel-install-" + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Force -Path $work | Out-Null
        # Windows PowerShell 5 may default to TLS versions GitHub no longer accepts.
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor
            [Net.SecurityProtocolType]::Tls12
        Write-Host "downloading $asset..."
        $zip = Join-Path $work $asset
        Invoke-WebRequest -UseBasicParsing -Uri "$releases/$asset" -OutFile $zip
        Invoke-WebRequest -UseBasicParsing -Uri "$releases/$asset.sha256" -OutFile "$zip.sha256"
        $expected = ((Get-Content -Raw "$zip.sha256").Trim() -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
        if ($expected -ne $actual) {
            throw "checksum mismatch for $asset (expected $expected, got $actual)"
        }
        Expand-Archive -Path $zip -DestinationPath $work -Force
        $From = Join-Path $work 'azazel-windows-x86_64\azazel.exe'
        $installer = Join-Path $work 'azazel-windows-x86_64\install.ps1'
    }
    if (-not (Test-Path $From -PathType Leaf)) {
        throw "$From is not a file"
    }

    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $target = Join-Path $Dir 'azazel.exe'
    try {
        Copy-Item -Force $From $target
    } catch {
        throw "could not replace $target (is azazel still running?): $($_.Exception.Message)"
    }
    if ($installer -and (Test-Path $installer) -and
        -not (Test-SameDir (Split-Path $installer -Parent) $Dir)) {
        Copy-Item -Force $installer (Join-Path $Dir 'install.ps1')
    }
    Write-Host "installed $target"
} finally {
    if ($work) { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
}

if (-not $NoPath) {
    $current = Get-UserPath
    if (-not ((Get-Entries $current) | Where-Object { Test-SameDir $_ $Dir })) {
        # A PATH that ends in `;` keeps doing so, which also lets -Uninstall restore it exactly.
        $new = if ($current -eq '') { $Dir }
               elseif ($current.EndsWith(';')) { "$current$Dir;" }
               else { "$current;$Dir" }
        Set-UserPath $new
        Write-Host "added $Dir to the user PATH (new terminals pick it up)"
    }
    if (-not ((Get-Entries $env:Path) | Where-Object { Test-SameDir $_ $Dir })) {
        $env:Path = "$env:Path;$Dir"
    }
}

$version = & $target --version
Write-Host "$version is ready: run ``azazel`` in a project directory to start"
Write-Host "tab completion: add  azazel completions powershell | Out-String | Invoke-Expression  to `$PROFILE"
Write-Host "update with ``azazel update``; remove with: & `"$(Join-Path $Dir 'install.ps1')`" -Uninstall"
}
