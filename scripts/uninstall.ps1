<#
.SYNOPSIS
    Removes RustWTM.

.DESCRIPTION
    Stops and removes the RustWTM service (which rolls back every limit it
    applied) along with its blocked-program entries and firewall rules,
    deletes the Start Menu shortcut, removes the install directory from the
    machine PATH and deletes it. Configuration and usage history in
    %ProgramData%\RustWTM are kept unless -RemoveData is given.

.PARAMETER InstallDir
    Directory RustWTM was installed to. Default: C:\Program Files\RustWTM.

.PARAMETER RemoveData
    Also delete %ProgramData%\RustWTM (config, history, logs).
#>
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()][string]$InstallDir = (Join-Path $env:ProgramFiles 'RustWTM'),
    [switch]$RemoveData
)

$ErrorActionPreference = 'Stop'
$ServiceName = 'RustWTM'

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Test-InstalledShortcut([string]$Link, [string]$Gui) {
    if (-not (Test-Path -LiteralPath $Link -PathType Leaf)) { return $false }
    $shell = New-Object -ComObject WScript.Shell
    $shell.CreateShortcut($Link).TargetPath -ieq $Gui
}

function Get-InstallPathEntries {
    [Environment]::GetEnvironmentVariable('Path', 'Machine') -split ';'
}

