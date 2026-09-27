<#
.SYNOPSIS
Installs aisshd and aissh-mcp for the current user and starts the daemon.

.DESCRIPTION
The Windows counterpart of scripts/install-local.sh. The macOS script installs a
LaunchAgent; the equivalent here is a logon entry under the current user's Run
key, which needs no elevation.

The daemon refuses a configuration that other accounts can read, so the copied
config.toml is restricted to this account, SYSTEM and Administrators before the
daemon is started, and the daemon restricts the rest of the tree on first run.

.PARAMETER Directory
The configuration root to install into. Defaults to AISSH_ROOT, or ~/.aissh.
A custom root is passed to the daemon this script starts through AISSH_ROOT; a
logon entry runs the daemon with whatever environment the user has, so it finds
a custom root only when AISSH_ROOT is set there too.

.PARAMETER NoAutostart
Installs without adding the logon entry, for a daemon started only on demand.

.PARAMETER Uninstall
Removes the logon entry and stops a running daemon. The configuration and the
installed helpers are left in place.

.EXAMPLE
./install-local.ps1

.EXAMPLE
./install-local.ps1 -Directory D:\aissh-test -NoAutostart
#>
[CmdletBinding()]
param(
    [string]$Directory,
    [switch]$NoAutostart,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$runValue = 'AI SSH daemon'

$projectRoot = Split-Path -Parent $PSScriptRoot
$root = if ($Directory) { $Directory } elseif ($env:AISSH_ROOT) { $env:AISSH_ROOT } else { Join-Path $env:USERPROFILE '.aissh' }
$bin = Join-Path $root 'bin'
$daemon = Join-Path $bin 'aisshd.exe'

if ($Uninstall) {
    Remove-ItemProperty -Path $runKey -Name $runValue -ErrorAction SilentlyContinue
    Get-Process aisshd -ErrorAction SilentlyContinue | Stop-Process
    Write-Host "Removed the logon entry and stopped aisshd. $root was left in place."
    return
}

cargo build --release --manifest-path (Join-Path $projectRoot 'Cargo.toml') -p aisshd -p aissh-mcp

$directories = @($root, (Join-Path $root 'keys'), (Join-Path $root 'data'), (Join-Path $root 'run'), $bin)
New-Item -ItemType Directory -Force -Path $directories | Out-Null

foreach ($name in 'aisshd', 'aissh-mcp') {
    Copy-Item -Force (Join-Path $projectRoot "target\release\$name.exe") (Join-Path $bin "$name.exe")
}

$config = Join-Path $root 'config.toml'
if (-not (Test-Path $config)) {
    Copy-Item (Join-Path $projectRoot 'config.example.toml') $config
    # Same guarantee as chmod 600: the file is readable only by this account.
    & icacls $config /inheritance:r /grant:r "${env:USERNAME}:(F)" 'SYSTEM:(F)' 'Administrators:(F)' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot restrict $config" }
}

# The daemon resolves its root from AISSH_ROOT when that is set, so a custom
# directory has to be exported for the process started below. It picks up the
# missing directories and restricts the whole tree on this first run, which is
# also why it is started before any key is placed in it.
$env:AISSH_ROOT = $root
# Started with no console window at all. `Start-Process -WindowStyle Hidden` does
# not hide the console Windows opens for a console program, so the process is
# created with CREATE_NO_WINDOW instead; it inherits this environment either way.
$daemonStart = New-Object System.Diagnostics.ProcessStartInfo
$daemonStart.FileName = $daemon
$daemonStart.UseShellExecute = $false
$daemonStart.CreateNoWindow = $true
[System.Diagnostics.Process]::Start($daemonStart) | Out-Null

if (-not $NoAutostart) {
    # A Run key cannot carry CREATE_NO_WINDOW, so at logon the daemon is started
    # the way Explorer starts a shortcut and hides its own console window.
    New-Item -Path $runKey -Force | Out-Null
    New-ItemProperty -Path $runKey -Name $runValue -Value "`"$daemon`"" -PropertyType String -Force | Out-Null
}

Write-Host "Installed aisshd and aissh-mcp in $bin"
Write-Host "Edit $config before using a target."
