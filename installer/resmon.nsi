; Resource Monitor installer (NSIS 3, buildable with Linux makensis).

!include "MUI2.nsh"

!define APP_NAME "Resource Monitor"
!define APP_EXE "resmon.exe"
!define APP_VERSION "1.0.0"
!define APP_PUBLISHER "resourcemonitor.app"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ResourceMonitor"

Name "${APP_NAME}"
OutFile "..\dist\ResourceMonitorSetup.exe"
InstallDir "$PROGRAMFILES64\${APP_NAME}"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
Unicode true

!define MUI_ICON "..\assets\app-classic.ico"
!define MUI_UNICON "..\assets\app-classic.ico"

VIProductVersion "${APP_VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey "FileVersion" "${APP_VERSION}"
VIAddVersionKey "ProductVersion" "${APP_VERSION}"
VIAddVersionKey "FileDescription" "${APP_NAME} installer"
VIAddVersionKey "LegalCopyright" "${APP_PUBLISHER}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Launch ${APP_NAME} now"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "${APP_NAME} (required)" SecMain
  SectionIn RO
  ; Stop a running instance so the exe can be replaced.
  nsExec::Exec 'taskkill /F /IM ${APP_EXE}'
  Sleep 400
  SetOutPath "$INSTDIR"
  File "..\target\x86_64-pc-windows-gnu\release\${APP_EXE}"
  File "..\target\x86_64-pc-windows-gnu\release\resmon-mcp.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\Uninstall ${APP_NAME}.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "URLInfoAbout" "https://resourcemonitor.app"
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Desktop shortcut" SecDesktop
  CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
SectionEnd

Section "Start with Windows (elevated, no UAC prompts)" SecAutostart
  nsExec::Exec 'schtasks /Delete /F /TN ResMon'
  nsExec::Exec 'schtasks /Create /F /TN ResourceMonitor /SC ONLOGON /RL HIGHEST /TR "\"$INSTDIR\${APP_EXE}\""'
SectionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /F /IM ${APP_EXE}'
  Sleep 400
  nsExec::Exec 'schtasks /Delete /F /TN ResourceMonitor'
  nsExec::Exec 'schtasks /Delete /F /TN ResMon'
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\resmon-mcp.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\Uninstall ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"
  DeleteRegKey HKLM "${UNINST_KEY}"
  ; Per-user config/logs are left in %LOCALAPPDATA% on purpose.
SectionEnd
