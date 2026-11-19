; Installer hooks for the standalone variant, which bundles a Fixed Version
; WebView2 runtime in $INSTDIR\wv2. tauri.standalone.conf.json selects this
; file; the regular (Evergreen) installer uses hooks.nsh directly.
!define CLIPLINE_STANDALONE_WEBVIEW2
!include "${__FILEDIR__}\hooks.nsh"
