!ifndef CLIPLINE_VERSION
  !error "CLIPLINE_VERSION is required"
!endif
!ifndef CLIPLINE_VERSION_NUMERIC
  !error "CLIPLINE_VERSION_NUMERIC is required"
!endif
!ifndef CLIPLINE_VARIANT
  !error "CLIPLINE_VARIANT is required"
!endif
!ifndef CLIPLINE_STAGE_DIR
  !error "CLIPLINE_STAGE_DIR is required"
!endif
!ifndef CLIPLINE_OUTPUT_FILE
  !error "CLIPLINE_OUTPUT_FILE is required"
!endif
!ifndef CLIPLINE_ICON_PATH
  !error "CLIPLINE_ICON_PATH is required"
!endif
!ifndef CLIPLINE_ESTIMATED_SIZE_KIB
  !error "CLIPLINE_ESTIMATED_SIZE_KIB is required"
!endif

!define CLIPLINE_PRODUCT_NAME "Clipline"
!define CLIPLINE_PUBLISHER "Clipline"
!define CLIPLINE_PRODUCT_IDENTITY "io.clipline.app"
!define CLIPLINE_INTERNAL_BINARY "Clipline-Slint-Internal-Candidate.exe"
!define CLIPLINE_PACKAGE_FENCE_NAME "Local\io.clipline.app.slint-candidate.package-fence"
!define CLIPLINE_PACKAGE_FENCE_WAIT_MS 30000

!if "${CLIPLINE_VARIANT}" == "regular"
  !define CLIPLINE_VARIANT_ID "regular"
  !define CLIPLINE_VARIANT_NAME "Regular"
!else if "${CLIPLINE_VARIANT}" == "standalone"
  !define CLIPLINE_VARIANT_ID "standalone"
  !define CLIPLINE_VARIANT_NAME "Standalone"
!else
  !error "CLIPLINE_VARIANT must be regular or standalone"
!endif

!define CLIPLINE_INSTALLED_BINARY "Clipline-Slint-Internal-Candidate.exe"
!define CLIPLINE_CANDIDATE_NAME "Clipline Slint Candidate (${CLIPLINE_VARIANT_NAME})"
!define CLIPLINE_INSTALL_DIRECTORY "$LOCALAPPDATA\Programs\Clipline Slint Candidate\${CLIPLINE_VARIANT_ID}"
!define CLIPLINE_UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${CLIPLINE_PRODUCT_IDENTITY}.slint-candidate.${CLIPLINE_VARIANT_ID}"
!define CLIPLINE_APP_PATH_KEY "Software\Microsoft\Windows\CurrentVersion\App Paths\Clipline-Slint-Internal-Candidate-${CLIPLINE_VARIANT_ID}.exe"
!define CLIPLINE_CANDIDATE_STATE_KEY "Software\Clipline\SlintCandidate"

; CliplineHasExactFlag scans complete, quote-aware parameter tokens. Prefixes
; such as /Rjunk and /REINSTALL never shadow a later exact /R.
!macro CLIPLINE_READ_EXACT_FLAG OPTION OUTPUT
  Push "${OPTION}"
  Call CliplineHasExactFlag
  Pop ${OUTPUT}
!macroend
