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
!include "FileFunc.nsh"
!include "LogicLib.nsh"
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE ValidateInstallRoot
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
Var Upgrade
Var VerifyPlan
Var PlanHandle
Var PlanLine
Var PlanKind
Var PlanRelative
Var PlanPath
Var PlanSegment
Var PlanCount

!macro CheckPath PREFIX PATH KIND
  Push "${PATH}"
  Push "${KIND}"
  Call ${PREFIX}CheckPath
!macroend
!include "${FILE_MANIFEST}"

!macro SafetyFunctions PREFIX
Function ${PREFIX}Refuse
  Pop $0
  MessageBox MB_OK|MB_ICONSTOP "$0" /SD IDOK
  SetErrorLevel 1603
  Abort
FunctionEnd

; Check the path and every ancestor before copy/delete. Missing paths are allowed;
; junctions, symlinks and inaccessible paths fail without following them.
Function ${PREFIX}CheckPath
  Pop $1
  Pop $0
  StrCpy $3 1
path_loop:
  System::Call 'kernel32::GetFileAttributesW(w r0) i.r2'
  IntCmp $2 -1 path_missing
  IntOp $4 $2 & 0x400
  IntCmp $4 0 path_kind
  Push "Reparse points are not allowed in the installation path. The operation was stopped."
  Call ${PREFIX}Refuse
path_kind:
  IntOp $4 $2 & 0x10
  StrCmp $1 "dir" 0 path_file
  IntCmp $4 0 path_invalid path_parent path_parent
path_file:
  IntCmp $3 1 0 path_parent path_parent
  IntCmp $4 0 path_parent path_invalid path_invalid
path_missing:
  System::Call 'kernel32::GetLastError() i.r2'
  IntCmp $2 2 path_parent
  IntCmp $2 3 path_parent
path_invalid:
  Push "The installation path is inaccessible or conflicts with a package path. The operation was stopped."
  Call ${PREFIX}Refuse
path_parent:
  ${GetParent} "$0" $4
  StrCmp $4 "" path_done
  StrCmp $4 $0 path_done
  StrCpy $0 $4
  StrCpy $1 "dir"
  StrCpy $3 0
  Goto path_loop
path_done:
FunctionEnd

Function ${PREFIX}RequireOwnedRoot
  !insertmacro CheckPath "${PREFIX}" "$INSTDIR" "dir"
  !insertmacro CheckPath "${PREFIX}" "$INSTDIR\.vibeface-install-owned" "file"
  !insertmacro CheckPath "${PREFIX}" "$INSTDIR\Uninstall.exe" "file"
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "InstallLocation"
  StrCmp $0 "$INSTDIR" 0 ownership_invalid
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "UninstallString"
  StrCmp $0 '$\"$INSTDIR\Uninstall.exe$\"' 0 ownership_invalid
  ClearErrors
  FileOpen $0 "$INSTDIR\.vibeface-install-owned" r
  IfErrors ownership_invalid
  FileReadUTF16LE $0 $1
  FileReadUTF16LE $0 $2
  FileReadUTF16LE $0 $3
  FileClose $0
  StrCmp $1 "vibeface-native-install-v2$\r$\n" 0 ownership_invalid
  StrCmp $2 "$INSTDIR$\r$\n" 0 ownership_invalid
  StrCmp $3 "" 0 ownership_invalid
  IfFileExists "$INSTDIR\Uninstall.exe" ownership_done
ownership_invalid:
  Push "This directory is not an owned Vibeface installation. The operation was stopped."
  Call ${PREFIX}Refuse
ownership_done:
FunctionEnd
!macroend
!insertmacro SafetyFunctions ""
!insertmacro SafetyFunctions "un."

Function ValidateInstallRoot
  GetFullPathName $INSTDIR "$INSTDIR"
  StrCpy $0 $INSTDIR 2
  StrCmp $0 "\\" invalid_root
  ${GetParent} "$INSTDIR" $0
  StrCmp $0 "" invalid_root
  !insertmacro CheckPath "" "$INSTDIR" "dir"
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "InstallLocation"
  StrCmp $0 "" check_directory
  StrCmp $0 "$INSTDIR" check_directory invalid_root
check_directory:
  IfFileExists "$INSTDIR\.vibeface-install-owned" owned_directory
  ClearErrors
  FindFirst $0 $1 "$INSTDIR\*"
  IfErrors directory_empty
