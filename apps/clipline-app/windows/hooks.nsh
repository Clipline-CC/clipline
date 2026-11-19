!include LogicLib.nsh
!include WinVer.nsh

; The standalone installer deploys its Fixed Version WebView2 runtime here
; (`webviewInstallMode.path` in tauri.standalone.conf.json). Keep it short: a
; long runtime path crashes the sandboxed GPU process on some drivers.
!define CLIPLINE_WEBVIEW2_DIR "$INSTDIR\wv2"
; Before `wv2`, the runtime lived in
; webview2-fixed\Microsoft.WebView2.FixedVersionRuntime.<version>.x64. Tauri
; updates only overwrite files and never delete resources a release drops, so
; an update from that layout leaves ~700 MB behind.
!define CLIPLINE_LEGACY_WEBVIEW2_DIR "$INSTDIR\webview2-fixed"

; Delete a folder tree without following links. NSIS's `RMDir /r` descends
; into a junction inside the tree and deletes the target's files, so walk the
; tree here and only unlink reparse points. Takes the folder on the stack.
!macro CLIPLINE_DEFINE_REMOVE_TREE un
  Function ${un}CliplineRemoveTree
    Exch $0
    Push $1
    Push $2
    Push $3
    Push $4
    FindFirst $1 $2 "$0\*.*"
    ${DoWhile} $2 != ""
      ${If} $2 != "."
      ${AndIf} $2 != ".."
        System::Call 'kernel32::GetFileAttributesW(w "$0\$2") i .r3'
        ${If} $3 <> -1
          IntOp $4 $3 & 0x400 ; FILE_ATTRIBUTE_REPARSE_POINT
          IntOp $3 $3 & 0x10 ; FILE_ATTRIBUTE_DIRECTORY
          ${If} $3 = 0
            Delete "$0\$2"
          ${ElseIf} $4 <> 0
            RMDir "$0\$2"
          ${Else}
            Push "$0\$2"
            Call ${un}CliplineRemoveTree
          ${EndIf}
        ${EndIf}
      ${EndIf}
      FindNext $1 $2
    ${Loop}
    FindClose $1
    RMDir "$0"
    Pop $4
    Pop $3
    Pop $2
    Pop $1
    Pop $0
  FunctionEnd
!macroend
!insertmacro CLIPLINE_DEFINE_REMOVE_TREE ""
!insertmacro CLIPLINE_DEFINE_REMOVE_TREE "un."

; Remove one of Clipline's own folders inside $INSTDIR. A link is removed
; itself and never followed. A runtime that just exited can hold files for a
; moment, so retry briefly; anything left is retried on the next install or
; uninstall. Pass "un." from uninstaller code.
!macro CLIPLINE_REMOVE_OWN_FOLDER un path
  Push $0
  Push $1
  Push $2
  StrCpy $2 0
  ${Do}
    System::Call 'kernel32::GetFileAttributesW(w "${path}") i .r0'
    ${If} $0 = -1
      ${ExitDo}
    ${EndIf}
    IntOp $1 $0 & 0x400 ; FILE_ATTRIBUTE_REPARSE_POINT
    ${If} $1 <> 0
      RMDir "${path}"
      ${ExitDo}
    ${EndIf}
    IntOp $1 $0 & 0x10 ; FILE_ATTRIBUTE_DIRECTORY
    ${If} $1 = 0
      ${ExitDo}
    ${EndIf}
    ${If} $2 >= 5
      DetailPrint "Clipline: could not fully remove ${path}; the next install will retry."
      ${ExitDo}
    ${EndIf}
    ${If} $2 > 0
      Sleep 1000
    ${EndIf}
    Push "${path}"
    Call ${un}CliplineRemoveTree
    IntOp $2 $2 + 1
  ${Loop}
  Pop $2
  Pop $1
  Pop $0
!macroend

