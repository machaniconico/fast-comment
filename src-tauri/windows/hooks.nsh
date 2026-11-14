; Keep the existing desktop-shortcut hook.
!include "${__FILEDIR__}\..\nsis-hooks.nsh"

; Preserve templates edited in v0.2.0 before installation replaces them.
; CopyFiles copies subdirectories recursively; an existing backup is left intact.
!macro NSIS_HOOK_PREINSTALL
  IfFileExists "$INSTDIR\templates\." 0 fast_comment_templates_backup_done
  IfFileExists "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2" fast_comment_templates_backup_done 0
  CreateDirectory "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2"
  CopyFiles /SILENT "$INSTDIR\templates\*.*" "$APPDATA\dev.skyfade.fastcomment\templates-backup-v0.2"
fast_comment_templates_backup_done:
!macroend