find_next:
  StrCmp $1 "" find_empty
  StrCmp $1 "." find_continue
  StrCmp $1 ".." find_continue
  FindClose $0
  Goto invalid_root
find_continue:
  FindNext $0 $1
  Goto find_next
find_empty:
  FindClose $0
directory_empty:
  !insertmacro ValidateOwnedPaths ""
  Return
owned_directory:
  Call RequireOwnedRoot
  !insertmacro ValidateOwnedPaths ""
  Return
invalid_root:
  Push "Choose an empty directory or this account's marked Vibeface installation. No files were copied."
  Call Refuse
FunctionEnd

Function un.ValidatePlanRelative
  StrLen $0 $PlanRelative
  IntCmp $0 0 plan_path_invalid
  IntCmp $0 768 0 0 plan_path_invalid
  StrCpy $1 0
  StrCpy $PlanSegment ""
plan_character:
  StrCpy $2 $PlanRelative 1 $1
  StrCmp $2 "" plan_segment_end
  StrCpy $3 "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-\"
  StrCpy $4 0
plan_allowed:
  StrCpy $5 $3 1 $4
  StrCmp $5 "" plan_path_invalid
  StrCmp $2 $5 plan_character_valid
  IntOp $4 $4 + 1
  Goto plan_allowed
plan_character_valid:
  StrCmp $2 "\" plan_segment_end
  StrCpy $PlanSegment "$PlanSegment$2"
  IntOp $1 $1 + 1
  Goto plan_character
plan_segment_end:
  StrCmp $PlanSegment "" plan_path_invalid
  StrCmp $PlanSegment "." plan_path_invalid
  StrCmp $PlanSegment ".." plan_path_invalid
  StrCpy $3 $PlanSegment 1 -1
  StrCmp $3 "." plan_path_invalid
  StrCmp $2 "" plan_path_done
  StrCpy $PlanSegment ""
  IntOp $1 $1 + 1
  Goto plan_character
plan_path_invalid:
  Push "The proposed upgrade has an invalid relative package path. The installation was not changed."
  Call un.Refuse
plan_path_done:
  GetFullPathName $PlanPath "$INSTDIR\$PlanRelative"
  StrCpy $0 "$INSTDIR\"
  StrLen $1 $0
  StrCpy $2 $PlanPath $1
  StrCmp $2 $0 0 plan_path_invalid
  StrCpy $2 $PlanPath "" $1
  StrCmp $2 $PlanRelative 0 plan_path_invalid
FunctionEnd

; This mode reads a proposed plan and compares existing paths with this OLD
; uninstaller's compiled ownership. The input is never used by deletion code.
Function un.VerifyNewPlan
  !insertmacro CheckPath "un." "$VerifyPlan" "file"
  ClearErrors
  FileOpen $PlanHandle "$VerifyPlan" r
  IfErrors invalid_plan
  FileRead $PlanHandle $PlanLine
  StrCmp $PlanLine "vibeface-install-path-plan-v2$\r$\n" 0 invalid_plan
  StrCpy $PlanCount 0
next_plan_line:
  ClearErrors
  FileRead $PlanHandle $PlanLine
  IfErrors plan_finished
  IntOp $PlanCount $PlanCount + 1
  IntCmp $PlanCount 16384 0 0 invalid_plan
  StrLen $0 $PlanLine
  IntCmp $0 4 0 invalid_plan
  StrCpy $0 $PlanLine 2 -2
  StrCmp $0 "$\r$\n" 0 invalid_plan
  StrCpy $PlanLine $PlanLine -2
  StrCpy $PlanKind $PlanLine 1
  StrCmp $PlanKind "F" kind_valid
  StrCmp $PlanKind "D" 0 invalid_plan
kind_valid:
  StrCpy $0 $PlanLine 1 1
  StrCmp $0 "|" 0 invalid_plan
  StrCpy $PlanRelative $PlanLine "" 2
  Call un.ValidatePlanRelative
  StrCmp $PlanKind "F" plan_file
  !insertmacro CheckPath "un." "$PlanPath" "dir"
  Goto plan_path_checked
plan_file:
  !insertmacro CheckPath "un." "$PlanPath" "file"
plan_path_checked:
  IfFileExists "$PlanPath" 0 next_plan_line
  Call un.IsOwnedPath
  Pop $0
  StrCmp $0 1 next_plan_line
  Push "A proposed upgrade path contains an unowned file or directory. The existing installation was not changed."
  Call un.Refuse
invalid_plan:
  Push "The proposed upgrade path plan is invalid. The existing installation was not changed."
  Call un.Refuse
