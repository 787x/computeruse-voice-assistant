Unicode true
!include "MUI2.nsh"
Name "CUAVA Rust"
Icon "${__FILEDIR__}\..\assets\cuava.ico"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\CUAVA-Rust"
RequestExecutionLevel user
!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"
Section "CUAVA Rust"
SetShellVarContext current
SetOutPath "$INSTDIR"
# In-use files raise the normal installer retry dialog; no process killing.
File /r "${STAGE}\*"
CreateDirectory "$SMPROGRAMS\CUAVA Rust"
CreateShortcut "$SMPROGRAMS\CUAVA Rust\CUAVA.lnk" "$INSTDIR\cuava.exe"
CreateShortcut "$DESKTOP\CUAVA Rust.lnk" "$INSTDIR\cuava.exe"
WriteUninstaller "$INSTDIR\Uninstall.exe"
WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CUAVA-Rust" "DisplayName" "CUAVA Rust (unsigned CPU candidate)"
WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CUAVA-Rust" "DisplayVersion" "0.1.0"
WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CUAVA-Rust" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CUAVA-Rust" "InstallLocation" "$INSTDIR"
WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CUAVA-Rust" "DisplayIcon" "$INSTDIR\cuava.exe"
SectionEnd
Section "Uninstall"
SetShellVarContext current
DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "CUAVA-Rust"
DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CUAVA-Rust"
Delete "$DESKTOP\CUAVA Rust.lnk"
Delete "$SMPROGRAMS\CUAVA Rust\CUAVA.lnk"
RMDir "$SMPROGRAMS\CUAVA Rust"
# Only installation-owned files. Never recursively delete user data/models.
Delete "$INSTDIR\cuava.exe"
Delete "$INSTDIR\cua.ps1"
Delete "$INSTDIR\import-legacy.ps1"
Delete "$INSTDIR\README.md"
Delete "$INSTDIR\msvcp140.dll"
Delete "$INSTDIR\msvcp140_1.dll"
Delete "$INSTDIR\msvcp140_2.dll"
Delete "$INSTDIR\msvcp140_atomic_wait.dll"
Delete "$INSTDIR\msvcp140_codecvt_ids.dll"
Delete "$INSTDIR\vcruntime140.dll"
Delete "$INSTDIR\vcruntime140_1.dll"
Delete "$INSTDIR\vcruntime140_threads.dll"
Delete "$INSTDIR\vccorlib140.dll"
Delete "$INSTDIR\concrt140.dll"
RMDir /r "$INSTDIR\licenses"
Delete "$INSTDIR\Uninstall.exe"
RMDir "$INSTDIR"
SectionEnd