; From Fixed Version 120, Windows 10 runs the WebView2 renderer in an
; AppContainer, and Microsoft requires unpackaged apps to grant ALL
; APPLICATION PACKAGES (S-1-15-2-1) and ALL RESTRICTED APPLICATION PACKAGES
; (S-1-15-2-2) inherited read/execute on the deployed runtime folder:
; https://learn.microsoft.com/microsoft-edge/webview2/concepts/distribution
; Windows 11 is unaffected, so the grant runs everywhere but only a Windows 10
; failure stops the install. A volume without ACLs (FAT32, exFAT) has nothing
; to grant.
!macro CLIPLINE_GRANT_WEBVIEW2_APPCONTAINER_ACCESS dir
  ${If} ${FileExists} "${dir}\msedgewebview2.exe"
    Push $0
    Push $1
    Push $2
    StrCpy $2 1
    System::Call 'kernel32::GetVolumePathNameW(w "${dir}", w .r1, i ${NSIS_MAX_STRLEN}) i .r0'
    ${If} $0 <> 0
      System::Call 'kernel32::GetVolumeInformationW(w r1, p 0, i 0, p 0, p 0, *i .r2, p 0, i 0) i .r0'
      ${If} $0 = 0
        StrCpy $2 1 ; Unknown: grant anyway.
      ${Else}
        IntOp $2 $2 & 0x8 ; FILE_PERSISTENT_ACLS
      ${EndIf}
    ${EndIf}
    ${If} $2 = 0
      DetailPrint "Clipline: ${dir} is on a volume without permissions; no WebView2 grant needed."
    ${Else}
      ${Do}
        nsExec::ExecToLog '"$SYSDIR\icacls.exe" "${dir}" /grant *S-1-15-2-2:(OI)(CI)(RX) *S-1-15-2-1:(OI)(CI)(RX)'
        Pop $0
        ${If} $0 == "0"
          DetailPrint "Clipline: granted app-sandbox read access to the WebView2 runtime."
          ${ExitDo}
        ${EndIf}
        ${If} ${AtLeastWin11}
          DetailPrint "Clipline: WebView2 runtime grant failed ($0); not required on Windows 11."
          ${ExitDo}
        ${EndIf}
        DetailPrint "Clipline: WebView2 runtime grant failed ($0)."
        ${IfNot} ${Cmd} `MessageBox MB_RETRYCANCEL|MB_ICONSTOP "Clipline could not give Windows' app sandbox read access to its bundled WebView2 runtime (icacls result: $0).$\r$\n$\r$\n${dir}$\r$\n$\r$\nOn Windows 10, Clipline's window cannot open without it. Install into a folder on a local NTFS drive that you own, or use the regular Clipline installer." /SD IDCANCEL IDRETRY`
          Abort "Clipline could not set the WebView2 runtime permissions Windows 10 requires."
        ${EndIf}
      ${Loop}
    ${EndIf}
    Pop $2
    Pop $1
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !ifdef CLIPLINE_STANDALONE_WEBVIEW2
    ; The runtime folder is unversioned and updates never delete files, so a
    ; runtime bump would otherwise leave the previous version's files mixed in.
    ; Close Clipline (which releases the runtime) and start from an empty
    ; folder, so wv2 holds exactly this installer's runtime.
    ${If} ${FileExists} "${CLIPLINE_WEBVIEW2_DIR}\*.*"
      !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
      !insertmacro CLIPLINE_REMOVE_OWN_FOLDER "" "${CLIPLINE_WEBVIEW2_DIR}"
    ${EndIf}
  !endif
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !ifdef CLIPLINE_STANDALONE_WEBVIEW2
    !insertmacro CLIPLINE_GRANT_WEBVIEW2_APPCONTAINER_ACCESS "${CLIPLINE_WEBVIEW2_DIR}"
  !else
    ; A standalone install replaced by the regular installer.
    !insertmacro CLIPLINE_REMOVE_OWN_FOLDER "" "${CLIPLINE_WEBVIEW2_DIR}"
  !endif
  !insertmacro CLIPLINE_REMOVE_OWN_FOLDER "" "${CLIPLINE_LEGACY_WEBVIEW2_DIR}"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ${If} ${Silent}
    Goto clipline_cleanup_done
  ${EndIf}
  ${If} $PassiveMode = 1
    Goto clipline_cleanup_done
  ${EndIf}
  ${If} $UpdateMode = 1
    Goto clipline_cleanup_done
  ${EndIf}

  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /IM ${MAINBINARYNAME}.exe'
  MessageBox MB_YESNO|MB_ICONQUESTION \
    "Also delete saved recordings?$\n$\nOnly recordings created by Clipline will be removed." \
    IDYES clipline_delete_recordings IDNO clipline_keep_recordings

  clipline_delete_recordings:
    nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-cleanup --delete-recordings'
    Goto clipline_cleanup_done

  clipline_keep_recordings:
    nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-cleanup'

  clipline_cleanup_done:
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  RMDir "$INSTDIR\ffmpeg"
  RMDir "$INSTDIR\ffmpeg-staging"
  RMDir "$INSTDIR\cloud-cache"
  RMDir "$INSTDIR\support-staging"
  RMDir "$INSTDIR\EBWebView"
  ; Tauri deletes only the runtime files this release listed.
  !insertmacro CLIPLINE_REMOVE_OWN_FOLDER "un." "${CLIPLINE_WEBVIEW2_DIR}"
  !insertmacro CLIPLINE_REMOVE_OWN_FOLDER "un." "${CLIPLINE_LEGACY_WEBVIEW2_DIR}"
  RMDir "$INSTDIR"
!macroend
