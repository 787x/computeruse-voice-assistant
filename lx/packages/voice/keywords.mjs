/**
 * 唤醒词与关键词文件的解析 / 迁移。
 *
 * 为什么不能继续把关键词文件只放在 models/ 下：
 *   - models/ 在 .gitignore 里，全新 clone / CI 打包时根本不存在；
 *   - 安装目录（resources/models）在部分机器上不可写；
 *   - 旧安装里的 models/kws-keywords.txt 还是「你好狼宝」，升级后不能继续用。
 *
 * 三层位置：
 *   1. 仓库内置默认值：packages/voice/kws-keywords.default.txt（唯一来源，随包分发）；
 *   2. 运行期可写副本：用户数据目录（dataPath）下的 kws-keywords.txt；
 *   3. 兜底只读：打包后的 resources/kws-keywords.default.txt。
 *
 * 迁移原则（PR #2 评审修复）：
 *   A. 只有文件内容与**已知的旧版自动生成默认文件逐行一致**（「你好狼宝」的
 *      两个发音变体）时，才认定为旧默认并迁移为「小爱同学」。
 *      只看 `@标签` 不算证据：用户可能保留标签改了音素、顺序或增删行——
 *      那属于用户自定义，绝不覆盖。
 *   B. 新位置（用户数据目录）只要有有效配置就**以它为准**，旧位置的残留文件
 *      不得再次参与选择——迁移完成后旧文件永远不再覆盖新配置，
 *      用户手动恢复默认词后重启也保持默认词。
 *   C. 从不原地改写旧文件：它是迁移输入，也可能是用户的其他资料。
 *      （旧版"顺手把 models 文件也改掉"会让升级前备份/复制出来的旧默认
 *      文件在新版本里重新生效。）
 *   D. 新位置文件存在但读不动 → 明确抛错，绝不静默覆盖或删除用户配置；
 *      迁移写入失败 → 直接只读使用旧文件并标记 warning，配置不丢。
 */
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { dataPath, modelPath } from '../config/paths.mjs'

export const DEFAULT_WAKE_WORD = '小爱同学'
/** 与 sherpa-onnx 官方 keywords.txt 中的「小爱同学」完全一致。 */
export const DEFAULT_KEYWORD_LINE = 'x iǎo ài t óng x ué @小爱同学'
/** 历史上的默认唤醒词（展示名，只用于文案与验收，不作为迁移判据）。 */
export const LEGACY_WAKE_WORDS = Object.freeze(['你好狼宝'])
/**
 * 旧版本自动生成的默认关键词文件的**完整内容**（l/n 两个发音变体各一行）。
 * 迁移判据是"逐行一致"，不是"标签都是你好狼宝"——
 * 用户改过音素/顺序/增删行的文件必须视为自定义保留。
 */
export const LEGACY_DEFAULT_LINES = Object.freeze([
  'n ǐ h ǎo l áng b ǎo @你好狼宝',
  'n ǐ h ǎo n áng b ǎo @你好狼宝',
])

const BUNDLED_DEFAULT_FILE = join(dirname(fileURLToPath(import.meta.url)), 'kws-keywords.default.txt')

function normalizeNewlines(text) {
  return String(text).replace(/\r\n?/g, '\n')
}

/** 关键词文件不能有空行/注释行；这里顺手去掉首尾空白和空行。 */
export function readKeywordLines(file) {
  return normalizeNewlines(readFileSync(file, 'utf8'))
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line !== '')
}

/** 取每行 `@` 后的展示名；KWS 命中时返回的就是它。 */
export function keywordLabel(line) {
  const at = line.lastIndexOf('@')
  return at === -1 ? '' : line.slice(at + 1).trim()
}

export function readKeywordLabels(file) {
  return [...new Set(readKeywordLines(file).map(keywordLabel).filter((label) => label !== ''))]
}

/** 可靠证据：整份文件逐行等于旧版自动生成的默认内容。 */
export function isLegacyDefaultLines(lines) {
  return lines.length === LEGACY_DEFAULT_LINES.length
    && lines.every((line, i) => line === LEGACY_DEFAULT_LINES[i])
}

/** 当前默认（小爱同学单行）内容。 */
export function isDefaultLines(lines) {
  return lines.length === 1 && lines[0] === DEFAULT_KEYWORD_LINE
}

/**
 * 文件内容签名：大小 + mtime + 内容哈希。
 * 「关闭再开启语音唤醒」用它判断是否需要重建 KWS（改过才重建，没改零开销）。
 */
