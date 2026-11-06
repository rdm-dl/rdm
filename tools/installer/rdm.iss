; The Windows setup program. tools/installer.py builds it; it passes AppVersion, SourceDir,
; OutputDir and OutputName, and Sign when a signing command is set.

#define AppExe "rdm-gui.exe"

[Setup]
; This id ties every version together: never change it, or upgrades become second copies.
AppId={{7E3C1E6A-4C2B-4F7D-9A55-2B9D1F0C6A31}
AppName=RDM
AppVersion={#AppVersion}
AppVerName=RDM {#AppVersion}
AppPublisher=RDM
AppPublisherURL=https://rdm-dl.github.io
AppSupportURL=https://github.com/rdm-dl/rdm/issues
AppUpdatesURL=https://github.com/rdm-dl/rdm/releases
AppComments=A fast download manager
AppCopyright=MIT License
VersionInfoVersion={#AppVersion}
VersionInfoCompany=RDM
VersionInfoDescription=RDM Setup
VersionInfoProductName=RDM
VersionInfoCopyright=MIT License

; For this user by default, like Chrome or VS Code: no admin prompt, and updates just work.
; {autopf} is then %LOCALAPPDATA%\Programs, or Program Files for all users.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
DefaultDirName={autopf}\RDM
; Any folder the person can write to. An update stays in the folder chosen the first time.
DisableDirPage=auto
DefaultGroupName=RDM
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

WizardStyle=modern dynamic
WizardImageFile=wizard-164.png,wizard-205.png,wizard-246.png,wizard-328.png,wizard-410.png
WizardSmallImageFile=small-55.png,small-69.png,small-83.png,small-110.png,small-138.png
WizardImageFileDynamicDark=wizard-164.png,wizard-205.png,wizard-246.png,wizard-328.png,wizard-410.png
WizardSmallImageFileDynamicDark=small-55.png,small-69.png,small-83.png,small-110.png,small-138.png
DisableWelcomePage=no
SetupIconFile=..\..\assets\rdm.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName=RDM
LicenseFile=..\..\LICENSE
ShowLanguageDialog=no

Compression=lzma2/ultra64
SolidCompression=yes
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
SetupMutex=RDMSetup
; RDM stays in the tray when its window closes. Setup asks it to quit first; this is for the rest.
CloseApplications=force
RestartApplications=no

#ifdef Sign
SignTool=rdmsign
SignedUninstaller=yes
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
WelcomeLabel1=Welcome to RDM
WelcomeLabel2=This will install RDM {#AppVersion} on your computer.%n%nRDM splits every file across many connections, saves videos and streams from your browser, and keeps your downloads organised.%n%nYour settings and download list stay safe if you already use RDM.
FinishedHeadingLabel=RDM is ready
FinishedLabelNoIcons=RDM has been installed on your computer.
FinishedLabel=RDM has been installed on your computer. You can start it from the Start menu.

[CustomMessages]
ConnectBrowsers=Connecting RDM to your browsers...
NoWriteAccess=You don't have permission to install in this folder.%n%nChoose another one, or start Setup again and pick "Install for all users".
RemoveData=Also remove your RDM settings and download list?%n%nFiles you downloaded are not touched either way.

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\RDM"; Filename: "{app}\{#AppExe}"; Comment: "RDM download manager"
Name: "{autodesktop}\RDM"; Filename: "{app}\{#AppExe}"; Comment: "RDM download manager"; Tasks: desktopicon

[Run]
; The browser link and the sign-in entry have to name this folder, whatever was used before.
Filename: "{app}\{#AppExe}"; Parameters: "--after-install"; StatusMsg: "{cm:ConnectBrowsers}"; Flags: runhidden waituntilterminated runasoriginaluser
Filename: "{app}\{#AppExe}"; Description: "Start RDM"; Flags: nowait postinstall skipifsilent runasoriginaluser
; RDM updating itself: it closed to let this run, so it's started again afterwards.
Filename: "{app}\{#AppExe}"; Parameters: "--after-update"; Flags: nowait runasoriginaluser; Check: Relaunching

[UninstallRun]
Filename: "{app}\{#AppExe}"; Parameters: "--before-uninstall"; RunOnceId: "RemoveBrowserLink"; Flags: runhidden waituntilterminated

[Code]
function Relaunching: Boolean;
begin
  Result := ExpandConstant('{param:relaunch|0}') = '1';
end;

{ Ask a running RDM to quit: it pauses its downloads and saves its list. rdm.exe waits until it's gone. }
procedure QuitRdm;
var
  Code: Integer;
begin
  if FileExists(ExpandConstant('{app}\rdm.exe')) then
    Exec(ExpandConstant('{app}\rdm.exe'), 'quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

{ The nearest part of a path that exists, as a folder a test file can go into. }
function NearestFolder(Path: String): String;
begin
  Result := RemoveBackslashUnlessRoot(Path);
  while (not DirExists(Result)) and (ExtractFileDir(Result) <> Result) do
    Result := ExtractFileDir(Result);
end;

function CanWriteTo(Path: String): Boolean;
var
  Test: String;
begin
  Test := AddBackslash(NearestFolder(Path)) + '.rdm-setup-test';
  Result := SaveStringToFile(Test, 'x', False);
  if Result then
    DeleteFile(Test);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpSelectDir) and (not IsAdminInstallMode) and (not CanWriteTo(WizardDirValue)) then begin
    SuppressibleMsgBox(CustomMessage('NoWriteAccess'), mbError, MB_OK, IDOK);
    Result := False;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  QuitRdm;
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  QuitRdm;
  Result := True;
end;

function DataFolder: String;
begin
  Result := GetEnv('RDM_DATA_DIR');
  if Result = '' then
    Result := ExpandConstant('{userappdata}\RDM');
end;

{ A person is asked, and the answer defaults to keeping everything. A script has to ask for it. }
function ShouldRemoveData: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/REMOVEDATA') = 0 then
      Result := True;
  if (not Result) and (not UninstallSilent) then
    Result := MsgBox(CustomMessage('RemoveData'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and ShouldRemoveData then
    DelTree(DataFolder, True, True, True);
end;
