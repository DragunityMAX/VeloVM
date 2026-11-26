<#
.SYNOPSIS
    Launches a Windows VM under VeloVM (OpenVMM WHP + FastDesk zero-copy SHM display)
    and connects velovm-viewer.exe.

.DESCRIPTION
    Auto-discovers openvmm.exe, velovm-viewer.exe, and firmware\MSVM.fd next to
    this script (or in local build/package directories), pre-allocates the shared
    VRAM file for zero-copy local display, attaches the primary VHDX/IMG (and an
    optional installer ISO) on a VMBus SCSI controller, and starts the viewer.

.EXAMPLE
    .\Start-VeloVM.ps1 -DiskPath C:\VMs\win11.vhdx -Memory 8G -Processors 4

.EXAMPLE
    .\Start-VeloVM.ps1 -DiskPath C:\VMs\win11.img -CreateDiskSize 64G -IsoPath C:\ISOs\Win11.iso -Memory 8G -Processors 4
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
    [string]$ShmPath = "",
    [int]$StartupTimeoutMs = 30000,
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

function Resolve-ListenEndpoint {
    param(
        [string]$Addr,
        [int]$DefaultPort = 5901
    )
    $trimmed = $Addr.Trim()
    $viewerAddr = $trimmed
    $vmmListenAddr = $trimmed
    $listenPort = $DefaultPort
    $listenIp = $null

    if ($trimmed -match '^\[([^\]]+)\]:(\d+)$') {
        $hostPart = $Matches[1]
        $listenPort = [int]$Matches[2]
        [System.Net.IPAddress]$parsed = $null
        if ([System.Net.IPAddress]::TryParse($hostPart, [ref]$parsed)) {
            $listenIp = $parsed
        }
    } elseif ($trimmed -match '^([^:]+):(\d+)$') {
        $hostPart = $Matches[1]
        $listenPort = [int]$Matches[2]
        [System.Net.IPAddress]$parsed = $null
        if ([System.Net.IPAddress]::TryParse($hostPart, [ref]$parsed)) {
            $listenIp = $parsed
        }
    } else {
        $bare = $trimmed.Trim('[', ']')
        [System.Net.IPAddress]$parsed = $null
        if ([System.Net.IPAddress]::TryParse($bare, [ref]$parsed)) {
            $listenIp = $parsed
            if ($parsed.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetworkV6) {
                $viewerAddr = "[$bare]:$DefaultPort"
                $vmmListenAddr = "[$bare]:$DefaultPort"
            } else {
                $viewerAddr = "${bare}:$DefaultPort"
                $vmmListenAddr = "${bare}:$DefaultPort"
            }
        } else {
            $viewerAddr = "${trimmed}:$DefaultPort"
        }
    }

    if ($null -ne $listenIp) {
        if ($listenIp.Equals([System.Net.IPAddress]::Any) -or
            ($listenIp.IsIPv4MappedToIPv6 -and $listenIp.MapToIPv4().Equals([System.Net.IPAddress]::Any))) {
            $viewerAddr = "127.0.0.1:$listenPort"
        } elseif ($listenIp.Equals([System.Net.IPAddress]::IPv6Any)) {
            $viewerAddr = "[::1]:$listenPort"
        }
    }

    return [PSCustomObject]@{
        ListenAddr = $vmmListenAddr
        ViewerAddr = $viewerAddr
        Port       = $listenPort
        IpAddress  = $listenIp
    }
}

