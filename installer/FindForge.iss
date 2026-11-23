; FindForge installer (Inno Setup 6).

#ifndef AppVersion
  #define AppVersion "1.1.0"
#endif
; package.ps1 passes these, so a build outside .\target (CARGO_TARGET_DIR) works too.
#ifndef AppExe
  #define AppExe "..\target\release\FindForge.exe"
#endif
#ifndef AppIcon
  #define AppIcon "..\dist\findforge.ico"
#endif

[Setup]
AppId={{7C4E2A91-5B3D-4F68-A1C7-9E0D3B6F2A85}
AppName=FindForge
AppVersion={#AppVersion}
AppVerName=FindForge {#AppVersion}
AppPublisher=Zheer Khalid
AppPublisherURL=https://zheer.dev
AppSupportURL=https://github.com/zheerkhalid/findforge
AppUpdatesURL=https://github.com/zheerkhalid/findforge/releases
DefaultDirName={autopf}\FindForge
DefaultGroupName=FindForge
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible 
ArchitecturesInstallIn64BitMode=x64compatible 
OutputDir=..\dist
OutputBaseFilename=FindForge-Setup
SetupIconFile={#AppIcon}
UninstallDisplayIcon={app}\FindForge.exe
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
LicenseFile=..\LICENSE
VersionInfoVersion={#AppVersion}
VersionInfoCompany=Zheer Khalid (zheer.dev)
VersionInfoDescription=FindForge Setup

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; 

[Files]
Source: "{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\FindForge"; Filename: "{app}\FindForge.exe"
Name: "{group}\Uninstall FindForge"; Filename: "{uninstallexe}"
Name: "{autodesktop}\FindForge"; Filename: "{app}\FindForge.exe"; Tasks: desktopicon


[Run]
Filename: "{app}\FindForge.exe"; Description: "{cm:LaunchProgram,FindForge}"; Flags: nowait postinstall skipifsilent shellexec

[UninstallRun]
Filename: "{cmd}"; Parameters: "/C taskkill /IM FindForge.exe /F"; Flags: runhidden; RunOnceId: "KillFindForge"

[Registry]
; Removes the optional "start with Windows" entry on uninstall (the program itself adds it from Settings).
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "FindForge"; Flags: dontcreatekey uninsdeletevalue

[UninstallDelete]
; The saved index and settings.
Type: filesandordirs; Name: "{localappdata}\FindForge"
