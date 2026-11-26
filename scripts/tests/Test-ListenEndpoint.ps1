#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$launcherPath = Join-Path (Split-Path -Parent $scriptDir) "Start-VeloVM.ps1"

# Extract Resolve-ListenEndpoint via PowerShell AST without running Start-VeloVM.ps1.
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($launcherPath, [ref]$tokens, [ref]$errors)
if ($errors.Count -gt 0) {
    throw "Parse errors in Start-VeloVM.ps1: $($errors | Out-String)"
}
$fnAst = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq "Resolve-ListenEndpoint"
}, $true)
if ($null -eq $fnAst) {
    throw "Could not find Resolve-ListenEndpoint in Start-VeloVM.ps1"
}
Invoke-Expression $fnAst.Extent.Text

function Assert-Equal {
    param(
        [Parameter(Mandatory)] $Actual,
        [Parameter(Mandatory)] $Expected,
        [Parameter(Mandatory)] [string]$Label
    )
    if ($Actual -ne $Expected) {
        throw "FAIL ($Label): expected '$Expected', got '$Actual'"
    }
}

$ep1 = Resolve-ListenEndpoint -Addr "[::1]"
Assert-Equal $ep1.ListenAddr "[::1]:5901" "bracketed-ipv6-vmm"
Assert-Equal $ep1.ViewerAddr "[::1]:5901" "bracketed-ipv6-viewer"
Assert-Equal $ep1.Port 5901 "bracketed-ipv6-port"

$ep2 = Resolve-ListenEndpoint -Addr "  [::1]  "
Assert-Equal $ep2.ListenAddr "[::1]:5901" "trimmed-bracketed-ipv6-vmm"
Assert-Equal $ep2.ViewerAddr "[::1]:5901" "trimmed-bracketed-ipv6-viewer"

$ep3 = Resolve-ListenEndpoint -Addr "[::]"
Assert-Equal $ep3.ListenAddr "[::]:5901" "bracketed-ipv6-any-vmm"
Assert-Equal $ep3.ViewerAddr "[::1]:5901" "bracketed-ipv6-any-viewer"

$ep4 = Resolve-ListenEndpoint -Addr "[::1]:5905"
Assert-Equal $ep4.ListenAddr "[::1]:5905" "bracketed-ipv6-port-vmm"
Assert-Equal $ep4.ViewerAddr "[::1]:5905" "bracketed-ipv6-port-viewer"
Assert-Equal $ep4.Port 5905 "bracketed-ipv6-port-num"

$ep5 = Resolve-ListenEndpoint -Addr " 127.0.0.1 "
Assert-Equal $ep5.ListenAddr "127.0.0.1:5901" "trimmed-ipv4-vmm"
Assert-Equal $ep5.ViewerAddr "127.0.0.1:5901" "trimmed-ipv4-viewer"

Write-Host "PASS: Start-VeloVM.ps1 Resolve-ListenEndpoint tests passed"
