#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$launcher = Join-Path (Split-Path -Parent $scriptDir) "Start-VeloVM.ps1"

# Extract Assert-VmmOptionSafePath via PowerShell AST without running the launcher.
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($launcher, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors -and $parseErrors.Count -gt 0) {
    throw "Failed to parse Start-VeloVM.ps1: $($parseErrors[0].Message)"
}
$fnAst = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq "Assert-VmmOptionSafePath" }, $true)
if ($null -eq $fnAst) {
    throw "Assert-VmmOptionSafePath not found in Start-VeloVM.ps1"
}
. ([ScriptBlock]::Create($fnAst.Extent.Text))

foreach ($bad in @(
    'D:\VMs\Win11, clean.vhdx',
    'D:\VMs\Win11;create=10G.vhdx',
    'D:\VMs\Win11[1].vhdx',
    'D:\VMs\Win11].vhdx',
    'D:\VMs\Win11[.vhdx'
)) {
    $threw = $false
    try {
        Assert-VmmOptionSafePath -Label "Disk image" -Path $bad
    } catch {
        $threw = $true
    }
    if (-not $threw) {
        throw "Expected Assert-VmmOptionSafePath to reject '$bad'"
    }
}

foreach ($good in @(
    'D:\VMs\Win11 clean.vhdx',
    '/tmp/dir-with_chars (1)/win11.vhdx'
)) {
    Assert-VmmOptionSafePath -Label "Disk image" -Path $good
}

# End-to-end launcher validation with stub files.
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("velovm-path-chars-" + [Guid]::NewGuid().ToString("N"))
try {
    New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null
    $stubVmm = Join-Path $tempRoot "openvmm.exe"
    $stubFw = Join-Path $tempRoot "MSVM.fd"
    $goodDisk = Join-Path $tempRoot "good.vhdx"
    $badDisk = Join-Path $tempRoot "Win11, clean.vhdx"
    $badIso = Join-Path $tempRoot "Win11;dvd.iso"
    $badFw = Join-Path $tempRoot "MSVM[1].fd"
    Set-Content -LiteralPath $stubVmm -Value ""
    Set-Content -LiteralPath $stubFw -Value ""
    Set-Content -LiteralPath $goodDisk -Value ""
    Set-Content -LiteralPath $badDisk -Value ""
    Set-Content -LiteralPath $badIso -Value ""
    Set-Content -LiteralPath $badFw -Value ""

    $diskRejected = $false
    try {
        & $launcher -DiskPath $badDisk -OpenVmmExe $stubVmm -UefiFirmware $stubFw
    } catch {
        if ($_.Exception.Message -like "*Disk image path cannot contain*") {
            $diskRejected = $true
        } else {
            throw
        }
    }
    if (-not $diskRejected) {
        throw "Expected Start-VeloVM.ps1 to reject disk path with comma"
    }

    $isoRejected = $false
    try {
        & $launcher -DiskPath $goodDisk -IsoPath $badIso -OpenVmmExe $stubVmm -UefiFirmware $stubFw
    } catch {
        if ($_.Exception.Message -like "*ISO image path cannot contain*") {
            $isoRejected = $true
        } else {
            throw
        }
    }
    if (-not $isoRejected) {
        throw "Expected Start-VeloVM.ps1 to reject ISO path with semicolon"
    }

    $fwRejected = $false
    try {
        & $launcher -DiskPath $goodDisk -OpenVmmExe $stubVmm -UefiFirmware $badFw
    } catch {
        if ($_.Exception.Message -like "*UEFI firmware path cannot contain*") {
            $fwRejected = $true
        } else {
            throw
        }
    }
    if (-not $fwRejected) {
        throw "Expected Start-VeloVM.ps1 to reject firmware path with brackets"
    }

    Write-Host "PASS: Start-VeloVM.ps1 path character validation checks passed"
} finally {
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force
    }
}
