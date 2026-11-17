<#
.SYNOPSIS
    Launches a Windows VM under OpenVMM (WHP) with FastDesk zero-copy SHM display
    and connects fastdesk-viewer.exe.

.DESCRIPTION
    Auto-discovers openvmm.exe, fastdesk-viewer.exe, and firmware\MSVM.fd next to
    this script (or in local build/package directories), pre-allocates the shared
    VRAM file for zero-copy local display, attaches the primary VHDX/IMG (and an
    optional installer ISO) on a VMBus SCSI controller, and starts the viewer.

.EXAMPLE
    .\Start-OpenVmmFastDesktop.ps1 -DiskPath C:\VMs\win11.vhdx -Memory 8G -Processors 4

.EXAMPLE
    .\Start-OpenVmmFastDesktop.ps1 -DiskPath C:\VMs\win11.img -CreateDiskSize 64G -IsoPath C:\ISOs\Win11.iso -Memory 8G -Processors 4
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$DiskPath,

    [string]$IsoPath = "",
    [string]$CreateDiskSize = "",
    [switch]$ReadOnlyDiff,

    [string]$OpenVmmExe = "",
    [string]$ViewerExe = "",
    [string]$UefiFirmware = "",

    [string]$Memory = "8G",
    [int]$Processors = 4,
    [int]$VramMb = 32,
    [string]$ListenAddr = "127.0.0.1:5901",
    [string]$ShmPath = "$env:TEMP\openvmm-fastdesk-vram.shm",
    [switch]$UseHvSock,
    [int]$VsockPort = 5901,
    [switch]$NoNet,
    [switch]$SerialConsole
)

$ErrorActionPreference = "Stop"

