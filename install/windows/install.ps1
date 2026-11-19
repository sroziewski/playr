<#
.SYNOPSIS
    Installs playr for the current user and adds it to "Open with" for media files.

.DESCRIPTION
    No administrator rights are needed: everything goes under the user's profile and HKCU.

    * playr.exe is copied to %LOCALAPPDATA%\Programs\playr (built first with cargo unless
      -Binary names a ready one), together with this script, which is the uninstaller.
    * A Start menu shortcut is added.
    * playr is registered for the media file types, so right-click > Open with lists it, and
      it appears in Settings > Apps > Default apps. Windows 11 does not let a program make itself
      the default for a type; pick playr there (or tick "Always" in Open with) to make it so.
    * An entry in Settings > Apps > Installed apps uninstalls it.

    Running it again updates the installation in place.

.PARAMETER Binary
    A built playr.exe to install. Without it the script runs `cargo build --release` in the
    repository this script belongs to and installs target\release\playr.exe.

.PARAMETER Uninstall
    Removes everything an install added.

.PARAMETER InstallDir
    Where playr.exe goes. Default: %LOCALAPPDATA%\Programs\playr.

.PARAMETER StartMenuDir
    Where the shortcut goes. Default: the user's Start menu Programs folder.

.PARAMETER RegistryRoot
    The registry key that stands for HKCU:\Software (the default). Tests point it at a scratch
    key so that they do not change the real file associations.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File install\windows\install.ps1

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File install\windows\install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [string]$Binary,
    [switch]$Uninstall,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\playr'),
    [string]$StartMenuDir = [Environment]::GetFolderPath('Programs'),
    [string]$RegistryRoot = 'HKCU:\Software'
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

# Must equal MEDIA_EXTENSIONS in src/ui.rs (tests/installers.rs checks it).
$Extensions = @('mp4', 'm4v', 'mov', 'mkv', 'webm', 'avi', 'mp3', 'flac', 'aac', 'm4a', 'ogg', 'oga', 'wav', 'm3u', 'm3u8', 'pls')

$ProgId = 'playr.media'
$AppName = 'playr'
$Description = 'Light, fast media player: video files, audio files and internet radio'

