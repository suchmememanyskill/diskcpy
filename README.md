# diskcpy

Copy between files and raw Windows disks.

## Install

Run this in PowerShell:

```powershell
$InstallDir = "$env:LOCALAPPDATA\Microsoft\WindowsApps"; Invoke-WebRequest "https://github.com/suchmememanyskill/diskcpy/releases/latest/download/diskcpy.exe" -OutFile "$InstallDir\diskcpy.exe"
```

This downloads the latest release to `%LOCALAPPDATA%\Microsoft\WindowsApps`,
which is normally included in the user `PATH` on Windows.

## Uninstall

```powershell
Remove-Item "$env:LOCALAPPDATA\Microsoft\WindowsApps\diskcpy.exe"
```

## Usage

```powershell
diskcpy [OPTIONS] <SOURCE> <DESTINATION>
```

Help:

```powershell
diskcpy -h
```

Get local version:

```powershell
diskcpy --version
```

Examples:

```powershell
diskcpy \\.\PhysicalDrive2 backup-emmc.img
diskcpy \\.\PhysicalDrive2 backup.img --count 8gb
diskcpy backup-emmc.img \\.\PhysicalDrive2
```

To identify physical drives:

```powershell
Get-CimInstance Win32_DiskDrive | Select-Object DeviceID, Model, Size
```
