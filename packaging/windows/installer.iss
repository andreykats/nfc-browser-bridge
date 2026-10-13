; Topps NFC Bridge — per-user installer (no admin rights).
; Build: iscc /DAppVersion=0.1.0 /DExePath=..\..\target\x86_64-pc-windows-msvc\release\topps-nfc-bridge.exe installer.iss

#ifndef AppVersion
  #error Pass /DAppVersion=x.y.z
#endif
#ifndef ExePath
  #error Pass /DExePath=<path to topps-nfc-bridge.exe>
#endif

#define AppName "Topps NFC Bridge"
#define ExeName "topps-nfc-bridge.exe"

[Setup]
AppId={{6F1C2B7A-3D4E-4F5A-9B8C-7D6E5F4A3B2C}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=drey.dev
AppPublisherURL=https://github.com/andreykats/nfc-browser-bridge
AppSupportURL=https://github.com/andreykats/nfc-browser-bridge/issues
AppUpdatesURL=https://github.com/andreykats/nfc-browser-bridge/releases
; Per-user install: {autopf} resolves to the user's Programs folder.
PrivilegesRequired=lowest
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; The running bridge is also stopped in PrepareToInstall; this covers anything else holding the exe.
CloseApplications=yes
RestartApplications=no
SetupIconFile=app.ico
UninstallDisplayIcon={app}\{#ExeName}
UninstallDisplayName={#AppName}
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
OutputDir=.
OutputBaseFilename=Topps-NFC-Bridge-{#AppVersion}-windows-setup

[Tasks]
Name: "startup"; Description: "Start {#AppName} when I sign in"

[Files]
Source: "{#ExePath}"; DestDir: "{app}"; DestName: "{#ExeName}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#ExeName}"

[Registry]
; Same value the app's "Start at login" menu item writes.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; \
  ValueName: "ToppsNFCBridge"; ValueData: """{app}\{#ExeName}"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\{#ExeName}"; Description: "Start {#AppName} now"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#ExeName}"; Flags: runhidden; RunOnceId: "StopBridge"

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\ToppsNFCBridge"

[Code]
procedure StopBridge();
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#ExeName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopBridge();
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  // Unchecking the task on an upgrade removes an earlier login item.
  if (CurStep = ssPostInstall) and not WizardIsTaskSelected('startup') then
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'ToppsNFCBridge');
end;
