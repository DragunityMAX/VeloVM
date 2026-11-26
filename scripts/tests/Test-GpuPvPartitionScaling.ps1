#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$parentDir = Split-Path -Parent $scriptDir
. (Join-Path $parentDir "Setup-HyperVGpuPvAndHvsock.ps1")

function Assert-Equal {
    param(
        [Parameter(Mandatory)] $Actual,
        [Parameter(Mandatory)] $Expected,
        [Parameter(Mandatory)] [string]$Label
    )
    if ($Actual -ne $Expected) {
        throw "FAIL ($Label): expected $Expected, got $Actual"
    }
}

# Regression: [uint64]::MaxValue at -PartitionPercent 100 must not overflow via double rounding.
$maxU64 = [uint64]::MaxValue
$resMax100 = Get-ScaledGpuPartitionLimits -Min ([uint64]0) -Max $maxU64 -Optimal $maxU64 -PartitionPercent 100
Assert-Equal $resMax100.Optimal $maxU64 "uint64-max-100pct"

# Value within 1024 of 2^64 (previously rounded to 2^64 in IEEE-754 double).
$nearMax = $maxU64 - 512
$resNear100 = Get-ScaledGpuPartitionLimits -Min ([uint64]0) -Max $maxU64 -Optimal $nearMax -PartitionPercent 100
Assert-Equal $resNear100.Optimal $nearMax "uint64-near-max-100pct"

# Exact decimal floor at 50% on uint64::MaxValue: (2^64 - 1) * 50 / 100 = 9223372036854775807.5 -> 9223372036854775807
$resMax50 = Get-ScaledGpuPartitionLimits -Min ([uint64]0) -Max $maxU64 -Optimal $maxU64 -PartitionPercent 50
Assert-Equal $resMax50.Optimal ([uint64]9223372036854775807) "uint64-max-50pct"

# Clamping against Min and Max.
$resMinClamp = Get-ScaledGpuPartitionLimits -Min ([uint64]1000) -Max ([uint64]5000) -Optimal ([uint64]2000) -PartitionPercent 25
Assert-Equal $resMinClamp.Optimal ([uint64]1000) "clamp-min"

$resMaxClamp = Get-ScaledGpuPartitionLimits -Min ([uint64]1000) -Max ([uint64]5000) -Optimal ([uint64]20000) -PartitionPercent 50
Assert-Equal $resMaxClamp.Optimal ([uint64]5000) "clamp-max"

Write-Host "PASS: Setup-HyperVGpuPvAndHvsock.ps1 GPU-PV uint64 partition scaling tests passed"
