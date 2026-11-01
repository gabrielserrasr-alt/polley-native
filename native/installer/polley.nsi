; Instalador do Polley nativo (NSIS). Gere com installer\build.ps1, que compila o app e passa a versão.
;
; - Instala só para o usuário atual (sem pedir administrador), em %LOCALAPPDATA%\Programs\Polley Nativo.
; - Fica separado do Polley v1 (Electron): nomes e pasta diferentes, os dois podem coexistir.
; - A conta salva (%APPDATA%\Polley) é mantida ao desinstalar/atualizar.

Unicode true
SetCompressor /SOLID lzma

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!define APP_NAME "Polley Nativo"
!define APP_EXE "polley.exe"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\PolleyNativo"

Name "${APP_NAME}"
OutFile "..\release\Polley-Nativo-Setup-${VERSION}.exe"
InstallDir "$LOCALAPPDATA\Programs\${APP_NAME}"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
BrandingText "Polley ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=1046 "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=1046 "FileDescription" "Instalador do ${APP_NAME}"
VIAddVersionKey /LANG=1046 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1046 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1046 "LegalCopyright" "Polley"

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!define MUI_ICON "..\polley\assets\polley.ico"
!define MUI_UNICON "..\polley\assets\polley.ico"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "Este assistente vai instalar o ${APP_NAME} ${VERSION}.$\r$\n$\r$\nChat, amigos, servidores, voz e transmissão de tela até 4K — usando pouca memória.$\r$\n$\r$\nClique em Avançar para continuar."
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Abrir o ${APP_NAME} agora"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "PortugueseBR"

; Fecha o Polley nativo se estiver aberto a partir desta pasta (senão o .exe fica travado).
!macro CloseApp
  nsExec::Exec `powershell -NoProfile -NonInteractive -Command "Get-CimInstance Win32_Process -Filter \"Name='polley.exe'\" | Where-Object { $$_.ExecutablePath -like '$INSTDIR\*' } | ForEach-Object { Stop-Process -Id $$_.ProcessId -Force }"`
  Pop $0
  Sleep 500
!macroend

Section "Polley" SecMain
  SetOutPath "$INSTDIR"
  !insertmacro CloseApp
  File "..\target\release\${APP_EXE}"
  WriteUninstaller "$INSTDIR\Desinstalar.exe"

  CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"

  ; Aparece em "Configurações > Aplicativos" para desinstalar.
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Polley"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Desinstalar.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Desinstalar.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ; Tamanho em KB (só para mostrar na lista de aplicativos).
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" 50000

  ; Atualização automática: o app roda este instalador com "/S /relancar" e ele reabre o Polley no fim.
  ${GetParameters} $R0
  ClearErrors
  ${GetOptions} $R0 "/relancar" $R1
  ${IfNot} ${Errors}
    Exec '"$INSTDIR\${APP_EXE}"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  !insertmacro CloseApp
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\Desinstalar.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  Delete "$DESKTOP\${APP_NAME}.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  ; "Iniciar com o Windows"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Polley"
SectionEnd
