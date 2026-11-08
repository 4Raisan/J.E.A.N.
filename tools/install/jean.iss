[Setup]
AppId={{3B9C1A7E-6A2B-4C0E-9A2B-1A2B3C4D5E6F}
AppName=J.E.A.N.
AppVersion=0.1.0
DefaultDirName={autopf}\JEAN
PrivilegesRequired=admin
; Copies jean.exe, jean-tray.exe, jean-elevated.exe, jean-voice.exe (if native)
; Registers AUMID HKCU\Software\Classes\AppUserModelId\JEAN.Orchestrator DisplayName
; Creates Start Menu shortcut (optional, DisplayName only — no pipe SDDL change)
[Files]
Source: "..\..\target\release\jean.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\jean-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\jean-elevated.exe"; DestDir: "{app}"; Flags: ignoreversion
; Voice worker is feature-gated (default build has no native dep):
; Source: "..\..\target\release\jean-voice.exe"; DestDir: "{app}"; Flags: ignoreversion
[Registry]
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\JEAN.Orchestrator"; ValueType: string; ValueName: "DisplayName"; ValueData: "J.E.A.N."
[Icons]
Name: "{autoprograms}\J.E.A.N. Tray"; Filename: "{app}\jean-tray.exe"
