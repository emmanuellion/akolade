; Windows installer (Inno Setup 6). Built by the Release workflow:
;   iscc /DAppVersion=0.2.0 /DExe=target\release\akolade.exe /Odist packaging\windows\akolade.iss
; Installs for the current user (no admin rights), adds a Start menu shortcut and an uninstaller.
; akolade.ico: the app icon at 16 to 256 px (PNG entries), made from packaging/icon.png.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Exe
  #define Exe "..\..\target\release\akolade.exe"
#endif

[Setup]
; Never change AppId: Windows uses it to find the installed version on upgrade and uninstall.
AppId={{B7E2D4A9-3C51-4F86-A0D2-7E9C4B1F5A38}
AppName=Akolade
AppVersion={#AppVersion}
AppVerName=Akolade {#AppVersion}
AppPublisher=Emmanuel Lion
AppPublisherURL=https://emmanuellion.github.io/akolade/
AppSupportURL=https://github.com/emmanuellion/akolade/issues
AppUpdatesURL=https://github.com/emmanuellion/akolade/releases
DefaultDirName={autopf}\Akolade
DefaultGroupName=Akolade
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableReadyPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
SetupIconFile=akolade.ico
UninstallDisplayIcon={app}\akolade.ico
UninstallDisplayName=Akolade
OutputBaseFilename=akolade-{#AppVersion}-windows-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
ShowLanguageDialog=no
CloseApplications=yes

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "fr"; MessagesFile: "compiler:Languages\French.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Exe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "akolade.ico"; DestDir: "{app}"
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"

[Icons]
Name: "{autoprograms}\Akolade"; Filename: "{app}\akolade.exe"; IconFilename: "{app}\akolade.ico"
Name: "{autodesktop}\Akolade"; Filename: "{app}\akolade.exe"; IconFilename: "{app}\akolade.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\akolade.exe"; Description: "{cm:LaunchProgram,Akolade}"; Flags: nowait postinstall skipifsilent
