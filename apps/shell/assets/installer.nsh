# NSIS 升级前保留旧版自定义唤醒词（buildResources/installer.nsh，electron-builder 自动引用）
#
# 原位升级流程：installSection 会先执行旧版卸载器（uninstallOldVersion），
# 它把整个旧 $INSTDIR（含 resources/models/kws-keywords.txt）原子移入临时目录后删除，
# 新文件才开始落盘。运行时的迁移逻辑因此可能永远看不到旧自定义关键词。
#
# customInit 在 .onInit 中、install 段（卸载旧版）之前执行；此时 initMultiUser
# 已把 $INSTDIR 指到旧安装目录。这里只把"旧版关键词文件"备份到用户数据
# 目录的旁路文件（kws-keywords.pre-upgrade.txt），由 packages/voice/keywords.mjs
# 在首启时消费一次：旧默认词会被识别并丢弃，自定义词被迁为生效配置。
#
# 不做的事：不保留旧安装目录任何文件、不改卸载/删除流程——避免新版本
# 继续从旧目录读到过期默认词。备份只进用户数据目录，走"新位置有效配置优先"
# 的既有规则；数据目录已有有效配置时不覆盖（用户显式改过就不需要备份）。

!macro vaBackupWakeWord ROOT
  ${if} ${FileExists} "$INSTDIR\resources\models\kws-keywords.txt"
    ${ifNot} ${FileExists} "$APPDATA\${ROOT}\data\kws-keywords.txt"
      CreateDirectory "$APPDATA\${ROOT}\data"
      CopyFiles /SILENT "$INSTDIR\resources\models\kws-keywords.txt" "$APPDATA\${ROOT}\data\kws-keywords.pre-upgrade.txt"
      DetailPrint "Wake-word pre-upgrade backup written to $APPDATA\${ROOT}\data"
    ${endIf}
  ${endIf}
!macroend

!macro customInit
  !insertmacro vaBackupWakeWord "${APP_FILENAME}"
  !ifdef APP_PRODUCT_FILENAME
    !insertmacro vaBackupWakeWord "${APP_PRODUCT_FILENAME}"
  !endif
!macroend