function Get-UninstallTarget([string]$Dir) {
    $full = [IO.Path]::GetFullPath($Dir)
    $dir = $full.TrimEnd('\', '/')
    if ($dir -ieq [IO.Path]::GetPathRoot($full).TrimEnd('\', '/')) {
        throw 'Choose the RustWTM installation directory inside the drive or share.'
    }
    $exe = Join-Path $dir 'wtm-service.exe'
    $gui = Join-Path $dir 'rustwtm.exe'
    $link = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'RustWTM.lnk'
    $registration = Get-CimInstance -ClassName Win32_Service -Filter "Name = '$ServiceName'"
    if ($registration) {
        $command = [Environment]::ExpandEnvironmentVariables($registration.PathName)
        if ($command -notmatch '^\s*(?:"([^"]+)"|([^\s"]+))(?=\s|$)') {
            throw 'Could not identify the registered RustWTM service executable.'
        }
        $registered = if ($Matches[1]) { $Matches[1] } else { $Matches[2] }
        if ($registered -notmatch '^(?:[a-z]:[\\/]|\\\\)' -or
            [IO.Path]::GetFullPath($registered) -ine $exe) {
            throw 'The RustWTM service is registered at a different path. Run the uninstaller for that installation.'
        }
    } elseif (Test-Path -LiteralPath $link) {
        if (-not (Test-InstalledShortcut $link $gui)) {
            throw 'The RustWTM shortcut belongs to another installation.'
        }
    } elseif (-not (Get-InstallPathEntries | Where-Object { $_ -and $_.TrimEnd('\', '/') -ieq $dir })) {
        throw 'RustWTM installation ownership could not be verified. Portable copies can be removed by deleting their own directory.'
    }
    $hasHelper = Test-Path -LiteralPath $exe -PathType Leaf
    if (-not $hasHelper -and ($registration -or (Test-Path -LiteralPath $gui))) {
        throw 'The RustWTM cleanup binary is missing. Repair the installation before uninstalling.'
    }
    [pscustomobject]@{
        Directory = $dir
        Executable = $exe
        Gui = $gui
        Cli = Join-Path $dir 'wtm.exe'
        Shortcut = $link
        HasService = [bool]$registration
        HasHelper = $hasHelper
    }
}

function Invoke-RustWTMMaintenance([string]$Exe, [string]$Command) {
    $PSNativeCommandUseErrorActionPreference = $false
    & $Exe $Command
    if ($LASTEXITCODE -ne 0) {
        throw "RustWTM $Command failed (exit code $LASTEXITCODE). Repair the installation or retry uninstall."
    }
}

function Remove-RustWTMRegistration($Target) {
    if ($Target.HasService) {
        Write-Step 'Stopping and removing the RustWTM service'
        # The service's uninstall command stops the service and runs cleanup.
        Invoke-RustWTMMaintenance $Target.Executable uninstall
    } elseif ($Target.HasHelper) {
        # A retry after service deletion may still need to remove integration.
        Invoke-RustWTMMaintenance $Target.Executable cleanup
    }
}

function Stop-TargetPrograms($Target) {
    Get-Process -Name 'rustwtm', 'wtm' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -iin @($Target.Gui, $Target.Cli) } |
        Stop-Process -Force
}

function Remove-Tree([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if (-not $item.PSIsContainer) {
        Remove-Item -LiteralPath $item.FullName -Force
        return
    }
    # Remove a junction as a leaf. Windows PowerShell's recursive directory
    # removal can fail on a junction even when its parent can be removed.
    if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        foreach ($child in Get-ChildItem -LiteralPath $item.FullName -Force) {
            Remove-Tree $child.FullName
        }
    }
    [IO.Directory]::Delete($item.FullName, $false)
}

function Invoke-UninstallCopy([string]$Self, [string]$Dir, [switch]$RemoveData) {
    $copyDir = Join-Path $env:SystemRoot ('Temp\rustwtm-uninstall-' + [Guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $copyDir
    $oldMarker = $env:RUSTWTM_UNINSTALL_COPY
    try {
        $acl = New-Object Security.AccessControl.DirectorySecurity
        $acl.SetSecurityDescriptorSddlForm('O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)')
        Set-Acl -LiteralPath $copyDir -AclObject $acl
        $copy = Join-Path $copyDir 'uninstall.ps1'
        Copy-Item -LiteralPath $Self -Destination $copy -Force
        $env:RUSTWTM_UNINSTALL_COPY = '1'
        $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $copy, '-InstallDir', $Dir)
        if ($RemoveData) { $arguments += '-RemoveData' }
        $PSNativeCommandUseErrorActionPreference = $false
        & powershell.exe @arguments | Out-Host
        return $LASTEXITCODE
    } finally {
        $env:RUSTWTM_UNINSTALL_COPY = $oldMarker
        try { Remove-Tree $copyDir }
        catch { Write-Warning "Could not remove the temporary uninstaller at $copyDir." -WarningAction Continue }
    }
}

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell (Run as administrator).'
}

$target = Get-UninstallTarget $InstallDir
$InstallDir = $target.Directory

# When run from inside the install directory, continue from a temp copy so the
# directory can be deleted.
$self = $MyInvocation.MyCommand.Path
if ($self -and $self.StartsWith("$InstallDir\", [StringComparison]::OrdinalIgnoreCase) -and -not $env:RUSTWTM_UNINSTALL_COPY) {
    # A fresh folder under the system's temp folder, which only SYSTEM and
    # administrators can reach: a fixed name in the user's temp folder could
    # be replaced by anything running as the user, then run elevated.
    exit (Invoke-UninstallCopy $self $InstallDir -RemoveData:$RemoveData)
}

Remove-RustWTMRegistration $target
Stop-TargetPrograms $target

if (Test-Path -LiteralPath $InstallDir) {
    Remove-Tree $InstallDir
    Write-Step "Removed $InstallDir"
}

$data = [IO.Path]::GetFullPath((Join-Path $env:ProgramData 'RustWTM'))
if ($RemoveData -and (Test-Path -LiteralPath $data)) {
    Remove-Tree $data
    Write-Step "Removed $data"
} elseif (Test-Path -LiteralPath $data) {
    Write-Host "Kept configuration and history in $data (use -RemoveData to delete)."
}

# Keep installation identity available while file removal can still fail.
if (Test-InstalledShortcut $target.Shortcut $target.Gui) {
    Remove-Item -LiteralPath $target.Shortcut -Force
    Write-Step 'Removed Start Menu shortcut'
}
$entries = @(Get-InstallPathEntries)
$parts = @($entries | Where-Object { -not $_ -or $_.TrimEnd('\', '/') -ine $InstallDir })
if ($parts.Count -ne $entries.Count) {
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'Machine')
}

Write-Host 'RustWTM has been removed.' -ForegroundColor Green
