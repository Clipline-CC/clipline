Unicode true
ManifestDPIAware true
RequestExecutionLevel user
CRCCheck force

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "WordFunc.nsh"
!ifndef CLIPLINE_PACKAGE_DIR
  !error "CLIPLINE_PACKAGE_DIR is required"
!endif
!include "${CLIPLINE_PACKAGE_DIR}\installer-shared.nsh"

!insertmacro VersionCompare

Name "${CLIPLINE_CANDIDATE_NAME}"
Caption "${CLIPLINE_CANDIDATE_NAME} ${CLIPLINE_VERSION}"
BrandingText "${CLIPLINE_PUBLISHER}"
OutFile "${CLIPLINE_OUTPUT_FILE}"
InstallDir "${CLIPLINE_INSTALL_DIRECTORY}"
Icon "${CLIPLINE_ICON_PATH}"
UninstallIcon "${CLIPLINE_ICON_PATH}"
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${CLIPLINE_VERSION_NUMERIC}"
VIAddVersionKey /LANG=1033 "ProductName" "${CLIPLINE_PRODUCT_NAME}"
VIAddVersionKey /LANG=1033 "CompanyName" "${CLIPLINE_PUBLISHER}"
VIAddVersionKey /LANG=1033 "FileDescription" "Clipline Slint internal candidate installer"
VIAddVersionKey /LANG=1033 "FileVersion" "${CLIPLINE_VERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${CLIPLINE_VERSION}"
VIAddVersionKey /LANG=1033 "InternalName" "Clipline-Slint-Internal-Candidate"

Var PassiveMode
Var ReinstallMode
Var RestartMode
Var UpdateMode
Var ArgsMarker
Var ExistingVersion
Var ExistingVariant
Var VersionRelation
Var PackageFenceHandle

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function CliplineHasExactFlag
  Exch $R0
  Push $R1
  Push $R2
  Push $R3
  Push $R4
  Push $R5

  ${GetParameters} $R1
  StrCpy $R2 ""
  StrCpy $R3 0
  StrCpy $R4 "0"
  StrCpy $R5 "0"

  clipline_flag_scan_loop:
    StrCpy $0 $R1 1 $R3
    StrCmp $0 "" clipline_flag_scan_finish
    IntOp $R3 $R3 + 1
    StrCmp $0 '$\"' clipline_flag_scan_quote
    StrCmp $R4 "1" clipline_flag_scan_append
    StrCmp $0 " " clipline_flag_scan_boundary
    StrCmp $0 '$\t' clipline_flag_scan_boundary
  clipline_flag_scan_append:
    StrCpy $R2 "$R2$0"
    Goto clipline_flag_scan_loop
  clipline_flag_scan_quote:
    StrCmp $R4 "1" 0 +3
      StrCpy $R4 "0"
      Goto clipline_flag_scan_loop
    StrCpy $R4 "1"
    Goto clipline_flag_scan_loop
  clipline_flag_scan_boundary:
    StrCmp $R2 $R0 0 +2
      StrCpy $R5 "1"
    StrCpy $R2 ""
    StrCmp $R5 "1" clipline_flag_scan_done clipline_flag_scan_loop
  clipline_flag_scan_finish:
    StrCmp $R2 $R0 0 clipline_flag_scan_done
      StrCpy $R5 "1"
  clipline_flag_scan_done:
    StrCpy $R0 $R5

  Pop $R5
  Pop $R4
  Pop $R3
  Pop $R2
  Pop $R1
  Exch $R0
FunctionEnd

Function CliplineAcquirePackageFence
  System::Call 'kernel32::CreateMutexW(p 0, i 0, w "${CLIPLINE_PACKAGE_FENCE_NAME}") p.rPackageFenceHandle'
  StrCmp $PackageFenceHandle 0 clipline_fence_failed
  System::Call 'kernel32::WaitForSingleObject(p rPackageFenceHandle, i ${CLIPLINE_PACKAGE_FENCE_WAIT_MS}) i.r0'
  StrCmp $0 0 clipline_fence_acquired
  StrCmp $0 128 clipline_fence_acquired
  System::Call 'kernel32::CloseHandle(p rPackageFenceHandle)'
  StrCpy $PackageFenceHandle 0
clipline_fence_failed:
  IfSilent +2
    MessageBox MB_ICONSTOP "Clipline is still running. Close it from the tray, then retry this operation."
  Abort
clipline_fence_acquired:
FunctionEnd

Function CliplineReleasePackageFence
  StrCmp $PackageFenceHandle 0 clipline_fence_release_done
  System::Call 'kernel32::ReleaseMutex(p rPackageFenceHandle)'
  System::Call 'kernel32::CloseHandle(p rPackageFenceHandle)'
  StrCpy $PackageFenceHandle 0
clipline_fence_release_done:
FunctionEnd

