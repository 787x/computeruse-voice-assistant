# NSIS 升级前保留旧版自定义唤醒词（buildResources/installer.nsh，electron-builder 自动引用）
#
# 为什么需要备份：原位升级时 installSection 会先执行旧版卸载器
# （uninstallOldVersion → un.atomicRMDir），它把整个旧 $INSTDIR（含
# resources/models/kws-keywords.txt）原子移入临时目录后删除，新文件才开始落盘。
# 运行期的迁移逻辑因此看不到旧自定义关键词——备份必须发生在删除之前。
#
# customInit 在 .onInit 中、install 段之前执行；initMultiUser 已把 $INSTDIR
# 解析为旧安装目录。
#
# 路径实测（不是变量名推测）：
#   - makensis 编译期探针：APP_FILENAME=computeruse-voice-assistant、
#     PRODUCT_FILENAME=语音助手。
#   - 打包 exe 实跑：Electron userData = %APPDATA%\语音助手（assistant.log、
#     Chromium 缓存都在那里），即 main.mjs 的
#     CUA_VA_DATA_DIR = app.getPath('userData')\data。
#   - 备份根用 ${PRODUCT_FILENAME}，与运行时唯一读取的
#     dataPath('kws-keywords.pre-upgrade.txt') 精确对应。
#   - 本包 perMachine=false，shell context 恒为当前用户，与 Electron
#     userData 落点一致（将来若支持 all-users 安装需显式切 current，
#     见函数内注释）。
#
# 失败处理：复制后做**源/备份逐行内容比对**（核心 FileOpen/FileRead）区分成败；
# 备份失败（旧副本马上会被删除）时弹窗 中止/重试/忽略——默认中止：静默/无人值守升级
# 安全退出而不是丢配置；绝不打印误导的成功消息、绝不静默继续。
# 数据目录已有生效配置时按「新位置优先」不备份；正常安装零额外确认。
#
# 共享头实测踩到的约束（改动前先读）：
#   - 本文件在安装器/卸载器两次编译都会被 !include，函数若进卸载器会触发
#     "never referenced" 警告（electron-builder 把警告当错误），整段包
#     !ifndef BUILD_UNINSTALLER。
#   - LogicLib 在本头求值时可能还没被模板引入，需要自己 !include；
#     FileCopy/FileGetSize 不是核心指令（属于 File 插件，本包未带），
#     复制用核心 CopyFiles、成败用 FileOpen/FileRead 内容比对判定。
#   - 跳转标签只能在 Function/Section 体内、不能在 macro 体内，所以逻辑在
#     Function、customInit 只 Call；Var 也不能声明在 macro 内。

!ifndef BUILD_UNINSTALLER

!include LogicLib.nsh

Function vaBackupWakeWord
  ; $APPDATA/$INSTDIR 在此处均已解析（initMultiUser 之后）。本包
  ; electron-builder 配置 perMachine=false，shell context 恒为当前用户，
  ; 与 Electron app.getPath('userData') 的落点一致；若将来支持 all-users
  ; 安装，需要在这里显式切到 current 上下文（$installMode 在本头求值时
  ; 还没声明，引用它会触发 unknown-variable 警告→warning-as-error）。
  Call vaBackupWakeWordInner
FunctionEnd

Function vaBackupWakeWordInner
  ; ── 源文件：不存在或空行 → 没有要保留的东西 ──
  ClearErrors
  FileOpen $R4 "$INSTDIR\resources\models\kws-keywords.txt" r
  IfErrors vaBwNoSource
  FileRead $R4 $R5
  FileClose $R4
  StrCmp $R5 "" vaBwEmpty
  Goto vaBwHasSource
  vaBwNoSource:
    DetailPrint "Wake-word: no old keyword file in install dir, nothing to back up"
    Return
  vaBwEmpty:
    DetailPrint "Wake-word: old keyword file is empty, nothing to preserve"
    Return

  vaBwHasSource:
  ; ── 「新位置优先」：数据目录已有非空生效配置 → 不需要备份 ──
  ClearErrors
  FileOpen $R4 "$APPDATA\${PRODUCT_FILENAME}\data\kws-keywords.txt" r
  IfErrors vaBwNeed
  FileRead $R4 $R6
  FileClose $R4
  StrCmp $R6 "" vaBwNeed
  DetailPrint "Wake-word: live config present in userData, no backup needed"
  Return

  vaBwNeed:
  vaBwAttempt:
    ClearErrors
    CreateDirectory "$APPDATA\${PRODUCT_FILENAME}\data"
    Delete "$APPDATA\${PRODUCT_FILENAME}\data\kws-keywords.pre-upgrade.txt"
    ; 核心指令 CopyFiles（FileCopy/FileGetSize 需要 File 插件，本包没带）。
    ; 成败完全由后面的内容比对决定：没复制成功 → 目标不存在/不一致 → 失败分支。
    CopyFiles /SILENT "$INSTDIR\resources\models\kws-keywords.txt" "$APPDATA\${PRODUCT_FILENAME}\data\kws-keywords.pre-upgrade.txt"
    ClearErrors
    FileOpen $R4 "$INSTDIR\resources\models\kws-keywords.txt" r
    IfErrors vaBwFail
    FileOpen $R3 "$APPDATA\${PRODUCT_FILENAME}\data\kws-keywords.pre-upgrade.txt" r
    IfErrors vaBwFailCloseSrc
    vaBwVloop:
      FileRead $R4 $R5
      FileRead $R3 $R7
      StrCmp $R5 "" vaBwVtail
      StrCmp $R5 $R7 vaBwVloop vaBwFailBoth
      vaBwVtail:
      FileRead $R3 $R8
      StrCmp $R8 "" vaBwOk vaBwFailBoth
    vaBwFailCloseSrc:
      FileClose $R4
      Goto vaBwFail
    vaBwFailBoth:
      FileClose $R3
      FileClose $R4
      Goto vaBwFail
    vaBwOk:
      FileClose $R3
      FileClose $R4
      DetailPrint "Wake-word pre-upgrade backup written to userData (verified: content matches)"
      Return

    vaBwFail:
    ; 备份失败且旧文件即将被卸载器删除：唯一副本，绝不静默继续。
    ; 默认按钮=中止：无人值守/静默升级时安全退出而不是丢配置；
    ; 交互式升级用户可见 重试/忽略 两个出口。
    MessageBox MB_ABORTRETRYIGNORE|MB_ICONEXCLAMATION \
      "升级前无法备份旧唤醒词配置（resources\models\kws-keywords.txt → 用户数据目录），$\n$\n\
       继续升级会删除旧安装目录，其中的自定义唤醒词将无法自动恢复。$\n\
       中止：取消本次升级（旧配置保持原样）$\n\
       重试：再次尝试备份$\n\
       忽略：继续升级；之后手动把旧文件复制到$\n\
       %APPDATA%\${PRODUCT_FILENAME}\data\kws-keywords.txt 可恢复" \
      /SD IDABORT IDRETRY vaBwAttempt IDIGNORE vaBwIgnore
    SetErrorLevel 3
    Quit
    vaBwIgnore:
    DetailPrint "Wake-word backup FAILED and skipped by user choice; old custom keywords will be lost with this upgrade"
FunctionEnd

!macro customInit
  ; 逻辑在 Function 里：NSIS 不允许在 macro 体内写跳转标签（重试循环需要）
  Call vaBackupWakeWord
!macroend

!endif
