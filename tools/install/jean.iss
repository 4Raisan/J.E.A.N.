[Setup]
AppId={{3B9C1A7E-6A2B-4C0E-9A2B-1A2B3C4D5E6F}
AppName=J.E.A.N.
AppVersion=0.1.0
DefaultDirName={autopf}\JEAN
PrivilegesRequired=admin
; Copies jean.exe and jean-tray.exe. Runtime elevation is unsupported.
; Toast AUMID registration is per-user and remains opt-in via `jean notify register`.
; Creates Start Menu shortcut (optional, DisplayName only — no pipe SDDL change)
[Files]
Source: "..\..\target\release\jean.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\jean-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
; Voice worker is feature-gated (default build has no native dep):
; Source: "..\..\target\release\jean-voice.exe"; DestDir: "{app}"; Flags: ignoreversion
[Icons]
Name: "{autoprograms}\J.E.A.N. Tray"; Filename: "{app}\jean-tray.exe"
