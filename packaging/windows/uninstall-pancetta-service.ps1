# Remove only the service registration. Preserve config, contacts and logs.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run PowerShell as administrator.'
}
$serviceName = 'Pancetta'
$service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if (-not $service) {
    Write-Host 'Service "Pancetta" is not installed. All data is left intact.'
    exit 0
}
$nssm = Join-Path $PSScriptRoot 'nssm.exe'
if (-not (Test-Path -LiteralPath $nssm -PathType Leaf)) {
    $command = Get-Command nssm.exe -CommandType Application -ErrorAction SilentlyContinue
    if ($command) { $nssm = $command.Source } else { $nssm = $null }
}
Write-Host 'Stopping service "Pancetta" (console stop, up to 20 s for PTT/rigctld drain)...'
if ($service.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Stopped) {
    if ($nssm) { & $nssm stop $serviceName } else { & sc.exe stop $serviceName }
    if ($LASTEXITCODE -ne 0) { throw 'Service stop failed. Registration and data left intact.' }
    $service.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(60))
}
if ($nssm) { & $nssm remove $serviceName confirm } else { & sc.exe delete $serviceName }
if ($LASTEXITCODE -ne 0) { throw 'Service removal failed. Data left intact.' }
Write-Host 'Service "Pancetta" stopped and removed.'
Write-Host 'Left intact: the service account home\.pancetta\ (config, QSO data, app logs)'
Write-Host 'Left intact: the service account LOCALAPPDATA\pancetta\logs\ (supervisor logs)'
