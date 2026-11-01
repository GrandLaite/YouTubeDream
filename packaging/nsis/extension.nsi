Unicode true
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!ifndef VERSION
  !define VERSION "2.0.0"
!endif
!ifndef EXTENSION_ID
  !define EXTENSION_ID "kimlopaoiianjkblnjfakfjjilaojkjc"
!endif
!ifndef UPDATE_URL
  !define UPDATE_URL "https://grandlaite.github.io/YouTubeDream/updates.xml"
!endif
!ifndef OUT_FILE
  !define OUT_FILE "lader-extension_${VERSION}-setup.exe"
!endif
!ifndef HOST_FILE
  !define HOST_FILE "..\..\lader-host.exe"
!endif

!define EDGE_EXTENSION_KEY "Software\Microsoft\Edge\Extensions\${EXTENSION_ID}"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\LaderExtension"

Name "Lader — расширение Edge ${VERSION}"
OutFile "${OUT_FILE}"
InstallDir "$PROGRAMFILES\Lader\Extension"
InstallDirRegKey HKLM "${UNINSTALL_KEY}" "InstallLocation"

Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section
  SetRegView 32
  SetOutPath $INSTDIR
  File /oname=$PLUGINSDIR\lader-host.exe "${HOST_FILE}"
  nsExec::ExecToLog '"$PLUGINSDIR\lader-host.exe" --install'
  Pop $0
  StrCmp $0 "0" host_installed
    MessageBox MB_OK|MB_ICONEXCLAMATION "Не удалось зарегистрировать помощника Lader (код $0). Запустите lader-host.exe вручную."
    Abort
  host_installed:
  WriteRegStr HKLM "${EDGE_EXTENSION_KEY}" "update_url" "${UPDATE_URL}"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "Lader — расширение Edge"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "GrandLaite"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$\"$INSTDIR$\""
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section Uninstall
  SetRegView 32
  ReadRegStr $0 HKLM "${EDGE_EXTENSION_KEY}" "update_url"
  StrCmp $0 "${UPDATE_URL}" 0 +2
    DeleteRegKey HKLM "${EDGE_EXTENSION_KEY}"
  DeleteRegKey HKLM "${UNINSTALL_KEY}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  RMDir "$PROGRAMFILES\Lader"
SectionEnd
