#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$launcher = Join-Path (Split-Path -Parent $scriptDir) "Start-VeloVM.ps1"

$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($launcher, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors -and $parseErrors.Count -gt 0) {
    throw "Failed to parse Start-VeloVM.ps1: $($parseErrors[0].Message)"
}

foreach ($fnName in @("Test-EndpointAddressMatch", "Get-ProcessTreeIds", "Test-AnyTcpListening", "Test-FastDeskTcpListening")) {
    $fnAst = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $fnName }, $true)
    if ($null -eq $fnAst) {
        throw "$fnName not found in Start-VeloVM.ps1"
    }
    . ([ScriptBlock]::Create($fnAst.Extent.Text))
}

# Test 1: Get-ProcessTreeIds walks parent -> child -> grandchild.
$procs = @(
    [PSCustomObject]@{ ProcessId = 100; ParentProcessId = 1 },
    [PSCustomObject]@{ ProcessId = 200; ParentProcessId = 100 },
    [PSCustomObject]@{ ProcessId = 300; ParentProcessId = 200 },
    [PSCustomObject]@{ ProcessId = 999; ParentProcessId = 1 }
)
$tree = Get-ProcessTreeIds -RootPid 100 -Processes $procs
if ($tree -notcontains 100 -or $tree -notcontains 200 -or $tree -notcontains 300 -or $tree -contains 999) {
    throw "Unexpected process tree: $($tree -join ', ')"
}

# Test 2: Test-FastDeskTcpListening rejects unrelated OwningProcess and accepts owned/child OwningProcess.
$targetIp = [System.Net.IPAddress]::Loopback
$otherConns = @(
    [PSCustomObject]@{ LocalPort = 5901; LocalAddress = "127.0.0.1"; OwningProcess = 999 }
)
if (Test-FastDeskTcpListening -Port 5901 -IpAddress $targetIp -ProcessIds $tree -Connections $otherConns) {
    throw "Expected Test-FastDeskTcpListening to reject unrelated OwningProcess 999"
}

$childConns = @(
    [PSCustomObject]@{ LocalPort = 5901; LocalAddress = "127.0.0.1"; OwningProcess = 200 }
)
if (-not (Test-FastDeskTcpListening -Port 5901 -IpAddress $targetIp -ProcessIds $tree -Connections $childConns)) {
    throw "Expected Test-FastDeskTcpListening to accept child OwningProcess 200"
}

$wrongIpConns = @(
    [PSCustomObject]@{ LocalPort = 5901; LocalAddress = "127.0.0.2"; OwningProcess = 100 }
)
if (Test-FastDeskTcpListening -Port 5901 -IpAddress $targetIp -ProcessIds $tree -Connections $wrongIpConns) {
    throw "Expected Test-FastDeskTcpListening to reject mismatched IP 127.0.0.2"
}

# Test 3: Test-AnyTcpListening detects a real live TCP listener on 127.0.0.1.
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$listener.Start()
try {
    $boundPort = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    if (-not (Test-AnyTcpListening -Port $boundPort -IpAddress ([System.Net.IPAddress]::Loopback))) {
        throw "Expected Test-AnyTcpListening to detect live listener on 127.0.0.1:$boundPort"
    }
} finally {
    $listener.Stop()
}

Write-Host "PASS: Start-VeloVM.ps1 readiness ownership checks passed"
