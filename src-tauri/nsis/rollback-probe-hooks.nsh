; Test-only evidence. This file is selected exclusively by the GA probe config.
!macro NSIS_HOOK_PREINSTALL
  Push $R8
  Push $R9
  FileOpen $R8 "$INSTDIR\rollback-probe-preinstall.txt" w
  IfErrors probe_hook_failed
  ReadRegStr $R9 SHCTX "${UNINSTKEY}" "DisplayVersion"
  FileWrite $R8 "DisplayVersion=$R9$\r$\n"
  ReadRegStr $R9 SHCTX "${UNINSTKEY}" "InstallLocation"
  FileWrite $R8 "InstallLocation=$R9$\r$\n"
  IfFileExists "$INSTDIR\${MAINBINARYNAME}.exe" 0 +3
    FileWrite $R8 "MainExecutable=present$\r$\n"
    Goto +2
    FileWrite $R8 "MainExecutable=absent$\r$\n"
  IfFileExists "$INSTDIR\uninstall.exe" 0 +3
    FileWrite $R8 "Uninstaller=present$\r$\n"
    Goto +2
    FileWrite $R8 "Uninstaller=absent$\r$\n"
  FileClose $R8
  Goto probe_hook_done
  probe_hook_failed:
    SetErrorLevel 2
    Abort "Could not write installer probe evidence."
  probe_hook_done:
  Pop $R9
  Pop $R8
!macroend
