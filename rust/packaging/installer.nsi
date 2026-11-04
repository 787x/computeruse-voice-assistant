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
Function .onInit
System::Call 'kernel32::GetCurrentProcessId() i.r1'
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "pid" "$1"
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "stage" "init"
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "target" "$INSTDIR"
FunctionEnd
Function CheckDestination
# The directory page may change INSTDIR after .onInit.
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "target" "$INSTDIR"
IfFileExists "$INSTDIR\cuava.exe" 0 executable_available
System::Call 'kernel32::CreateFileW(w "$INSTDIR\cuava.exe", i 0xC0000000, i 0, p 0, i 3, i 0, p 0) p.r0'
IntCmp $0 -1 executable_busy
System::Call 'kernel32::CloseHandle(p r0)'
Goto executable_available
executable_busy:
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "stage" "blocked-in-use"
SetErrorLevel 2
MessageBox MB_OK|MB_ICONEXCLAMATION "请先通过托盘退出此安装的 CUAVA，再重新安装。不会强制结束进程。" /SD IDOK
Abort
executable_available:
FunctionEnd
Function .onInstSuccess
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "stage" "success"
FunctionEnd
Function .onInstFailed
ReadINIStr $0 "$TEMP\cuava-install-diagnostic.ini" "install" "stage"
StrCmp $0 "blocked-in-use" 0 generic_failure
SetErrorLevel 2
Return
generic_failure:
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "stage" "failed"
SetErrorLevel 1
FunctionEnd
Section "CUAVA Rust"
Call CheckDestination
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "stage" "extract"
SetShellVarContext current
ClearErrors
SetOutPath "$INSTDIR"
IfErrors 0 directory_ready
System::Call 'kernel32::GetLastError() i.r0'
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "stage" "directory-failed"
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "win32-error" "$0"
SetErrorLevel 1
Abort "无法创建安装目录（Windows 错误 $0）：$INSTDIR。请选择当前用户有写入权限的目录。"
directory_ready:
# In-use files raise the normal installer retry dialog; no process killing.
ClearErrors
File /r "${STAGE}\*"
IfErrors 0 files_ready
WriteINIStr "$TEMP\cuava-install-diagnostic.ini" "install" "stage" "extraction-failed"
SetErrorLevel 1
Abort "写入失败，请退出此安装的程序并检查目标目录权限后重试。"
files_ready:
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
ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "CUAVA-Rust"
StrCmp $0 '$\"$INSTDIR\cuava.exe$\" --background' 0 +2
DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "CUAVA-Rust"
ReadRegStr $1 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CUAVA-Rust" "InstallLocation"
StrCmp $1 $INSTDIR 0 skip_metadata
DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CUAVA-Rust"
Delete "$DESKTOP\CUAVA Rust.lnk"
Delete "$SMPROGRAMS\CUAVA Rust\CUAVA.lnk"
RMDir "$SMPROGRAMS\CUAVA Rust"
skip_metadata:
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