plan_finished:
  FileClose $PlanHandle
  IntCmp $PlanCount 0 invalid_plan
  SetErrorLevel 0
FunctionEnd

Function .onInit
  SetShellVarContext current
  SetRegView 64
FunctionEnd

Function un.onInit
  SetShellVarContext current
  SetRegView 64
  GetFullPathName $INSTDIR "$INSTDIR"
  Call un.RequireOwnedRoot
  !insertmacro ValidateOwnedPaths "un."
  StrCpy $Upgrade 0
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/VERIFYPLAN=" $VerifyPlan
  IfErrors no_verify_plan
  StrCmp $VerifyPlan "" invalid_verify_plan
  Call un.VerifyNewPlan
  SetErrorLevel 0
  Quit
invalid_verify_plan:
  Push "The proposed upgrade path plan is missing. The existing installation was not changed."
  Call un.Refuse
no_verify_plan:
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/UPGRADE=" $1
  IfErrors no_upgrade
  StrCmp $1 "1" 0 no_upgrade
  StrCpy $Upgrade 1
no_upgrade:
FunctionEnd

Section "Vibeface"
  SetShellVarContext current
  Call ValidateInstallRoot
  IfFileExists "$INSTDIR\.vibeface-install-owned" 0 install_payload
  ; Only the v2 ownership marker reaches this path. Earlier uninstallers do not
  ; understand validation-only mode and must never receive an unknown switch.
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  ClearErrors
  File /oname=vibeface-next-paths.txt "${PATH_PLAN}"
  IfErrors upgrade_failed
  ExecWait '$\"$INSTDIR\Uninstall.exe$\" /S /VERIFYPLAN=$\"$PLUGINSDIR\vibeface-next-paths.txt$\" _?=$INSTDIR' $0
  IfErrors upgrade_failed
  IntCmp $0 0 upgrade_preflight_done upgrade_failed upgrade_failed
upgrade_preflight_done:
  ; The previous uninstaller carries its own exact build-generated deletion list,
  ; so obsolete hashed assets are removed without trusting an editable manifest.
  ClearErrors
  ExecWait '$\"$INSTDIR\Uninstall.exe$\" /S /UPGRADE=1 _?=$INSTDIR' $0
  IfErrors upgrade_failed
  IntCmp $0 0 upgrade_checked
upgrade_failed:
  Push "The owned installation could not be prepared for upgrade. Nothing will be copied."
  Call Refuse
upgrade_checked:
  Call RequireOwnedRoot
install_payload:
  !insertmacro ValidateOwnedPaths ""
  !insertmacro AssertNewPayloadVacant
  SetOutPath "$INSTDIR"
  ClearErrors
  File /r "${APP_DIR}\*"
  IfErrors copy_failed
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  IfErrors copy_failed
  FileOpen $0 "$INSTDIR\.vibeface-install-owned" w
  IfErrors copy_failed
  FileWriteUTF16LE $0 "vibeface-native-install-v2$\r$\n$INSTDIR$\r$\n"
  FileClose $0
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "DisplayName" "Vibeface"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
  CreateShortcut "$SMPROGRAMS\Vibeface.lnk" "$INSTDIR\Vibeface.exe"
  WriteRegStr HKCU "Software\Classes\vibeface" "" "URL:Vibeface"
  WriteRegStr HKCU "Software\Classes\vibeface" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\vibeface\shell\open\command" "" '$\"$INSTDIR\Vibeface.exe$\" $\"%1$\"'
  Goto install_done
copy_failed:
  Push "Installation could not write all owned files. User files have not been removed."
  Call Refuse
install_done:
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  Call un.RequireOwnedRoot
  !insertmacro ValidateOwnedPaths "un."
  !insertmacro DeleteOwnedPayload
  !insertmacro RemoveOwnedDirectories
  StrCmp $Upgrade 1 uninstall_done
  Delete "$SMPROGRAMS\Vibeface.lnk"
  ReadRegStr $0 HKCU "Software\Classes\vibeface\shell\open\command" ""
  StrCmp $0 '$\"$INSTDIR\Vibeface.exe$\" $\"%1$\"' 0 keep_protocol
  DeleteRegKey HKCU "Software\Classes\vibeface"
keep_protocol:
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface"
  !insertmacro CheckPath "un." "$INSTDIR\.vibeface-install-owned" "file"
  Delete "$INSTDIR\.vibeface-install-owned"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
uninstall_done:
SectionEnd
