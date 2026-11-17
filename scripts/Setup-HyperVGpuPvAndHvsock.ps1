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
Set-ItemProperty -Path $svcKey -Name "ElementName" -Value "OpenVMM FastDesk Display Stream" -Type String
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

$scale = $PartitionPercent / 100.0
$minVram = [uint64]($gpu.MinPartitionVRAM)
$maxVram = [uint64]($gpu.MaxPartitionVRAM)
$optVram = [uint64]([Math]::Max($minVram, [Math]::Min($maxVram, $gpu.OptimalPartitionVRAM * $scale)))

$minEnc = [uint64]($gpu.MinPartitionEncode)
$maxEnc = [uint64]($gpu.MaxPartitionEncode)
$optEnc = [uint64]([Math]::Max($minEnc, [Math]::Min($maxEnc, $gpu.OptimalPartitionEncode * $scale)))

$minDec = [uint64]($gpu.MinPartitionDecode)
$maxDec = [uint64]($gpu.MaxPartitionDecode)
$optDec = [uint64]([Math]::Max($minDec, [Math]::Min($maxDec, $gpu.OptimalPartitionDecode * $scale)))

$minComp = [uint64]($gpu.MinPartitionCompute)
$maxComp = [uint64]($gpu.MaxPartitionCompute)
$optComp = [uint64]([Math]::Max($minComp, [Math]::Min($maxComp, $gpu.OptimalPartitionCompute * $scale)))

Set-VMGpuPartitionAdapter -VMName $VmName `
    -MinPartitionVRAM $minVram -MaxPartitionVRAM $maxVram -OptimalPartitionVRAM $optVram `
    -MinPartitionEncode $minEnc -MaxPartitionEncode $maxEnc -OptimalPartitionEncode $optEnc `
    -MinPartitionDecode $minDec -MaxPartitionDecode $maxDec -OptimalPartitionDecode $optDec `
    -MinPartitionCompute $minComp -MaxPartitionCompute $maxComp -OptimalPartitionCompute $optComp

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

        $displayDrivers = Get-CimInstance Win32_VideoController | ForEach-Object { $_.InfName } | Where-Object { $_ } | Select-Object -Unique
        $hostRepo = "$env:SystemRoot\System32\DriverStore\FileRepository"
        $hostDrv = "$env:SystemRoot\System32\drivers"
        $guestDrv = Join-Path $windowsRoot "System32\drivers"

        # Copy all display-class driver packages referenced by active display adapters (plus nv* / amd* / igfx* / iigd* if present).
        $pkgDirs = Get-ChildItem -Path $hostRepo -Directory | Where-Object {
            $name = $_.Name.ToLowerInvariant()
            $name -match '^(nv|amd|ati|igfx|iigd|ialm|u0|qxl)' -or ($displayDrivers | Where-Object { $name.StartsWith([System.IO.Path]::GetFileNameWithoutExtension($_).ToLowerInvariant()) })
        }

        foreach ($dir in $pkgDirs) {
            $dest = Join-Path $guestRepo $dir.Name
            Write-Host "  Copying driver package: $($dir.Name)"
            Copy-Item -Path $dir.FullName -Destination $dest -Recurse -Force
        }

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