export function keywordFileSignature(file) {
  try {
    const bytes = readFileSync(file)
    const st = statSync(file)
    return `${bytes.length}:${st.mtimeMs}:${createHash('sha1').update(bytes).digest('hex').slice(0, 16)}`
  } catch {
    return 'unreadable'
  }
}

function writeKeywordFile(file, lines) {
  try {
    mkdirSync(dirname(file), { recursive: true })
    writeFileSync(file, `${lines.join('\n')}\n`, 'utf8')
    return true
  } catch {
    return false
  }
}

/**
 * 打包后默认关键词在 resources/ 根目录（见 electron-builder.config.cjs）。
 * 开发态回落到仓库里的 packages/voice/kws-keywords.default.txt。
 */
export function defaultKeywordsFile() {
  const packaged = process.env.CUA_VA_ROOT
    ? join(process.env.CUA_VA_ROOT, 'kws-keywords.default.txt')
    : ''
  if (packaged !== '' && existsSync(packaged)) return packaged
  return BUNDLED_DEFAULT_FILE
}

function defaultKeywordLines(bundledFile) {
  try {
    const lines = readKeywordLines(bundledFile)
    if (lines.length > 0) return lines
  } catch { /* 打包漏文件时回落到常量，保证仍能唤醒 */ }
  return [DEFAULT_KEYWORD_LINE]
}

function result(file, source, lines, { custom, migrated, fallback = false, warning = '' }) {
  return {
    file,
    source,
    labels: [...new Set(lines.map(keywordLabel).filter((label) => label !== ''))],
    custom,
    migrated,
    fallback,
    warning,
    signature: keywordFileSignature(file),
  }
}

/**
 * 解析当前应该使用的关键词文件，并在需要时把默认值或自定义值落到可写目录。
 *
 * 优先级（PR #2 评审修复后）：
 *   显式指定 > 用户数据目录有效配置（存在即权威，不再看旧位置/备份）
 *   > 旧位置迁移输入（仅当数据目录没有有效配置时，一次性拷贝）
 *   > 安装器升级前备份（installer.nsh 写入，一次性消费）
 *   > 内置默认（生成到数据目录；写不进就用只读兜底）。
 *
 * @param {{
 *   keywordsFile?: string,   // 显式指定（工具/测试），不读写默认位置
 *   dataFile?: string,       // 默认：dataPath('kws-keywords.txt')
 *   legacyFile?: string,     // 默认：modelPath('kws-keywords.txt')
 *   upgradeBackupFile?: string, // 默认：dataPath('kws-keywords.pre-upgrade.txt')
 *   bundledFile?: string,    // 默认：defaultKeywordsFile()
 *   defaultLines?: string[], // 测试注入：默认词内容
 * }} options
 */