function Test-FastDeskTcpListening {
    param(
        [int]$Port,
        [System.Net.IPAddress]$IpAddress
    )
    $listeners = [System.Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()
    foreach ($ep in $listeners) {
        if ($ep.Port -ne $Port) {
            continue
        }
        if ($null -eq $IpAddress -or
            $IpAddress.Equals([System.Net.IPAddress]::Any) -or
            $IpAddress.Equals([System.Net.IPAddress]::IPv6Any) -or
            ($IpAddress.IsIPv4MappedToIPv6 -and $IpAddress.MapToIPv4().Equals([System.Net.IPAddress]::Any))) {
            return $true
        }
        if ($ep.Address.Equals($IpAddress) -or
            $ep.Address.Equals([System.Net.IPAddress]::Any) -or
            $ep.Address.Equals([System.Net.IPAddress]::IPv6Any)) {
            return $true
        }
        if ($IpAddress.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetwork -and
            $ep.Address.IsIPv4MappedToIPv6 -and
            $ep.Address.MapToIPv4().Equals($IpAddress)) {
            return $true
        }
    }
    return $false
}

# Verify Windows Hypervisor Platform (WHP) is enabled.
if (Get-Command Get-WindowsOptionalFeature -ErrorAction SilentlyContinue) {
    $whpFeature = Get-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -ErrorAction SilentlyContinue
    if ($null -ne $whpFeature -and $whpFeature.State -ne "Enabled") {
        Write-Warning "Windows Hypervisor Platform (HypervisorPlatform) is currently $($whpFeature.State)."
        Write-Warning "Enable it from an elevated PowerShell prompt with:"
        Write-Warning "  Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform"
    }
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
    (Join-Path $PSScriptRoot "velovm-viewer.exe"),
    ".\velovm-viewer.exe",
    (Join-Path $PSScriptRoot "..\velovm-viewer\target\release\velovm-viewer.exe")
)

$resolvedUefi = Resolve-FirstExisting -ExplicitPath $UefiFirmware -Candidates @(
    (Join-Path $PSScriptRoot "firmware\MSVM.fd"),
    ".\firmware\MSVM.fd",
    (Join-Path $PSScriptRoot "..\openvmm\.packages\hyperv.uefi.mscoreuefi.x64.RELEASE\MsvmX64\RELEASE_VS2022\FV\MSVM.fd")
)

$uefiArgs = @()
if (-not [string]::IsNullOrWhiteSpace($resolvedUefi)) {
    $uefiArgs = @("--uefi", "firmware=$resolvedUefi")
} elseif (-not [string]::IsNullOrWhiteSpace($env:X86_64_OPENVMM_UEFI_FIRMWARE) -and (Test-Path -LiteralPath $env:X86_64_OPENVMM_UEFI_FIRMWARE)) {
    $uefiArgs = @("--uefi")
} else {
    throw "UEFI firmware (MSVM.fd) not found at '$PSScriptRoot\firmware\MSVM.fd'. Download a build artifact containing firmware\MSVM.fd or pass -UefiFirmware <path>."
}

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

$autoShm = [string]::IsNullOrWhiteSpace($ShmPath)
$vmmProc = $null

try {
    # Prepare the SHM path without pre-creating the file; openvmm.exe creates it
    # exclusively with CreateNew when initializing FastDesk VRAM.
    if ($autoShm) {
        $shmDir = Join-Path ([System.IO.Path]::GetTempPath()) "velovm"
        if (-not (Test-Path -LiteralPath $shmDir)) {
            New-Item -ItemType Directory -Path $shmDir -Force | Out-Null
        }
        $ShmPath = Join-Path $shmDir ("vram-{0}-{1}.shm" -f $PID, [Guid]::NewGuid().ToString("N"))
    } else {
        $shmDir = Split-Path -Parent $ShmPath
        if ($shmDir -and -not (Test-Path -LiteralPath $shmDir)) {
            New-Item -ItemType Directory -Path $shmDir -Force | Out-Null
        }
        if (Test-Path -LiteralPath $ShmPath) {
            $existingShm = Get-Item -LiteralPath $ShmPath -Force
            if (($existingShm.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or $existingShm.PSIsContainer) {
                throw "Refusing unsafe non-regular or symlinked FastDesk SHM path: $ShmPath"
            }
            Remove-Item -LiteralPath $ShmPath -Force
        }
    }

    $vmmArgs = @()
    $vmmArgs += $uefiArgs
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

    $tcpEndpoint = $null
    if ($UseHvSock) {
        $vmmArgs += @("--fastdesk-vsock-port", "$VsockPort")
        $viewerTarget = "hvsock://loopback/$VsockPort"
    } else {
        $tcpEndpoint = Resolve-ListenEndpoint -Addr $ListenAddr
        $vmmArgs += @("--fastdesk-listen", $tcpEndpoint.ListenAddr)
        $viewerTarget = "tcp://$($tcpEndpoint.ViewerAddr)"
    }

    $quotedVmmArgs = ($vmmArgs | ForEach-Object { Format-ProcessArgument $_ }) -join " "
    Write-Host "Starting VeloVM ($VramMb MiB VRAM, SHM=$ShmPath)..." -ForegroundColor Cyan
    $vmmProc = Start-Process -FilePath $resolvedOpenVmm -ArgumentList $quotedVmmArgs -PassThru -NoNewWindow

    $deadline = (Get-Date).AddMilliseconds($StartupTimeoutMs)
    if (-not $UseHvSock) {
        Write-Host "Waiting for VeloVM FastDesk listener on $($tcpEndpoint.ListenAddr)..." -ForegroundColor Cyan
        while ($true) {
            $vmmProc.Refresh()
            if ($vmmProc.HasExited) {
                throw "openvmm.exe exited prematurely with code $($vmmProc.ExitCode)."
            }
            if (Test-FastDeskTcpListening -Port $tcpEndpoint.Port -IpAddress $tcpEndpoint.IpAddress) {
                break
            }
            if ((Get-Date) -ge $deadline) {
                $vmmProc.Refresh()
                if ($vmmProc.HasExited) {
                    throw "openvmm.exe exited prematurely with code $($vmmProc.ExitCode)."
                }
                throw "Timed out waiting for VeloVM FastDesk listener on $($tcpEndpoint.ListenAddr)."
            }
            Start-Sleep -Milliseconds 50
        }
    } else {
        $vmmProc.Refresh()
        if ($vmmProc.HasExited) {
            throw "openvmm.exe exited prematurely with code $($vmmProc.ExitCode)."
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($resolvedViewer)) {
        Write-Host "Launching velovm-viewer -> $viewerTarget" -ForegroundColor Green
        while ($true) {
            $attemptStart = Get-Date
            & $resolvedViewer --connect $viewerTarget
            $viewerExitCode = if ($null -eq $LASTEXITCODE) { 0 } else { $LASTEXITCODE }
            if ($viewerExitCode -eq 0) {
                break
            }
            $vmmProc.Refresh()
            if ($vmmProc.HasExited) {
                throw "openvmm.exe exited prematurely with code $($vmmProc.ExitCode)."
            }
            $elapsedMs = ((Get-Date) - $attemptStart).TotalMilliseconds
            if ($elapsedMs -ge 2000 -or (Get-Date) -ge $deadline) {
                throw "velovm-viewer.exe exited with code $viewerExitCode."
            }
            Start-Sleep -Milliseconds 100
        }
    } else {
        Write-Warning "velovm-viewer.exe not found; OpenVMM is running (PID=$($vmmProc.Id)) on $viewerTarget."
        $vmmProc.WaitForExit()
    }
} finally {
    if ($null -ne $vmmProc -and -not $vmmProc.HasExited) {
        Write-Host "Stopping OpenVMM (PID=$($vmmProc.Id))..." -ForegroundColor Yellow
        Stop-Process -Id $vmmProc.Id -Force -ErrorAction SilentlyContinue
        try { $vmmProc.WaitForExit(5000) | Out-Null } catch {}
    }
    if ($autoShm -and -not [string]::IsNullOrWhiteSpace($ShmPath) -and (Test-Path -LiteralPath $ShmPath)) {
        Remove-Item -LiteralPath $ShmPath -Force -ErrorAction SilentlyContinue
    }
}