function Resolve-FirstExisting {
    param(
        [string]$ExplicitPath,
        [string[]]$Candidates
    )
    if (-not [string]::IsNullOrWhiteSpace($ExplicitPath)) {
        if (-not (Test-Path -LiteralPath $ExplicitPath)) {
            throw "Specified file not found: $ExplicitPath"
        }
        return (Resolve-Path -LiteralPath $ExplicitPath).Path
    }
    foreach ($candidate in $Candidates) {
        if (-not [string]::IsNullOrWhiteSpace($candidate) -and (Test-Path -LiteralPath $candidate)) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    return ""
}

function Format-ProcessArgument {
    param([string]$Arg)
    if ($Arg -match '[\s"]') {
        return '"' + ($Arg -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
    }
    return $Arg
}

# Verify Windows Hypervisor Platform (WHP) is enabled.
$whpFeature = Get-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -ErrorAction SilentlyContinue
if ($null -ne $whpFeature -and $whpFeature.State -ne "Enabled") {
    Write-Warning "Windows Hypervisor Platform (HypervisorPlatform) is currently $($whpFeature.State)."
    Write-Warning "Enable it from an elevated PowerShell prompt with:"
    Write-Warning "  Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform"
}

$resolvedOpenVmm = Resolve-FirstExisting -ExplicitPath $OpenVmmExe -Candidates @(
    (Join-Path $PSScriptRoot "openvmm.exe"),
    ".\openvmm.exe",
    (Join-Path $PSScriptRoot "..\openvmm\target\release\openvmm.exe")
)
if ([string]::IsNullOrWhiteSpace($resolvedOpenVmm)) {
    throw "openvmm.exe not found next to this script, in the current directory, or in openvmm\target\release\openvmm.exe."
}

$resolvedViewer = Resolve-FirstExisting -ExplicitPath $ViewerExe -Candidates @(
    (Join-Path $PSScriptRoot "fastdesk-viewer.exe"),
    ".\fastdesk-viewer.exe",
    (Join-Path $PSScriptRoot "..\fastdesk-viewer\target\release\fastdesk-viewer.exe")
)

$resolvedUefi = Resolve-FirstExisting -ExplicitPath $UefiFirmware -Candidates @(
    (Join-Path $PSScriptRoot "firmware\MSVM.fd"),
    ".\firmware\MSVM.fd",
    (Join-Path $PSScriptRoot "..\openvmm\.packages\hyperv.uefi.mscoreuefi.x64.RELEASE\MsvmX64\RELEASE_VS2022\FV\MSVM.fd")
)

if (-not (Test-Path -LiteralPath $DiskPath) -and [string]::IsNullOrWhiteSpace($CreateDiskSize)) {
    throw "Disk image not found: $DiskPath (pass -CreateDiskSize e.g. '64G' to create a new raw disk image)."
}
if (-not [string]::IsNullOrWhiteSpace($IsoPath) -and -not (Test-Path -LiteralPath $IsoPath)) {
    throw "ISO image not found: $IsoPath"
}

$diskParent = Split-Path -Parent $DiskPath
if ($diskParent -and -not (Test-Path -LiteralPath $diskParent)) {
    New-Item -ItemType Directory -Path $diskParent -Force | Out-Null
}

# Pre-create/size shared VRAM backing file for zero-copy local viewer mapping.
$vramBytes = [int64]$VramMb * 1MB
$shmDir = Split-Path -Parent $ShmPath
if ($shmDir -and -not (Test-Path -LiteralPath $shmDir)) {
    New-Item -ItemType Directory -Path $shmDir -Force | Out-Null
}
$fs = [System.IO.File]::Open($ShmPath, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
try {
    $fs.SetLength($vramBytes)
} finally {
    $fs.Dispose()
}

$vmmArgs = @()
if (-not [string]::IsNullOrWhiteSpace($resolvedUefi)) {
    $vmmArgs += @("--uefi", "firmware=$resolvedUefi")
} elseif (-not [string]::IsNullOrWhiteSpace($env:X86_64_OPENVMM_UEFI_FIRMWARE) -and (Test-Path -LiteralPath $env:X86_64_OPENVMM_UEFI_FIRMWARE)) {
    $vmmArgs += "--uefi"
} else {
    throw "UEFI firmware (MSVM.fd) not found at '$PSScriptRoot\firmware\MSVM.fd'. Download a build artifact containing firmware\MSVM.fd or pass -UefiFirmware <path>."
}

$vmmArgs += @(
    "-m", $Memory,
    "-p", "$Processors",
    "--vmbus-scsi", "id=scsi0"
)

$baseDiskSpec = if ($CreateDiskSize -and -not (Test-Path -LiteralPath $DiskPath)) {
    "file:${DiskPath};create=${CreateDiskSize}"
} else {
    "file:${DiskPath}"
}
$diskSpec = if ($ReadOnlyDiff) {
    "memdiff:${baseDiskSpec},on=scsi0"
} else {
    "${baseDiskSpec},on=scsi0"
}
$vmmArgs += @("--disk", $diskSpec)

if (-not [string]::IsNullOrWhiteSpace($IsoPath)) {
    $resolvedIso = (Resolve-Path -LiteralPath $IsoPath).Path
    $vmmArgs += @("--disk", "file:${resolvedIso},dvd,on=scsi0")
}

if (-not $NoNet) {
    $vmmArgs += @("--net", "consomme")
}

if (-not $SerialConsole) {
    $vmmArgs += @("--com1", "null")
}

$vmmArgs += @(
    "--fastdesk",
    "--vram-mb", "$VramMb",
    "--fastdesk-shm", $ShmPath
)

if ($UseHvSock) {
    $vmmArgs += @("--fastdesk-vsock-port", "$VsockPort")
    $viewerTarget = "hvsock://loopback/$VsockPort"
} else {
    $vmmArgs += @("--fastdesk-listen", $ListenAddr)
    $viewerTarget = "tcp://$ListenAddr"
}

$quotedVmmArgs = ($vmmArgs | ForEach-Object { Format-ProcessArgument $_ }) -join " "
Write-Host "Starting OpenVMM FastDesk ($VramMb MiB VRAM, SHM=$ShmPath)..." -ForegroundColor Cyan
$vmmProc = Start-Process -FilePath $resolvedOpenVmm -ArgumentList $quotedVmmArgs -PassThru -NoNewWindow

try {
    Start-Sleep -Milliseconds 700
    if ($vmmProc.HasExited) {
        throw "openvmm.exe exited prematurely with code $($vmmProc.ExitCode)."
    }

    if (-not [string]::IsNullOrWhiteSpace($resolvedViewer)) {
        Write-Host "Launching fastdesk-viewer -> $viewerTarget" -ForegroundColor Green
        & $resolvedViewer --connect $viewerTarget
    } else {
        Write-Warning "fastdesk-viewer.exe not found; OpenVMM is running (PID=$($vmmProc.Id)) on $viewerTarget."
        $vmmProc.WaitForExit()
    }
} finally {
    if ($null -ne $vmmProc -and -not $vmmProc.HasExited) {
        Write-Host "Stopping OpenVMM (PID=$($vmmProc.Id))..." -ForegroundColor Yellow
        Stop-Process -Id $vmmProc.Id -Force -ErrorAction SilentlyContinue
    }
}
