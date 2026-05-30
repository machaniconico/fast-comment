; Tauri NSIS インストーラフック
;
; Tauri 2 の NSIS インストーラは、デフォルトでは完了(Finish)ページの任意ボタンを
; 押したときにしかデスクトップショートカットを作らない。ここで POSTINSTALL フックを
; 使い、通常インストール時も必ずデスクトップにショートカットを作成する。
;
; ${PRODUCTNAME} / ${MAINBINARYNAME} / $INSTDIR / $DESKTOP は Tauri の NSIS
; テンプレートが提供する変数。アンインストール時は標準テンプレートが
; "$DESKTOP\${PRODUCTNAME}.lnk" を削除するため、削除フックは不要。
!macro NSIS_HOOK_POSTINSTALL
  CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
!macroend
