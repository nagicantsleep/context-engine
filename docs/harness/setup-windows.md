# Windows Setup

## Prerequisites

### Install Rust

Download and run [rustup-init.exe](https://rustup.rs/):

```bash
# Verify installation
rustc --version
cargo --version
```

### Install Git Bash

Download [Git for Windows](https://git-scm.com/download/win). During installation, select "Git Bash Here" context menu option.

### Visual Studio Build Tools

Rust on Windows requires MSVC linker. Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022) with "Desktop development with C++" workload.

Alternatively, if you have Visual Studio installed, ensure C++ build tools are included.

## Windows Defender Exclusions

RocksDB uses memory-mapped files and creates thousands of small SST files. Windows Defender real-time scanning causes severe lock contention (measured 7+ second delays in similar projects).

Add exclusions for RocksDB directories:

```powershell
# Run PowerShell as Administrator
Add-MpPreference -ExclusionPath "$env:USERPROFILE\.context-engine\data"
Add-MpPreference -ExclusionPath "E:\Workspaces\context-engine\target"

# Verify exclusions
Get-MpPreference | Select-Object -ExpandProperty ExclusionPath
```

If using a different workspace location, add that path as well.

## Enable Long Path Support

Windows has a 260-character path limit by default. Deep node_modules or nested source trees exceed this.

Enable long paths:

```powershell
# Run PowerShell as Administrator
New-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem" `
  -Name "LongPathsEnabled" -Value 1 -PropertyType DWORD -Force

# Restart required
```

Alternatively, enable via Group Policy:
1. Run `gpedit.msc`
2. Navigate to: Computer Configuration > Administrative Templates > System > Filesystem
3. Enable "Enable Win32 long paths"
4. Restart

## Configure Git Line Endings

Prevent line-ending issues when working with cross-platform repos:

```bash
git config --global core.autocrlf false
git config --global core.eol lf
```

## Build Context Engine

```bash
cd E:\Workspaces\context-engine
cargo build --release
```

First build downloads dependencies and compiles tree-sitter parsers (20+ languages). Expect 5-10 minutes.

## Add to PATH

Add cargo binaries to PATH for convenient invocation:

```powershell
# Add to user PATH (does not require Administrator)
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$cargoPath = "$env:USERPROFILE\.cargo\bin"
if ($userPath -notlike "*$cargoPath*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$cargoPath", "User")
}

# Restart terminal to pick up new PATH
```

## Verify Installation

```bash
# Run from any directory
context-engine-rs --version

# Index a test repository
context-engine-rs index E:\Workspaces\context-engine

# Check database was created
ls ~/.context-engine/data/
```

Expected output: one directory per indexed repo, named by content hash.

## Common Issues

### "linker 'link.exe' not found"

Install Visual Studio Build Tools with C++ workload. Verify with:

```bash
where link.exe
```

Should show path under `Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\...`.

### "access is denied" during build

Windows Defender or antivirus is scanning target directory. Add exclusion (see above).

### Database lock timeout during indexing

Windows Defender exclusion missing, or another process (backup software, cloud sync) is scanning the database directory. Check Task Manager for high disk I/O from MsMpEng.exe or similar.

### Path too long errors

Enable long path support (see above) and restart.

## Next Steps

- [Usage guide](usage.md) for indexing and querying
- [Troubleshooting](troubleshooting.md) for detailed debugging