Function .onInit
  SetShellVarContext current
  SetRegView 64
  !insertmacro CLIPLINE_READ_EXACT_FLAG "/P" $PassiveMode
  !insertmacro CLIPLINE_READ_EXACT_FLAG "/REINSTALL" $ReinstallMode
  !insertmacro CLIPLINE_READ_EXACT_FLAG "/R" $RestartMode
  !insertmacro CLIPLINE_READ_EXACT_FLAG "/UPDATE" $UpdateMode
  !insertmacro CLIPLINE_READ_EXACT_FLAG "/ARGS" $ArgsMarker

  ${If} $PassiveMode == "1"
    SetSilent silent
  ${EndIf}
  ; /ARGS is an exact empty compatibility marker in the current signed updater
  ; contract. Future relaunch arguments require a separately bounded parser.

  Call CliplineAcquirePackageFence
  ReadRegStr $ExistingVariant HKCU "${CLIPLINE_CANDIDATE_STATE_KEY}" "InstallVariant"
  ${If} $ExistingVariant != ""
  ${AndIf} $ExistingVariant != "${CLIPLINE_VARIANT_ID}"
    IfSilent +2
      MessageBox MB_ICONSTOP "A different Clipline installer variant is already present. Uninstall it before installing ${CLIPLINE_VARIANT_NAME}."
    Abort
  ${EndIf}

  ReadRegStr $ExistingVersion HKCU "${CLIPLINE_UNINSTALL_KEY}" "DisplayVersion"
  ${If} $ExistingVersion != ""
    ${VersionCompare} "$ExistingVersion" "${CLIPLINE_VERSION}" $VersionRelation
    ${If} $VersionRelation == 1
      IfSilent +2
        MessageBox MB_ICONSTOP "Downgrading Clipline from $ExistingVersion to ${CLIPLINE_VERSION} is not permitted."
      Abort
    ${ElseIf} $VersionRelation == 0
    ${AndIf} $ReinstallMode != "1"
    ${AndIf} $UpdateMode != "1"
      IfSilent +2
        MessageBox MB_ICONSTOP "Clipline ${CLIPLINE_VERSION} is already installed. Use /REINSTALL for an explicit reinstall."
      Abort
    ${EndIf}
  ${EndIf}
FunctionEnd

Section "Clipline" SEC_CLIPLINE
  SectionIn RO
  SetShellVarContext current
  StrCmp $INSTDIR "${CLIPLINE_INSTALL_DIRECTORY}" +4
    IfSilent +2
      MessageBox MB_ICONSTOP "The internal candidate install directory cannot be changed."
    Abort
  SetOutPath "$INSTDIR"
  File /oname=${CLIPLINE_INSTALLED_BINARY} "${CLIPLINE_STAGE_DIR}\${CLIPLINE_INTERNAL_BINARY}"
  File "${CLIPLINE_STAGE_DIR}\icon.ico"
  File "${CLIPLINE_STAGE_DIR}\THIRD-PARTY-NOTICES.md"
  File "${CLIPLINE_STAGE_DIR}\package-manifest.json"

  SetOutPath "$INSTDIR\ffmpeg"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\README.md"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\PROVENANCE.json"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\LICENSE.txt"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\ffmpeg.exe"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\avcodec-62.dll"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\avdevice-62.dll"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\avfilter-11.dll"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\avformat-62.dll"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\avutil-60.dll"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\swresample-6.dll"
  File "${CLIPLINE_STAGE_DIR}\ffmpeg\swscale-9.dll"

  SetOutPath "$INSTDIR"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  ${If} $UpdateMode != "1"
    CreateDirectory "$SMPROGRAMS\Clipline Slint Candidate"
    CreateShortcut "$SMPROGRAMS\Clipline Slint Candidate\${CLIPLINE_VARIANT_NAME}.lnk" "$INSTDIR\${CLIPLINE_INSTALLED_BINARY}" "" "$INSTDIR\icon.ico" 0
    CreateShortcut "$DESKTOP\${CLIPLINE_CANDIDATE_NAME}.lnk" "$INSTDIR\${CLIPLINE_INSTALLED_BINARY}" "" "$INSTDIR\icon.ico" 0
  ${EndIf}

  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "DisplayName" "${CLIPLINE_CANDIDATE_NAME}"
  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "DisplayVersion" "${CLIPLINE_VERSION}"
  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "Publisher" "${CLIPLINE_PUBLISHER}"
  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "InstallVariant" "${CLIPLINE_VARIANT_ID}"
  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\icon.ico,0"
  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "UninstallString" '$"$INSTDIR\Uninstall.exe$"'
  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "QuietUninstallString" '$"$INSTDIR\Uninstall.exe$" /S'
  WriteRegDWORD HKCU "${CLIPLINE_UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${CLIPLINE_UNINSTALL_KEY}" "NoRepair" 1
  WriteRegDWORD HKCU "${CLIPLINE_UNINSTALL_KEY}" "EstimatedSize" ${CLIPLINE_ESTIMATED_SIZE_KIB}
  WriteRegStr HKCU "${CLIPLINE_UNINSTALL_KEY}" "ProductIdentifier" "${CLIPLINE_PRODUCT_IDENTITY}"
  WriteRegStr HKCU "${CLIPLINE_CANDIDATE_STATE_KEY}" "InstallVariant" "${CLIPLINE_VARIANT_ID}"
  WriteRegStr HKCU "${CLIPLINE_APP_PATH_KEY}" "" "$INSTDIR\${CLIPLINE_INSTALLED_BINARY}"
  WriteRegStr HKCU "${CLIPLINE_APP_PATH_KEY}" "Path" "$INSTDIR"