# Paths are kept without a trailing backslash so that keys and folders compare equal however
# they were typed.
function Get-TrimmedPath([string]$Path) {
    $trimmed = $Path.TrimEnd('\')
    # "C:\" stays itself: "C:" would mean the current folder on drive C.
    if ($trimmed -match '^[A-Za-z]:$') { return "$trimmed\" }
    return $trimmed
}
# Quotes a path for a Windows command line. Backslashes before a closing quote escape it, so a
# trailing one (a drive root such as C:\) is doubled; folder names cannot contain quotes.
function Format-Argument([string]$Path) {
    if ($Path.EndsWith('\')) { $Path += '\' }
    return "`"$Path`""
}

$InstallDir = Get-TrimmedPath $InstallDir
$StartMenuDir = Get-TrimmedPath $StartMenuDir
$Root = Get-TrimmedPath $RegistryRoot
$Classes = "$Root\Classes"
$CapabilitiesKey = "$Root\playr\Capabilities"
$ExePath = Join-Path $InstallDir 'playr.exe'
$Shortcut = Join-Path $StartMenuDir 'playr.lnk'
$UninstallKey = "$Root\Microsoft\Windows\CurrentVersion\Uninstall\playr"
$AppPathKey = "$Root\Microsoft\Windows\CurrentVersion\App Paths\playr.exe"

# RegisteredApplications holds a path relative to the hive (HKCU), without the "HKCU:\" drive.
$CapabilitiesRelative = $CapabilitiesKey -replace '^[A-Za-z]+:\\', ''

function Set-KeyValue([string]$Key, [string]$Name, $Value, [string]$Type = 'String') {
    if (-not (Test-Path -LiteralPath $Key)) { New-Item -Path $Key -Force | Out-Null }
    if ($Name -eq '') {
        # The key's (Default) value.
        Set-Item -LiteralPath $Key -Value $Value
    } else {
        New-ItemProperty -LiteralPath $Key -Name $Name -Value $Value -PropertyType $Type -Force | Out-Null
    }
}

function Remove-KeyTree([string]$Key) {
    if (Test-Path -LiteralPath $Key) { Remove-Item -LiteralPath $Key -Recurse -Force }
}

function Remove-KeyValue([string]$Key, [string]$Name) {
    if ((Test-Path -LiteralPath $Key) -and ($null -ne (Get-Item -LiteralPath $Key).GetValue($Name, $null))) {
        Remove-ItemProperty -LiteralPath $Key -Name $Name -Force
    }
}

# Removes the key if nothing is left in it, then its parent if that is empty too, up to Stop.
# File-type keys such as .mp4 are shared with every other program, so only empty ones go.
function Remove-EmptyKeys([string]$Key, [string]$Stop) {
    while ($Key -and ($Key.TrimEnd('\') -ne $Stop.TrimEnd('\')) -and (Test-Path -LiteralPath $Key)) {
        $item = Get-Item -LiteralPath $Key
        if ($item.ValueCount -gt 0 -or $item.SubKeyCount -gt 0) { return }
        Remove-Item -LiteralPath $Key -Force
        $Key = Split-Path -Path $Key -Parent
    }
}

function Send-AssociationsChanged {
    # SHChangeNotify(SHCNE_ASSOCCHANGED) makes Explorer re-read the associations now instead of
    # after the next sign-in.
    if (-not ('Playr.Shell' -as [type])) {
        Add-Type -Namespace Playr -Name Shell -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("shell32.dll")]
public static extern void SHChangeNotify(int eventId, uint flags, System.IntPtr item1, System.IntPtr item2);
'@
    }
    [Playr.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)
}

function Test-Running([string]$Path) {
    $full = [IO.Path]::GetFullPath($Path)
    @(Get-Process -Name playr -ErrorAction SilentlyContinue | Where-Object {
        try { $_.Path -and ([IO.Path]::GetFullPath($_.Path) -eq $full) } catch { $false }
    }).Count -gt 0
}

function Resolve-Binary {
    if ($Binary) {
        if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) { throw "No such file: $Binary" }
        return (Resolve-Path -LiteralPath $Binary).Path
    }
    $repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
    if (-not (Test-Path (Join-Path $repo 'Cargo.toml'))) {
        throw "No playr source next to this script ($repo). Pass -Binary <path to playr.exe>."
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo was not found. Install Rust from https://rustup.rs, or pass -Binary <path to playr.exe>.'
    }
    Write-Host 'Building playr (cargo build --release)...'
    Push-Location $repo
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
    return (Join-Path $repo 'target\release\playr.exe')
}

function Install-Playr {
    $source = Resolve-Binary
    if (Test-Running $ExePath) { throw "playr is running from $ExePath; close it and run the installer again." }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    if ([IO.Path]::GetFullPath($source) -ne [IO.Path]::GetFullPath($ExePath)) {
        Copy-Item -LiteralPath $source -Destination $ExePath -Force
    }
    $uninstaller = Join-Path $InstallDir 'uninstall.ps1'
    if ([IO.Path]::GetFullPath($PSCommandPath) -ne [IO.Path]::GetFullPath($uninstaller)) {
        Copy-Item -LiteralPath $PSCommandPath -Destination $uninstaller -Force
    }

    $icon = "`"$ExePath`",0"
    $open = "`"$ExePath`" `"%1`""

    # The file type ("ProgID") that the extensions point at.
    Set-KeyValue "$Classes\$ProgId" '' 'Media file'
    Set-KeyValue "$Classes\$ProgId" 'FriendlyTypeName' 'Media file (playr)'
    Set-KeyValue "$Classes\$ProgId\DefaultIcon" '' $icon
    Set-KeyValue "$Classes\$ProgId\shell" '' 'open'
    Set-KeyValue "$Classes\$ProgId\shell\open" 'FriendlyAppName' $AppName
    Set-KeyValue "$Classes\$ProgId\shell\open\command" '' $open

    # The application itself, for Open with > Choose another app.
    $app = "$Classes\Applications\playr.exe"
    Set-KeyValue $app 'FriendlyAppName' $AppName
    Set-KeyValue "$app\DefaultIcon" '' $icon
    Set-KeyValue "$app\shell\open\command" '' $open

    # Default apps in Settings lists programs that declare capabilities.
    Set-KeyValue $CapabilitiesKey 'ApplicationName' $AppName
    Set-KeyValue $CapabilitiesKey 'ApplicationDescription' $Description
    Set-KeyValue $CapabilitiesKey 'ApplicationIcon' $icon
    Set-KeyValue "$Root\RegisteredApplications" $AppName $CapabilitiesRelative

    foreach ($ext in $Extensions) {
        Set-KeyValue "$app\SupportedTypes" ".$ext" ''
        Set-KeyValue "$CapabilitiesKey\FileAssociations" ".$ext" $ProgId
        # This adds playr to the extension's Open with list; the default program is untouched.
        Set-KeyValue "$Classes\.$ext\OpenWithProgids" $ProgId '' 'String'
    }

    # `playr` from Win+R.
    Set-KeyValue $AppPathKey '' $ExePath

    $version = (Get-Item -LiteralPath $ExePath).VersionInfo.ProductVersion
    Set-KeyValue $UninstallKey 'DisplayName' $AppName
    Set-KeyValue $UninstallKey 'DisplayIcon' $icon
    Set-KeyValue $UninstallKey 'Publisher' 'playr'
    if ($version) { Set-KeyValue $UninstallKey 'DisplayVersion' $version }
    Set-KeyValue $UninstallKey 'InstallLocation' $InstallDir
    # The locations are passed on so that an install to non-default places uninstalls from them.
    $uninstallCommand = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File $(Format-Argument $uninstaller) -Uninstall" +
        " -InstallDir $(Format-Argument $InstallDir) -StartMenuDir $(Format-Argument $StartMenuDir)" +
        " -RegistryRoot $(Format-Argument $Root)"
    Set-KeyValue $UninstallKey 'UninstallString' $uninstallCommand
    Set-KeyValue $UninstallKey 'NoModify' 1 'DWord'
    Set-KeyValue $UninstallKey 'NoRepair' 1 'DWord'

    New-Item -ItemType Directory -Path $StartMenuDir -Force | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($Shortcut)
    $link.TargetPath = $ExePath
    $link.WorkingDirectory = $InstallDir
    $link.IconLocation = "$ExePath,0"
    $link.Description = $Description
    $link.Save()

    Send-AssociationsChanged
    Write-Host "playr is installed in $InstallDir."
    Write-Host 'Right-click a media file > Open with > playr. To make playr the default player,'
    Write-Host 'choose it in Settings > Apps > Default apps > playr.'
}

function Uninstall-Playr {
    if (Test-Running $ExePath) { throw "playr is running from $ExePath; close it and run the uninstaller again." }

    foreach ($ext in $Extensions) {
        Remove-KeyValue "$Classes\.$ext\OpenWithProgids" $ProgId
        Remove-EmptyKeys "$Classes\.$ext\OpenWithProgids" $Classes
    }
    Remove-KeyTree "$Classes\$ProgId"
    Remove-KeyTree "$Classes\Applications\playr.exe"
    Remove-KeyValue "$Root\RegisteredApplications" $AppName
    Remove-KeyTree "$Root\playr"
    Remove-KeyTree $AppPathKey
    Remove-KeyTree $UninstallKey

    if (Test-Path -LiteralPath $Shortcut) { Remove-Item -LiteralPath $Shortcut -Force }
    # Only the two files the install put there go: -InstallDir may name a folder shared with
    # other programs (or a drive root), so the folder is removed only when that leaves it empty.
    foreach ($file in @($ExePath, (Join-Path $InstallDir 'uninstall.ps1'))) {
        if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force }
    }
    $isDriveRoot = [IO.Path]::GetPathRoot($InstallDir) -eq $InstallDir
    if (-not $isDriveRoot -and (Test-Path -LiteralPath $InstallDir) -and -not (Get-ChildItem -LiteralPath $InstallDir -Force)) {
        # The uninstaller may be running from the folder it deletes; step out of it first.
        Set-Location -LiteralPath ([IO.Path]::GetTempPath())
        Remove-Item -LiteralPath $InstallDir -Force
    }

    Send-AssociationsChanged
    Write-Host 'playr is uninstalled.'
}

if ($Uninstall) { Uninstall-Playr } else { Install-Playr }
