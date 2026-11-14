; Keep the existing desktop-shortcut hook.
!include "${__FILEDIR__}\..\nsis-hooks.nsh"

; Tauri includes this file before declaring any pages. Its .onInit restores the
; previous $INSTDIR before pages run. This invisible first page therefore backs
; up before PageLeaveReinstall can launch the old uninstaller (also with /P).
; Do not use MUI_PAGE_CUSTOMFUNCTION_PRE: Tauri defines it for its own pages.
Page custom FastCommentBackupBeforeReinstall

; A user may explicitly continue without a backup. Do not prompt again when
; PREINSTALL follows the early page in the same installer process.
Var FastCommentTemplatesBackupSkipped

Function FastCommentBackupBeforeReinstall
  Call FastCommentBackupTemplates
  Abort ; Skip this custom page without displaying a dialog.
FunctionEnd

; CopyFiles without /FILESONLY includes subdirectories. Publish the backup only
; after the entire copy succeeds; an existing final backup is never modified.
Function FastCommentBackupTemplates
  StrCmp $FastCommentTemplatesBackupSkipped "1" fast_comment_templates_backup_done
  IfFileExists "$INSTDIR\templates\." 0 fast_comment_templates_backup_done
  IfFileExists "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2" fast_comment_templates_backup_done 0

  ; CopyFiles reports an error for an empty source. Ignore the directory's
  ; dot entries, but count files (including hidden ones) and subdirectories.
  Push $0
  Push $1
  ClearErrors
  FindFirst $0 $1 "$INSTDIR\templates\*.*"
  IfErrors fast_comment_templates_backup_no_entries
fast_comment_templates_backup_find:
  StrCmp $1 "." fast_comment_templates_backup_find_next
  StrCmp $1 ".." fast_comment_templates_backup_find_next
  FindClose $0
  Pop $1
  Pop $0
  Goto fast_comment_templates_backup_nonempty
fast_comment_templates_backup_find_next:
  FindNext $0 $1
  IfErrors fast_comment_templates_backup_empty
  Goto fast_comment_templates_backup_find
fast_comment_templates_backup_empty:
  FindClose $0
fast_comment_templates_backup_no_entries:
  Pop $1
  Pop $0
  Goto fast_comment_templates_backup_done

fast_comment_templates_backup_nonempty:
  ; Remove an interrupted copy before retrying. Never merge it into a new copy.
  IfFileExists "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2.tmp" 0 fast_comment_templates_backup_copy
  ClearErrors
  RMDir /r "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2.tmp"
  IfErrors fast_comment_templates_backup_failed

fast_comment_templates_backup_copy:
  ClearErrors
  CreateDirectory "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2.tmp"
  IfErrors fast_comment_templates_backup_failed
  ClearErrors
  CopyFiles /SILENT "$INSTDIR\templates\*.*" "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2.tmp"
  IfErrors fast_comment_templates_backup_failed
  ClearErrors
  Rename "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2.tmp" "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2"
  IfErrors fast_comment_templates_backup_failed
  Goto fast_comment_templates_backup_done

fast_comment_templates_backup_failed:
  RMDir /r "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2.tmp"
  MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 "OBS テンプレートを次の場所へ退避できませんでした。$\r$\n$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2$\r$\n$\r$\n退避せずに更新を続けますか?$\r$\n「はい」を選ぶと、編集したテンプレートが失われる可能性があります。$\r$\n「いいえ」を選ぶと更新を中止します。空き容量・アクセス権を確認して再実行してください。" /SD IDNO IDYES fast_comment_templates_backup_skip
  SetErrorLevel 1
  Quit ; Abort would only skip a page here, allowing the old uninstall to proceed.

fast_comment_templates_backup_skip:
  StrCpy $FastCommentTemplatesBackupSkipped "1"
fast_comment_templates_backup_done:
  ClearErrors
FunctionEnd

; Silent installs skip pages. /UPDATE also needs this fallback before extraction.
!macro NSIS_HOOK_PREINSTALL
  Call FastCommentBackupTemplates
!macroend