SectionEnd

Function .onInstSuccess
  Call CliplineReleasePackageFence
  ${If} $RestartMode == "1"
    ExecShell "open" "$INSTDIR\${CLIPLINE_INSTALLED_BINARY}"
  ${EndIf}
FunctionEnd

Function .onGUIEnd
  Call CliplineReleasePackageFence
FunctionEnd

Function un.onInit
  SetShellVarContext current
  SetRegView 64
  StrCmp $INSTDIR "${CLIPLINE_INSTALL_DIRECTORY}" +4
    IfSilent +2
      MessageBox MB_ICONSTOP "The internal candidate uninstall directory is invalid."
    Abort
  Call un.CliplineAcquirePackageFence
FunctionEnd

Function un.CliplineAcquirePackageFence
  System::Call 'kernel32::CreateMutexW(p 0, i 0, w "${CLIPLINE_PACKAGE_FENCE_NAME}") p.rPackageFenceHandle'
  StrCmp $PackageFenceHandle 0 clipline_un_fence_failed
  System::Call 'kernel32::WaitForSingleObject(p rPackageFenceHandle, i ${CLIPLINE_PACKAGE_FENCE_WAIT_MS}) i.r0'
  StrCmp $0 0 clipline_un_fence_acquired
  StrCmp $0 128 clipline_un_fence_acquired
  System::Call 'kernel32::CloseHandle(p rPackageFenceHandle)'
  StrCpy $PackageFenceHandle 0
clipline_un_fence_failed:
  IfSilent +2
    MessageBox MB_ICONSTOP "Clipline is still running. Close it from the tray, then retry uninstall."
  Abort
clipline_un_fence_acquired:
FunctionEnd

Function un.CliplineDeleteRequired
  Exch $0
  IfFileExists "$0" 0 clipline_un_delete_done
  ClearErrors
  Delete "$0"
  IfErrors 0 clipline_un_delete_done
    IfSilent +2
      MessageBox MB_ICONSTOP "Could not remove $0. Clipline may still be running; close it and retry uninstall."
    Abort
clipline_un_delete_done:
  Pop $0
FunctionEnd

Function un.CliplineRemoveRequiredDirectory
  Exch $0
  IfFileExists "$0\*.*" 0 clipline_un_rmdir_try
    IfSilent +2
      MessageBox MB_ICONSTOP "Unexpected files remain in $0. The uninstall entry was preserved."
    Abort
clipline_un_rmdir_try:
  IfFileExists "$0" 0 clipline_un_rmdir_done
  ClearErrors
  RMDir "$0"
  IfErrors 0 clipline_un_rmdir_done
    IfSilent +2
      MessageBox MB_ICONSTOP "Could not remove $0. The uninstall entry was preserved."
    Abort
clipline_un_rmdir_done:
  Pop $0
FunctionEnd

Function un.onGUIEnd
  StrCmp $PackageFenceHandle 0 clipline_un_fence_release_done
  System::Call 'kernel32::ReleaseMutex(p rPackageFenceHandle)'
  System::Call 'kernel32::CloseHandle(p rPackageFenceHandle)'
  StrCpy $PackageFenceHandle 0
clipline_un_fence_release_done:
FunctionEnd

Section "Uninstall"
  SetShellVarContext current
  Push "$INSTDIR\${CLIPLINE_INSTALLED_BINARY}"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\icon.ico"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\THIRD-PARTY-NOTICES.md"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\package-manifest.json"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\README.md"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\PROVENANCE.json"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\LICENSE.txt"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\ffmpeg.exe"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\avcodec-62.dll"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\avdevice-62.dll"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\avfilter-11.dll"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\avformat-62.dll"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\avutil-60.dll"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\swresample-6.dll"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg\swscale-9.dll"
  Call un.CliplineDeleteRequired
  Push "$INSTDIR\ffmpeg"
  Call un.CliplineRemoveRequiredDirectory
  Push "$INSTDIR\Uninstall.exe"
  Call un.CliplineDeleteRequired

  Delete "$DESKTOP\${CLIPLINE_CANDIDATE_NAME}.lnk"
  Delete "$SMPROGRAMS\Clipline Slint Candidate\${CLIPLINE_VARIANT_NAME}.lnk"
  RMDir "$SMPROGRAMS\Clipline Slint Candidate"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "${CLIPLINE_APP_PATH_KEY}"
  DeleteRegKey HKCU "${CLIPLINE_UNINSTALL_KEY}"
  ReadRegStr $0 HKCU "${CLIPLINE_CANDIDATE_STATE_KEY}" "InstallVariant"
  ${If} $0 == "${CLIPLINE_VARIANT_ID}"
    DeleteRegValue HKCU "${CLIPLINE_CANDIDATE_STATE_KEY}" "InstallVariant"
    DeleteRegKey /ifempty HKCU "${CLIPLINE_CANDIDATE_STATE_KEY}"
  ${EndIf}
SectionEnd
