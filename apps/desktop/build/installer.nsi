Unicode true
RequestExecutionLevel user
Name "Vibeface"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\Vibeface"
SetCompressor /SOLID lzma
Icon "${__FILEDIR__}\icon.ico"
UninstallIcon "${__FILEDIR__}\icon.ico"
!ifdef SIGNING_SCRIPT
  !uninstfinalize 'node "${SIGNING_SCRIPT}" sign "%1"'
!endif
!include "MUI2.nsh"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
Section "Vibeface"
  SetShellVarContext current
  SetOutPath "$INSTDIR"
  File /r "${APP_DIR}\*"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Vibeface.lnk" "$INSTDIR\Vibeface.exe"
  WriteRegStr HKCU "Software\Classes\vibeface" "" "URL:Vibeface"
  WriteRegStr HKCU "Software\Classes\vibeface" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\vibeface\shell\open\command" "" '$\"$INSTDIR\Vibeface.exe$\" $\"%1$\"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "DisplayName" "Vibeface"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
SectionEnd
Section "Uninstall"
  SetShellVarContext current
  Delete "$SMPROGRAMS\Vibeface.lnk"
  DeleteRegKey HKCU "Software\Classes\vibeface"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface"
  RMDir /r "$INSTDIR"
SectionEnd
