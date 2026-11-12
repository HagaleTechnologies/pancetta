# Windows service

The Windows release zip includes these scripts to run `pancetta.exe --headless`
as the auto-starting Windows service `Pancetta`. They require Windows PowerShell
5.1 and [NSSM 2.24-101-g897c7ad or newer](https://nssm.cc/download).
NSSM is not bundled or downloaded by the scripts. Put the appropriate 64-bit `nssm.exe` beside these scripts in
`windows\`, or on PATH. Keep it there while the service is installed: Windows
runs NSSM as the service wrapper. The official download page requires build
2.24-101 or newer on modern Windows to avoid service-start failures. Keep the
console enabled so NSSM can send Ctrl-C for graceful shutdown.

Extract the zip to a stable folder, for example `C:\Tools\pancetta-windows`.
Configure and test pancetta interactively with your station account first.
Close that instance before installing the service.

## Install

Open PowerShell **as administrator using the same station account**. Elevating
with another administrator's credentials installs for that other account instead.
The account needs a password and permission to log on as a service. Use the
account password, not a Windows Hello PIN. Domain policy may override the logon
right NSSM grants. The password is passed to NSSM to register the service logon;
Windows stores those credentials. Do not capture the install command's process
arguments in diagnostic logs.

Windows clients may block unsigned scripts by default. After reviewing these
scripts, allow them in this PowerShell process only. This also allows scripts
marked as downloaded; machine and domain policies still take precedence:

```powershell
Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
```

From the extracted folder:

```powershell
.\windows\install-pancetta-service.ps1
```

The script prompts for your password, registers and starts `Pancetta`.
It refuses an existing registration. To select another executable, pass
`-BinaryPath 'C:\Station tools\pancetta.exe'`.

The service runs under this account at boot, without an interactive login.
It restarts two seconds after any exit. Supervisor stdout and stderr go to
`%LOCALAPPDATA%\pancetta\logs\service.out.log` and `service.err.log`,
with NSSM rotation at 1 MiB on restart. The application's daily logs remain
under `%USERPROFILE%\.pancetta\logs`; config and QSO data remain under
`%USERPROFILE%\.pancetta\`. Log paths are expanded at installation time.

## Operate and upgrade

Use an elevated PowerShell window:

```powershell
sc.exe query Pancetta
sc.exe stop Pancetta
sc.exe start Pancetta
# Or, with NSSM on PATH:
nssm restart Pancetta
```

Stop during a safe idle interval and wait for `sc.exe query Pancetta` to show
`STOPPED`. NSSM sends console Ctrl-C first and allows 20 seconds for pancetta's
PTT/rigctld shutdown drain before its later stop methods.

To upgrade, stop the service, replace `pancetta.exe` in the stable folder,
then start it. Keep NSSM and the folder paths intact. Service registration
continues to point at those paths. If installation fails partway through,
check `sc.exe query Pancetta`, correct the reported problem, and uninstall
the partial registration before retrying. A failed install disables the service
to prevent a partially configured registration from starting at boot.

## Uninstall

From the extracted folder in elevated PowerShell, with the process execution
policy set as above:

```powershell
.\windows\uninstall-pancetta-service.ps1
```

This stops the service and removes its registration. If NSSM is unavailable,
the script uses `sc.exe`. Running it again succeeds with a "not installed"
message. It deletes no files: config, contacts, application logs and supervisor
logs remain in the service account's directories.

## First station check

Linux CI checks packaging wiring; the Windows release job checks PowerShell
syntax and zip contents. Neither proves service behavior on a station.
On Windows, verify startup and logs, reboot and confirm automatic startup,
and check access to the configured audio devices and rigctld without a user
login. During a supervised stop, confirm PTT releases and the rigctld drain
finishes. Test uninstall and confirm your config, QSO data and logs remain.
Windows service sessions can have different audio access from interactive
sessions; qualify the station before unattended operation.
