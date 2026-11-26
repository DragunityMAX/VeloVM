<#
.SYNOPSIS
    Configures GPU-PV (GPU Partitioning) + HostDriverStore sync + AF_HYPERV
    GuestCommunicationServices registration for a Hyper-V Windows VM.

.DESCRIPTION
    Run from an elevated (Administrator) PowerShell prompt on the Windows 10/11
    or Windows Server Hyper-V host.

    1. Registers the FastDesk AF_HYPERV service GUID
       ({0000170d-facb-11e6-bd58-64006a7986d3}, corresponding to vsock port 5901)
       under Hyper-V GuestCommunicationServices so host/guest AF_HYPERV streams
       succeed without firewall/network overhead.
    2. Unless -RegisterHvsockOnly is set, assigns a GPU-PV partition adapter to
       -VmName, sets MMIO space required by WDDM/GPU-PV, and copies matching host
       GPU driver packages into the guest VHDX HostDriverStore.

.EXAMPLE
    .\Setup-HyperVGpuPvAndHvsock.ps1 -VmName "Win11-Dev" -PartitionPercent 50
#>
[CmdletBinding()]
param(
    [string]$VmName = "",

    [ValidateRange(10, 100)]
    [int]$PartitionPercent = 50,

    [string]$GpuNameFilter = "*",

    [int]$VsockPort = 5901,

    [switch]$RegisterHvsockOnly,

    [switch]$SkipDriverCopy
)

$ErrorActionPreference = "Stop"

function Assert-Administrator {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]$id
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "This script must be run from an elevated Administrator PowerShell session."
    }
}

function Resolve-DisplayDriverInfNames {
    param(
        [string[]]$InfNames = @(),
        [hashtable]$PublishedDriverMap = @{}
    )

    $resolved = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($raw in $InfNames) {
        if ([string]::IsNullOrWhiteSpace($raw)) { continue }
        $fileName = ($raw.Trim() -split '[\\/]')[-1].ToLowerInvariant()
        if ($PublishedDriverMap.ContainsKey($fileName)) {
            $orig = (([string]$PublishedDriverMap[$fileName]).Trim() -split '[\\/]')[-1].ToLowerInvariant()
            if (-not [string]::IsNullOrWhiteSpace($orig)) {
                $null = $resolved.Add($orig)
            }
        }
        $null = $resolved.Add($fileName)
    }
    return @($resolved)
}

function Get-PublishedDriverOriginalInfMap {
    $map = @{}
    if (Get-Command Get-WindowsDriver -ErrorAction SilentlyContinue) {
        try {
            foreach ($drv in (Get-WindowsDriver -Online -ErrorAction Stop)) {
                if ($drv.Driver -and $drv.OriginalFileName) {
                    $pub = [System.IO.Path]::GetFileName([string]$drv.Driver).ToLowerInvariant()
                    $orig = [System.IO.Path]::GetFileName([string]$drv.OriginalFileName).ToLowerInvariant()
                    if ($pub -and $orig) {
                        $map[$pub] = $orig
                    }
                }
            }
            if ($map.Count -gt 0) {
                return $map
            }
        } catch {}
    }
    if (Get-Command pnputil.exe -ErrorAction SilentlyContinue) {
        try {
            $curPub = $null
            foreach ($line in (& pnputil.exe /enum-drivers 2>$null)) {
                if ($line -match '^\s*Published Name\s*:\s*(\S+\.inf)\s*$') {
                    $curPub = [System.IO.Path]::GetFileName($Matches[1].Trim()).ToLowerInvariant()
                } elseif ($curPub -and $line -match '^\s*Original Name\s*:\s*(\S+\.inf)\s*$') {
                    $map[$curPub] = [System.IO.Path]::GetFileName($Matches[1].Trim()).ToLowerInvariant()
                    $curPub = $null
                }
            }
        } catch {}
    }
    return $map
}

function Sync-HostDriverStorePackages {
    param(
        [Parameter(Mandatory)]
        [string]$HostRepo,
        [Parameter(Mandatory)]
        [string]$GuestRepo,
        [string[]]$DisplayDriverInfNames = @(),
        [hashtable]$PublishedDriverMap = @{}
    )

    New-Item -ItemType Directory -Path $GuestRepo -Force | Out-Null
    $resolvedInfNames = Resolve-DisplayDriverInfNames -InfNames $DisplayDriverInfNames -PublishedDriverMap $PublishedDriverMap

    $pkgDirs = Get-ChildItem -Path $HostRepo -Directory | Where-Object {
        $name = $_.Name.ToLowerInvariant()
        if ($name -match '^(nv|amd|ati|igfx|iigd|ialm|u0|qxl)') {
            return $true
        }
        foreach ($infName in $resolvedInfNames) {
            $base = [System.IO.Path]::GetFileNameWithoutExtension($infName).ToLowerInvariant()
            if ($name.StartsWith($infName + "_") -or $name -eq $infName -or $name.StartsWith($base + ".inf_") -or $name.StartsWith($base + "_")) {
                return $true
            }
        }
        return $false
    }

    foreach ($dir in $pkgDirs) {
        $dest = Join-Path $GuestRepo $dir.Name
        if (Test-Path -LiteralPath $dest) {
            Remove-Item -LiteralPath $dest -Recurse -Force
        }
        Write-Host "  Copying driver package: $($dir.Name)"
        Copy-Item -LiteralPath $dir.FullName -Destination $dest -Recurse -Force
    }
    return @($pkgDirs | ForEach-Object { $_.Name })
}

