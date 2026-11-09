#ifndef PayloadDir
  #define PayloadDir "..\target\windows-package"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
AppId={{07E28C25-0E6D-497E-B9EE-1E93DFBE70B5}
AppName=Telinha
AppVersion={#AppVersion}
AppPublisher=Telinha
DefaultDirName={localappdata}\Programs\Telinha
DefaultGroupName=Telinha
PrivilegesRequired=lowest
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
MinVersion=10.0.22000
OutputDir={#OutputDir}
OutputBaseFilename=Telinha-Setup-{#AppVersion}-windows-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\Telinha Desktop.exe
CloseApplications=yes
RestartApplications=no
SetupLogging=yes

[Languages]
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[Tasks]
Name: "desktopicon"; Description: "Criar atalho na area de trabalho"; Flags: unchecked
Name: "startup"; Description: "Manter o Telinha perto do relogio ao entrar no Windows (sem iniciar transmissao)"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Telinha"; Filename: "{app}\Telinha Desktop.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\Telinha"; Filename: "{app}\Telinha Desktop.exe"; WorkingDir: "{app}"; Tasks: desktopicon
Name: "{userstartup}\Telinha"; Filename: "{app}\Telinha Desktop.exe"; Parameters: "--tray"; WorkingDir: "{app}"; Tasks: startup

[Run]
Filename: "{app}\Telinha Desktop.exe"; Description: "Abrir Telinha"; Flags: nowait postinstall skipifsilent
