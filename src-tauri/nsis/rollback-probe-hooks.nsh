; Test-only evidence. This file is selected exclusively by the GA probe config.
!macro NSIS_HOOK_PREINSTALL
  Push $R8
  Push $R9
  ; NSIS error flags are sticky. Earlier optional registry lookups can set
  ; them; a successful FileOpen does not clear an unrelated old error.
  ClearErrors
  FileOpen $R8 "$INSTDIR\rollback-probe-preinstall.txt" w
  IfErrors probe_hook_failed
  ReadRegStr $R9 SHCTX "${UNINSTKEY}" "DisplayVersion"
  FileWriteUTF16LE /BOM $R8 "DisplayVersion=$R9$\r$\n"
  ReadRegStr $R9 SHCTX "${UNINSTKEY}" "InstallLocation"
  FileWriteUTF16LE $R8 "InstallLocation=$R9$\r$\n"
  IfFileExists "$INSTDIR\${MAINBINARYNAME}.exe" 0 +3
    FileWriteUTF16LE $R8 "MainExecutable=present$\r$\n"
    Goto +2
    FileWriteUTF16LE $R8 "MainExecutable=absent$\r$\n"
  IfFileExists "$INSTDIR\uninstall.exe" 0 +3
    FileWriteUTF16LE $R8 "Uninstaller=present$\r$\n"
    Goto +2
    FileWriteUTF16LE $R8 "Uninstaller=absent$\r$\n"
  ClearErrors
  FileClose $R8
  IfErrors probe_hook_failed
  Goto probe_hook_done
  probe_hook_failed:
    SetErrorLevel 2
    Abort "Could not write installer probe evidence."
  probe_hook_done:
  Pop $R9
  Pop $R8
!macroend
