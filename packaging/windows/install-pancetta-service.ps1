# Run in Windows PowerShell 5.1 as administrator, using your station account.
[CmdletBinding()]
param(
    [string]$BinaryPath = (Join-Path $PSScriptRoot '..\pancetta.exe')
)
$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run PowerShell as administrator using your station account.'
}
$serviceName = 'Pancetta'
if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
    throw 'Service Pancetta already exists. Stop it and replace the binary to upgrade, or uninstall first.'
}
$nssm = Join-Path $PSScriptRoot 'nssm.exe'
if (-not (Test-Path -LiteralPath $nssm -PathType Leaf)) {
    $command = Get-Command nssm.exe -CommandType Application -ErrorAction SilentlyContinue
    if (-not $command) {
        throw 'Get NSSM 2.24-101-g897c7ad or newer from https://nssm.cc/download and put nssm.exe beside this script or on PATH.'
    }
    $nssm = $command.Source
}
$binary = (Resolve-Path -LiteralPath $BinaryPath).ProviderPath
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw 'BinaryPath must name pancetta.exe.'
}
$installDirectory = Split-Path -Parent $binary
$logDirectory = Join-Path $env:LOCALAPPDATA 'pancetta\logs'
$outLog = Join-Path $logDirectory 'service.out.log'
$errLog = Join-Path $logDirectory 'service.err.log'
$account = $identity.Name
Write-Host "Service account : $account"
$password = Read-Host 'Service password (account password, not Windows Hello PIN)' -AsSecureString
function Invoke-Nssm {
    param([string[]]$NssmArguments)
    # Windows PowerShell 5.1's native invocation drops embedded quotes.
    # Quote every argument using the Windows C runtime rules, including passwords.
    $quoted = foreach ($argument in $NssmArguments) {
        $escaped = [regex]::Replace($argument, '(\\*)"', '$1$1\"')
        $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
        '"' + $escaped + '"'
    }
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $nssm
    $start.Arguments = $quoted -join ' '
    $start.UseShellExecute = $false
    $process = [System.Diagnostics.Process]::Start($start)
    try {
        $process.WaitForExit()
        if ($process.ExitCode -ne 0) {
            throw "NSSM command failed (exit $($process.ExitCode)). Check service Pancetta before retrying."
        }
    } finally {
        $process.Dispose()
        $start.Arguments = ''
    }
}
$secretPointer = [IntPtr]::Zero
$plainPassword = $null
$installed = $false
try {
    New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
    Invoke-Nssm @('install', $serviceName, $binary, '--headless')
    $installed = $true
    Invoke-Nssm @('set', $serviceName, 'Start', 'SERVICE_DEMAND_START')
    Invoke-Nssm @('set', $serviceName, 'AppDirectory', $installDirectory)
    Invoke-Nssm @('set', $serviceName, 'AppStdout', $outLog)
    Invoke-Nssm @('set', $serviceName, 'AppStderr', $errLog)
    Invoke-Nssm @('set', $serviceName, 'AppRotateFiles', '1')
    Invoke-Nssm @('set', $serviceName, 'AppRotateBytes', '1048576')
    Invoke-Nssm @('set', $serviceName, 'AppExit', 'Default', 'Restart')
    Invoke-Nssm @('set', $serviceName, 'AppRestartDelay', '2000')
    Invoke-Nssm @('set', $serviceName, 'AppThrottle', '1500')
    Invoke-Nssm @('set', $serviceName, 'AppStopMethodConsole', '20000')
    $secretPointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($password)
    $plainPassword = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($secretPointer)
    Invoke-Nssm @('set', $serviceName, 'ObjectName', $account, $plainPassword)
    Invoke-Nssm @('set', $serviceName, 'Start', 'SERVICE_AUTO_START')
    Invoke-Nssm @('start', $serviceName)
    $service = Get-Service -Name $serviceName
    $service.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(30))
} catch {
    # Never leave a partially configured service to start as LocalSystem at boot.
    if ($installed) {
        Set-Service -Name $serviceName -StartupType Disabled -ErrorAction Continue
    }
    throw
} finally {
    if ($secretPointer -ne [IntPtr]::Zero) {
        [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($secretPointer)
    }
    $plainPassword = $null
    $password.Dispose()
}
Write-Host 'Service "Pancetta" started. Starts automatically at boot; restarts 2s after any exit.'
Write-Host "Supervisor logs : $outLog"
Write-Host "                  $errLog"
Write-Host "Config / data   : $env:USERPROFILE\.pancetta\ (uninstall never touches this)"