function Get-ScaledGpuPartitionLimits {
    param(
        [Parameter(Mandatory)]
        [uint64]$Min,
        [Parameter(Mandatory)]
        [uint64]$Max,
        [Parameter(Mandatory)]
        [uint64]$Optimal,
        [Parameter(Mandatory)]
        [ValidateRange(10, 100)]
        [int]$PartitionPercent
    )

    # Use [decimal] (96-bit exact integer mantissa) instead of IEEE-754 double
    # so [uint64]::MaxValue does not round up to 2^64 and overflow on cast.
    $scaled = [Math]::Floor(([decimal]$Optimal * [decimal]$PartitionPercent) / [decimal]100)
    $clamped = [uint64]([Math]::Max([decimal]$Min, [Math]::Min([decimal]$Max, $scaled)))
    return [pscustomobject]@{
        Min     = $Min
        Max     = $Max
        Optimal = $clamped
    }
}

if ($MyInvocation.InvocationName -ne '.') {
Assert-Administrator

# 1. Register AF_HYPERV GuestCommunicationServices GUID for FastDesk (port 5901 default).
$serviceGuid = "{0}{1}" -f ("{0:x8}" -f $VsockPort), "-facb-11e6-bd58-64006a7986d3"
$regBase = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices"
$svcKey = Join-Path $regBase $serviceGuid

if (-not (Test-Path $regBase)) {
    New-Item -Path $regBase -Force | Out-Null
}
if (-not (Test-Path $svcKey)) {
    New-Item -Path $svcKey -Force | Out-Null
}
Set-ItemProperty -Path $svcKey -Name "ElementName" -Value "VeloVM FastDesk Display Stream" -Type String
Write-Host "Registered AF_HYPERV GuestCommunicationServices GUID: $serviceGuid" -ForegroundColor Green

if ($RegisterHvsockOnly) {
    return
}

if (-not $VmName) {
    throw "Specify -VmName <VM> (or pass -RegisterHvsockOnly to only register the AF_HYPERV GUID)."
}

$vm = Get-VM -Name $VmName
if ($vm.State -ne "Off") {
    throw "VM '$VmName' must be turned Off before configuring GPU-PV and copying drivers (current state: $($vm.State))."
}

# 2. Pick partitionable host GPU and configure GPU-PV on the Hyper-V VM.
$gpus = Get-VMHostPartitionableGpu | Where-Object { $_.Name -like $GpuNameFilter }
if (-not $gpus) {
    throw "No partitionable host GPU matched '$GpuNameFilter'. Verify WDDM 2.5+ GPU drivers and Hyper-V support."
}
$gpu = $gpus | Select-Object -First 1
Write-Host "Selected partitionable GPU: $($gpu.Name)" -ForegroundColor Cyan

$existing = Get-VMGpuPartitionAdapter -VMName $VmName -ErrorAction SilentlyContinue
if ($existing) {
    Remove-VMGpuPartitionAdapter -VMName $VmName
}
Add-VMGpuPartitionAdapter -VMName $VmName -InstancePath $gpu.Name

$vramLimits = Get-ScaledGpuPartitionLimits -Min ([uint64]$gpu.MinPartitionVRAM) -Max ([uint64]$gpu.MaxPartitionVRAM) -Optimal ([uint64]$gpu.OptimalPartitionVRAM) -PartitionPercent $PartitionPercent
$encLimits = Get-ScaledGpuPartitionLimits -Min ([uint64]$gpu.MinPartitionEncode) -Max ([uint64]$gpu.MaxPartitionEncode) -Optimal ([uint64]$gpu.OptimalPartitionEncode) -PartitionPercent $PartitionPercent
$decLimits = Get-ScaledGpuPartitionLimits -Min ([uint64]$gpu.MinPartitionDecode) -Max ([uint64]$gpu.MaxPartitionDecode) -Optimal ([uint64]$gpu.OptimalPartitionDecode) -PartitionPercent $PartitionPercent
$compLimits = Get-ScaledGpuPartitionLimits -Min ([uint64]$gpu.MinPartitionCompute) -Max ([uint64]$gpu.MaxPartitionCompute) -Optimal ([uint64]$gpu.OptimalPartitionCompute) -PartitionPercent $PartitionPercent

Set-VMGpuPartitionAdapter -VMName $VmName `
    -MinPartitionVRAM $vramLimits.Min -MaxPartitionVRAM $vramLimits.Max -OptimalPartitionVRAM $vramLimits.Optimal `
    -MinPartitionEncode $encLimits.Min -MaxPartitionEncode $encLimits.Max -OptimalPartitionEncode $encLimits.Optimal `
    -MinPartitionDecode $decLimits.Min -MaxPartitionDecode $decLimits.Max -OptimalPartitionDecode $decLimits.Optimal `
    -MinPartitionCompute $compLimits.Min -MaxPartitionCompute $compLimits.Max -OptimalPartitionCompute $compLimits.Optimal

Set-VM -VMName $VmName -GuestControlledCacheTypes $true -LowMemoryMappedIoSpace 1GB -HighMemoryMappedIoSpace 32GB
Write-Host "Configured GPU-PV ($PartitionPercent%) and 32GB High MMIO space for '$VmName'." -ForegroundColor Green

# 3. Copy host GPU drivers into Guest:\Windows\System32\HostDriverStore\FileRepository.
if (-not $SkipDriverCopy) {
    $osDrive = Get-VMHardDiskDrive -VMName $VmName | Select-Object -First 1
    if (-not $osDrive -or -not (Test-Path $osDrive.Path)) {
        throw "Could not find primary VHDX path for '$VmName'."
    }
    $vhdxPath = $osDrive.Path
    Write-Host "Mounting VHDX '$vhdxPath' to sync HostDriverStore..." -ForegroundColor Cyan

    $mounted = Mount-VHD -Path $vhdxPath -PassThru
    try {
        $disk = if ($null -ne $mounted.DiskNumber) {
            Get-Disk -Number $mounted.DiskNumber
        } else {
            $mounted | Get-Disk
        }
        $partitions = $disk | Get-Partition | Where-Object { $_.DriveLetter }
        $windowsRoot = $null
        foreach ($part in $partitions) {
            $candidate = "$($part.DriveLetter):\Windows\System32"
            if (Test-Path $candidate) {
                $windowsRoot = "$($part.DriveLetter):\Windows"
                break
            }
        }
        if (-not $windowsRoot) {
            throw "Could not locate Windows partition inside VHDX '$vhdxPath'."
        }

        $guestRepo = Join-Path $windowsRoot "System32\HostDriverStore\FileRepository"
        New-Item -ItemType Directory -Path $guestRepo -Force | Out-Null

        $displayDrivers = @(Get-CimInstance Win32_VideoController | ForEach-Object { $_.InfName } | Where-Object { $_ } | Select-Object -Unique)
        $publishedDriverMap = Get-PublishedDriverOriginalInfMap
        $hostRepo = "$env:SystemRoot\System32\DriverStore\FileRepository"
        $hostDrv = "$env:SystemRoot\System32\drivers"
        $guestDrv = Join-Path $windowsRoot "System32\drivers"

        # Copy all display-class driver packages referenced by active display adapters (plus nv* / amd* / igfx* / iigd* if present).
        $null = Sync-HostDriverStorePackages `
            -HostRepo $hostRepo `
            -GuestRepo $guestRepo `
            -DisplayDriverInfNames $displayDrivers `
            -PublishedDriverMap $publishedDriverMap

        # Copy NVIDIA host user-mode/kernel API DLLs if present (required by NVIDIA GPU-PV).
        foreach ($pattern in @("nvapi*.dll", "nvcuda*.dll", "nvdlist*.dll", "nvofapi*.dll", "nvencodeapi*.dll", "nvlddmkm.sys")) {
            Get-ChildItem -Path "$env:SystemRoot\System32" -Filter $pattern -ErrorAction SilentlyContinue | ForEach-Object {
                Copy-Item -Path $_.FullName -Destination (Join-Path $windowsRoot "System32\$($_.Name)") -Force
            }
            Get-ChildItem -Path $hostDrv -Filter $pattern -ErrorAction SilentlyContinue | ForEach-Object {
                Copy-Item -Path $_.FullName -Destination (Join-Path $guestDrv $_.Name) -Force
            }
        }

        Write-Host "HostDriverStore sync complete." -ForegroundColor Green
    } finally {
        Dismount-VHD -Path $vhdxPath
    }
}

Write-Host "Done. Next: Set guest registry HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\DWMFRAMEINTERVAL = 15 (DWORD) if also using RDP, or launch FastDesk over hvsock://$($vm.Id)/$VsockPort." -ForegroundColor Cyan
}
