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
  ; Stop everything holding either binary open. The MCP shim matters as much as
  ; the app: it is spawned by whichever AI client connects, it stays alive for
  ; that client's whole session, and Windows locks a running exe against being
  ; overwritten. Killing only resmon.exe is why an install could leave the app
  ; updated and the shim months old, with nothing on screen to say so — the two
  ; then speak different versions of the pipe protocol at each other.
  nsExec::Exec 'taskkill /F /IM ${APP_EXE}'
  nsExec::Exec 'taskkill /F /IM resmon-mcp.exe'
  Sleep 600
  SetOutPath "$INSTDIR"
  ; Refuse the install rather than half-finish it. A mismatched pair is worse
  ; than no install, because a stale shim fails quietly and at a distance.
  SetOverwrite on
  ClearErrors
  File "..\target\x86_64-pc-windows-gnu\release\${APP_EXE}"
  File "..\target\x86_64-pc-windows-gnu\release\resmon-mcp.exe"
  IfErrors 0 filesWritten
    MessageBox MB_ICONSTOP "Could not replace the program files.$\r$\n$\r$\nSomething still has them open. Close Resource Monitor and any AI tool connected to it, then run this installer again."
    Abort
  filesWritten:
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
  ; Same pair as the install: the shim holds its own file open.
  nsExec::Exec 'taskkill /F /IM ${APP_EXE}'
  nsExec::Exec 'taskkill /F /IM resmon-mcp.exe'
  Sleep 600
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
