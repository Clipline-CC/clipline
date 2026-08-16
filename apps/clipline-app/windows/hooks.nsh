!macro NSIS_HOOK_PREINSTALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
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
  RMDir /r "$INSTDIR\ffmpeg"
  RMDir /r "$INSTDIR\ffmpeg-staging"
  RMDir /r "$INSTDIR\cloud-cache"
  RMDir /r "$INSTDIR\support-staging"
  RMDir /r "$INSTDIR\EBWebView"
!macroend
