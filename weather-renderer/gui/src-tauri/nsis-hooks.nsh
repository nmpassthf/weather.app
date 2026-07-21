!macro NSIS_HOOK_PREINSTALL
  IfFileExists "$INSTDIR\weather.app.exe" 0 weather_service_preinstall_done
  nsExec::ExecToLog '"$INSTDIR\weather.app.exe" daemon service remove windows --system --path "$COMMONAPPDATA\Weather" --with-bin'
  Pop $0
weather_service_preinstall_done:
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog '"$INSTDIR\weather.app.exe" daemon service reinstall windows --system --path "$COMMONAPPDATA\Weather"'
  Pop $0
  StrCmp $0 "0" weather_service_postinstall_done
  MessageBox MB_ICONSTOP|MB_OK "Weather Engine service installation failed (exit code $0)."
  Abort
weather_service_postinstall_done:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  IfFileExists "$INSTDIR\weather.app.exe" 0 weather_service_preuninstall_done
  nsExec::ExecToLog '"$INSTDIR\weather.app.exe" daemon service remove windows --system --path "$COMMONAPPDATA\Weather" --with-bin'
  Pop $0
  StrCmp $0 "0" weather_service_preuninstall_done
  MessageBox MB_ICONSTOP|MB_OK "Weather Engine service removal failed (exit code $0)."
  Abort
weather_service_preuninstall_done:
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
