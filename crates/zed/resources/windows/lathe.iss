; Lathe Windows installer (Inno Setup 6.x).
;
; Compiled by script/package-fork-windows.ps1 after the zip is staged.
; Mirrors the layout that script/install-fork-windows.ps1 produces, so an
; install via this .exe ends up at the same on-disk location and the same
; PATH/Start-Menu/uninstaller wiring as the manual zip + PowerShell flow.
;
; Required preprocessor inputs (passed via ISCC /D flags):
;   LatheChannel  - stable | beta | preview | nightly | dev
;   LatheVersion  - e.g. "0.236.2" (no leading v, no -beta suffix)
;   LatheArch     - x86_64 | aarch64
;   StageDir      - absolute path to the staged install tree (bin/, libexec/, lib/, share/)
;   OutputDir     - absolute path where the compiled installer .exe is written

#ifndef LatheChannel
  #define LatheChannel "dev"
#endif
#ifndef LatheVersion
  #define LatheVersion "0.0.0"
#endif
#ifndef LatheArch
  #define LatheArch "x86_64"
#endif
#ifndef StageDir
  #error StageDir is required
#endif
#ifndef OutputDir
  #error OutputDir is required
#endif

; Per-channel branding. AppId GUIDs are stable per channel so installer
; upgrades replace the existing install in place rather than creating a
; second entry in Add/Remove Programs.
#if LatheChannel == "stable"
  #define LatheDisplayName "Lathe"
  #define LatheDirName "lathe"
  #define LatheAppId "{{F7A93B40-9C12-4F8E-B1A0-E1D6C7B4A001}"
  #define LatheIcon "app-icon.ico"
#elif LatheChannel == "beta"
  #define LatheDisplayName "Lathe Beta"
  #define LatheDirName "lathe-beta"
  #define LatheAppId "{{F7A93B40-9C12-4F8E-B1A0-E1D6C7B4A002}"
  #define LatheIcon "app-icon-beta.ico"
#elif LatheChannel == "preview"
  #define LatheDisplayName "Lathe Preview"
  #define LatheDirName "lathe-preview"
  #define LatheAppId "{{F7A93B40-9C12-4F8E-B1A0-E1D6C7B4A003}"
  #define LatheIcon "app-icon-preview.ico"
#elif LatheChannel == "nightly"
  #define LatheDisplayName "Lathe Nightly"
  #define LatheDirName "lathe-nightly"
  #define LatheAppId "{{F7A93B40-9C12-4F8E-B1A0-E1D6C7B4A004}"
  #define LatheIcon "app-icon-nightly.ico"
#else
  #define LatheDisplayName "Lathe Dev"
  #define LatheDirName "lathe-dev"
  #define LatheAppId "{{F7A93B40-9C12-4F8E-B1A0-E1D6C7B4A005}"
  #define LatheIcon "app-icon-dev.ico"
#endif

#if LatheArch == "aarch64"
  #define LatheArchAllowed "arm64"
#else
  #define LatheArchAllowed "x64compatible"
#endif

[Setup]
AppId={#LatheAppId}
AppName={#LatheDisplayName}
AppVersion={#LatheVersion}
AppVerName={#LatheDisplayName} {#LatheVersion}
AppPublisher=Chris Paterson
AppPublisherURL=https://github.com/paterschris/lathe
AppSupportURL=https://github.com/paterschris/lathe/issues
AppUpdatesURL=https://github.com/paterschris/lathe/releases
DefaultDirName={localappdata}\Programs\Lathe\{#LatheDirName}
DefaultGroupName={#LatheDisplayName}
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=no
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed={#LatheArchAllowed}
ArchitecturesInstallIn64BitMode={#LatheArchAllowed}
OutputDir={#OutputDir}
OutputBaseFilename=Lathe-{#LatheVersion}-{#LatheArch}-windows-setup
; share\lathe.ico in the staged tree was already chosen for the channel by
; the packaging script, so we point straight at it instead of duplicating
; the channel->icon mapping here.
SetupIconFile={#StageDir}\share\lathe.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
CloseApplications=force
RestartApplications=no
UninstallDisplayIcon={app}\share\lathe.ico
UninstallDisplayName={#LatheDisplayName}
WizardImageStretch=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked
Name: "addtopath"; Description: "Add the &lathe CLI to your PATH"; GroupDescription: "Additional tasks:"

[Files]
; The staged tree mirrors script/install-fork-windows.ps1's layout:
;   bin\lathe.exe                 (cli)
;   libexec\lathe-editor.exe      (editor)
;   lib\conpty.dll, OpenConsole.exe, optionally amd_ags_x64.dll
;   share\lathe.ico
Source: "{#StageDir}\bin\*";     DestDir: "{app}\bin";     Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#StageDir}\libexec\*"; DestDir: "{app}\libexec"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#StageDir}\lib\*";     DestDir: "{app}\lib";     Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#StageDir}\share\*";   DestDir: "{app}\share";   Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#LatheDisplayName}"; Filename: "{app}\libexec\lathe-editor.exe"; IconFilename: "{app}\share\lathe.ico"; WorkingDir: "{app}"
Name: "{userdesktop}\{#LatheDisplayName}"; Filename: "{app}\libexec\lathe-editor.exe"; IconFilename: "{app}\share\lathe.ico"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\libexec\lathe-editor.exe"; Description: "Launch {#LatheDisplayName}"; Flags: nowait postinstall skipifsilent

[Registry]
; Idempotent PATH update -- only added when not already present, scoped to the user's environment.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{olddata};{app}\bin"; \
    Check: NeedsAddPath(ExpandConstant('{app}\bin')); \
    Tasks: addtopath

[Code]
function NeedsAddPath(NewDir: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  // Pad both sides with ';' so we match whole entries, not substrings.
  Result := Pos(';' + Lowercase(NewDir) + ';', ';' + Lowercase(OrigPath) + ';') = 0;
end;

procedure RemoveFromPath(StripDir: string);
var
  OrigPath: string;
  Padded: string;
  P: integer;
  Cleaned: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
    exit;
  Padded := ';' + OrigPath + ';';
  P := Pos(';' + Lowercase(StripDir) + ';', Lowercase(Padded));
  if P = 0 then exit;
  // Splice the matched segment (length: StripDir + 1 trailing ';') out of Padded.
  Delete(Padded, P, Length(StripDir) + 1);
  // Trim our padding semicolons.
  if (Length(Padded) > 0) and (Padded[1] = ';') then
    Delete(Padded, 1, 1);
  if (Length(Padded) > 0) and (Padded[Length(Padded)] = ';') then
    Delete(Padded, Length(Padded), 1);
  Cleaned := Padded;
  RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Cleaned);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}\bin'));
end;
