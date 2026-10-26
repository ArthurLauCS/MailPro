; MailPro 安装包（Inno Setup 6）。用 scripts\installer.ps1 构建：它先编译，再传入版本号。
; 按当前用户安装到 %LOCALAPPDATA%\Programs\MailPro，不需要管理员权限。
; 卸载时删掉全部本地数据：数据目录（邮件缓存、设置、WebView2 数据）和凭据管理器里存的密码。

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId=MailPro
AppName=MailPro
AppVersion={#Version}
AppVerName=MailPro {#Version}
AppPublisher=ArthurLau
AppPublisherURL=https://github.com/ArthurLauCS/MailPro
DefaultDirName={autopf}\MailPro
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\target\installer
OutputBaseFilename=MailPro-Setup-{#Version}
SetupIconFile=..\crates\app\assets\app.ico
UninstallDisplayIcon={app}\mailpro.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UsePreviousLanguage=no
; 升级时结束正在运行的实例
CloseApplications=force
RestartApplications=no

[Languages]
Name: "zh"; MessagesFile: "ChineseSimplified.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "..\target\release\mailpro.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\crates\app\assets\fonts\*.ttf"; DestDir: "{app}\fonts"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\THIRD_PARTY_NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\MailPro"; Filename: "{app}\mailpro.exe"
Name: "{autodesktop}\MailPro"; Filename: "{app}\mailpro.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\mailpro.exe"; Description: "{cm:LaunchProgram,MailPro}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; 在删文件之前跑，exe 还在
Filename: "{app}\mailpro.exe"; Parameters: "--purge-credentials"; Flags: runhidden waituntilterminated; RunOnceId: "PurgeCredentials"

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\MailPro"
