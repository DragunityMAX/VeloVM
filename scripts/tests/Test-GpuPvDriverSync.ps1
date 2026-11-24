#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$parentDir = Split-Path -Parent $scriptDir
. (Join-Path $parentDir "Setup-HyperVGpuPvAndHvsock.ps1")

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("gpupv-sync-test-" + [Guid]::NewGuid().ToString("N"))
try {
    $hostRepo = Join-Path $tempRoot "host"
    $guestRepo = Join-Path $tempRoot "guest"

    $amdPkg = "c0398123.inf_amd64_1234567890abcdef"
    $nvPkg = "nv_dispig.inf_amd64_abcdef1234567890"
    $unrelatedPkg = "e1d68x64.inf_amd64_1111222233334444"

    $hostAmd = Join-Path $hostRepo $amdPkg
    $hostNv = Join-Path $hostRepo $nvPkg
    $hostUnrelated = Join-Path $hostRepo $unrelatedPkg
    $guestAmd = Join-Path $guestRepo $amdPkg
    $guestNv = Join-Path $guestRepo $nvPkg

    New-Item -ItemType Directory -Path $hostAmd, $hostNv, $hostUnrelated -Force | Out-Null
    Set-Content -Path (Join-Path $hostAmd "driver.sys") -Value "v1"
    Set-Content -Path (Join-Path $hostNv "nv.sys") -Value "nv1"
    Set-Content -Path (Join-Path $hostUnrelated "nic.sys") -Value "nic"

    # Simulate stale leftover file in guest package to ensure clean replacement.
    New-Item -ItemType Directory -Path $guestAmd -Force | Out-Null
    Set-Content -Path (Join-Path $guestAmd "stale.txt") -Value "stale"

    $pubMap = @{
        "oem42.inf" = "C:\Windows\System32\DriverStore\FileRepository\$amdPkg\c0398123.inf"
    }

    $copied1 = Sync-HostDriverStorePackages -HostRepo $hostRepo -GuestRepo $guestRepo -DisplayDriverInfNames @("oem42.inf") -PublishedDriverMap $pubMap
    if ($copied1 -notcontains $amdPkg -or $copied1 -notcontains $nvPkg -or $copied1 -contains $unrelatedPkg) {
        throw "Unexpected copied packages on pass 1: $($copied1 -join ', ')"
    }

    if (-not (Test-Path (Join-Path $guestAmd "driver.sys"))) {
        throw "Expected $amdPkg/driver.sys after pass 1"
    }
    if (Test-Path (Join-Path $guestAmd "stale.txt")) {
        throw "Expected stale.txt to be removed on clean replace"
    }

    # Update host driver content and run pass 2; must replace cleanly without nesting <pkg>/<pkg>.
    Set-Content -Path (Join-Path $hostAmd "driver.sys") -Value "v2"
    $null = Sync-HostDriverStorePackages -HostRepo $hostRepo -GuestRepo $guestRepo -DisplayDriverInfNames @("oem42.inf") -PublishedDriverMap $pubMap

    if (Test-Path (Join-Path $guestAmd $amdPkg)) {
        throw "Nested package directory detected: $amdPkg/$amdPkg"
    }
    if (Test-Path (Join-Path $guestNv $nvPkg)) {
        throw "Nested package directory detected: $nvPkg/$nvPkg"
    }
    $v2Content = (Get-Content -Raw (Join-Path $guestAmd "driver.sys")).Trim()
    if ($v2Content -ne "v2") {
        throw "Expected updated driver.sys content 'v2', got '$v2Content'"
    }

    Write-Host "PASS: Setup-HyperVGpuPvAndHvsock.ps1 driver sync tests passed"
} finally {
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force
    }
}