export function resolveKeywordsFile(options = {}) {
  const explicit = options.keywordsFile ?? process.env.CUA_VA_KEYWORDS_FILE ?? ''
  if (explicit !== '') {
    if (!existsSync(explicit)) throw new Error(`关键词文件不存在: ${explicit}`)
    const lines = readKeywordLines(explicit)
    return result(explicit, 'explicit', lines, { custom: !isDefaultLines(lines), migrated: false })
  }

  const dataFile = options.dataFile ?? dataPath('kws-keywords.txt')
  const legacyFile = options.legacyFile ?? modelPath('kws-keywords.txt')
  const upgradeBackupFile = options.upgradeBackupFile ?? dataPath('kws-keywords.pre-upgrade.txt')
  const bundledFile = options.bundledFile ?? defaultKeywordsFile()
  const fallbackLines = options.defaultLines ?? defaultKeywordLines(bundledFile)

  /*
   * ── 第一优先级：用户数据目录 ──
   * 存在且有效 → 权威。包括"用户手动恢复成小爱同学默认词"的场景：
   * 旧位置哪怕还留着自定义/旧默认文件，也绝不再次覆盖（迁移只做一次）。
   */
  if (existsSync(dataFile)) {
    let dataLines
    try {
      dataLines = readKeywordLines(dataFile)
    } catch (error) {
      // 读不动 = 状态未知。覆盖或删除都可能丢用户配置，必须明确报错让人处理。
      throw new Error(`关键词文件无法读取，为避免丢失用户配置已中止: ${dataFile}（${error.message}）`)
    }
    if (dataLines.length > 0 && !isLegacyDefaultLines(dataLines)) {
      return result(dataFile, 'data', dataLines, { custom: !isDefaultLines(dataLines), migrated: false })
    }
    if (isLegacyDefaultLines(dataLines)) {
      // 数据目录里是旧版写过的旧默认 → 迁移为新默认（数据目录本就可写；失败即明确报错）
      if (writeKeywordFile(dataFile, fallbackLines)) {
        return result(dataFile, 'data', fallbackLines, { custom: false, migrated: true })
      }
      throw new Error(`旧默认关键词迁移失败（无法写入）: ${dataFile}`)
    }
    // 空文件：不是有效配置，也不会有内容可丢，继续往下重新生成。
  }

  /*
   * ── 第二优先级：旧位置迁移输入（只在数据目录没有有效配置时读取一次）──
   */
  const legacyExists = existsSync(legacyFile)
  let legacyLines = []
  let legacyUnreadable = false
  if (legacyExists) {
    try {
      legacyLines = readKeywordLines(legacyFile)
    } catch {
      legacyUnreadable = true
    }
  }
  const legacyIsCustom = legacyExists && !legacyUnreadable
    && legacyLines.length > 0
    && !isLegacyDefaultLines(legacyLines)
    && !isDefaultLines(legacyLines)

  if (legacyIsCustom) {
    // 用户自定义：拷贝到数据目录；旧文件保持原样。写不进去就直接只读用它——配置不丢。
    if (writeKeywordFile(dataFile, legacyLines)) {
      return result(dataFile, 'legacy-custom', legacyLines, { custom: true, migrated: true })
    }
    return result(legacyFile, 'legacy-custom', legacyLines, {
      custom: true,
      migrated: false,
      warning: `迁移写入失败，本次只读使用旧文件: ${dataFile} 不可写`,
    })
  }

  /*
   * ── 第三优先级：安装器升级前备份（NSIS customInit 写入）──
   * 原位升级时旧 $INSTDIR（含 resources/models/kws-keywords.txt）会在
   * 新文件落盘前被旧卸载器整体删除；apps/shell/assets/installer.nsh 的
   * customInit 在删除前把旧版自定义关键词备份到这里。首启消费后删除，
   * 之后永远不再参与选择（数据目录有效配置优先）。
   */
  if (existsSync(upgradeBackupFile)) {
    let backupLines = []
    try {
      backupLines = readKeywordLines(upgradeBackupFile)
    } catch {
      backupLines = []
    }
    const backupIsCustom = backupLines.length > 0
      && !isLegacyDefaultLines(backupLines)
      && !isDefaultLines(backupLines)
    if (backupIsCustom) {
      if (writeKeywordFile(dataFile, backupLines)) {
        rmSync(upgradeBackupFile, { force: true })
        return result(dataFile, 'upgrade-backup', backupLines, { custom: true, migrated: true })
      }
      return result(upgradeBackupFile, 'upgrade-backup', backupLines, {
        custom: true,
        migrated: false,
        warning: `升级备份写入失败，本次只读使用备份文件: ${dataFile} 不可写`,
      })
    }
    // 备份是旧默认或已是新默认：不含自定义信息，消费掉不留残留。
    rmSync(upgradeBackupFile, { force: true })
  }

  /*
   * ── 生成新默认（全新安装 / 旧默认升级 / 旧文件不可用）──
   * 旧默认文件不删除也不改写：数据目录生效后它不再是任何人的输入。
   */
  const migratedFromLegacy = legacyExists && !legacyUnreadable && isLegacyDefaultLines(legacyLines)
  const warning = legacyUnreadable ? `旧关键词文件无法读取，按全新默认生成: ${legacyFile}` : ''
  if (writeKeywordFile(dataFile, fallbackLines)) {
    return result(dataFile, 'default', fallbackLines, { custom: false, migrated: migratedFromLegacy, warning })
  }
  // 数据目录写不进去（如非常规安装）：绝不回落到"旧默认"内容。
  if (legacyExists && isDefaultLines(legacyLines)) {
    return result(legacyFile, 'legacy-default', legacyLines, { custom: false, migrated: false, warning })
  }
  if (existsSync(bundledFile)) {
    return result(bundledFile, 'bundled', fallbackLines, {
      custom: false, migrated: migratedFromLegacy, fallback: true,
      warning: warning || `数据目录不可写，本次只读使用内置默认: ${dataFile} 不可写`,
    })
  }
  throw new Error(`无法准备关键词文件：数据目录不可写，且内置默认文件不存在（${bundledFile}）`)
}
