/**
 * 唤醒词与关键词文件的解析 / 迁移。
 *
 * 为什么不能继续把关键词文件只放在 models/ 下：
 *   - models/ 在 .gitignore 里，全新 clone / CI 打包时根本不存在；
 *   - 安装目录（resources/models）在部分机器上不可写；
 *   - 旧安装里的 models/kws-keywords.txt 还是「你好狼宝」，升级后不能继续用。
 *
 * 因此采用三层：
 *   1. 仓库内置默认值：packages/voice/kws-keywords.default.txt（唯一来源，随包分发）；
 *   2. 运行期可写副本：用户数据目录（dataPath）下的 kws-keywords.txt；
 *   3. 兜底只读：打包后的 resources/kws-keywords.default.txt。
 *
 * 迁移规则只覆盖「旧默认文件」：只有整份文件的关键词标签都是旧唤醒词时才替换。
 * 用户自己写过的任何关键词组合都会原样保留，绝不覆盖。
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { dataPath, modelPath } from '../config/paths.mjs'

export const DEFAULT_WAKE_WORD = '小爱同学'
/** 与 sherpa-onnx 官方 keywords.txt 中的「小爱同学」完全一致。 */
export const DEFAULT_KEYWORD_LINE = 'x iǎo ài t óng x ué @小爱同学'
/** 历史上的默认唤醒词（只用于识别“旧默认文件”，不会加入新关键词）。 */
export const LEGACY_WAKE_WORDS = Object.freeze(['你好狼宝'])

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

function isLegacyDefaultLines(lines) {
  if (lines.length === 0) return false
  const labels = lines.map(keywordLabel)
  return labels.every((label) => label !== '' && LEGACY_WAKE_WORDS.includes(label))
}

function isDefaultLines(lines) {
  return lines.length === 1 && lines[0] === DEFAULT_KEYWORD_LINE
}

function writeKeywordFile(file, lines) {
  try {
    mkdirSync(dirname(file), { recursive: true })
    // 原子性不是硬要求：这个文件只有本进程在写，最坏情况是下次重新生成。
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

function result(file, source, lines, { custom, migrated, fallback = false }) {
  return {
    file,
    source,
    labels: [...new Set(lines.map(keywordLabel).filter((label) => label !== ''))],
    custom,
    migrated,
    fallback,
  }
}

/**
 * 解析当前应该使用的关键词文件，并在需要时把默认值或自定义值落到可写目录。
 *
 * @param {{
 *   keywordsFile?: string,   // 显式指定（工具/测试），不读写默认位置
 *   dataFile?: string,       // 默认：dataPath('kws-keywords.txt')
 *   legacyFile?: string,     // 默认：modelPath('kws-keywords.txt')
 *   bundledFile?: string,    // 默认：defaultKeywordsFile()
 * }} options
 */
export function resolveKeywordsFile(options = {}) {
  const explicit = options.keywordsFile ?? process.env.CUA_VA_KEYWORDS_FILE ?? ''
  if (explicit !== '') {
    if (!existsSync(explicit)) throw new Error(`关键词文件不存在: ${explicit}`)
    return result(explicit, 'explicit', readKeywordLines(explicit), { custom: true, migrated: false })
  }

  const dataFile = options.dataFile ?? dataPath('kws-keywords.txt')
  const legacyFile = options.legacyFile ?? modelPath('kws-keywords.txt')
  const bundledFile = options.bundledFile ?? defaultKeywordsFile()
  const fallbackLines = options.defaultLines ?? defaultKeywordLines(bundledFile)

  let dataExists = existsSync(dataFile)
  const legacyExists = existsSync(legacyFile)
  let dataLines = []
  let legacyLines = []
  try { if (dataExists) dataLines = readKeywordLines(dataFile) } catch { dataLines = [] }
  try { if (legacyExists) legacyLines = readKeywordLines(legacyFile) } catch { legacyLines = [] }

  let migrated = false

  /*
   * 旧安装的 models/kws-keywords.txt 可能是旧默认词。先尽量把它本身也改成新默认，
   * 这样开发者打开文件看到的和实际生效的一致；安装目录不可写就只迁移用户数据副本。
   */
  if (legacyExists && isLegacyDefaultLines(legacyLines)) {
    migrated = writeKeywordFile(legacyFile, fallbackLines) || migrated
  }

  // 用户数据副本是升级后的正式位置：旧默认则替换，自定义则原样保留。
  if (dataExists && isLegacyDefaultLines(dataLines)) {
    if (writeKeywordFile(dataFile, fallbackLines)) {
      return result(dataFile, 'data', fallbackLines, { custom: false, migrated: true })
    }
    // 写不进去也绝不能继续使用旧默认文件。
    dataExists = false
    dataLines = []
  }
  if (dataExists && dataLines.length > 0 && !isDefaultLines(dataLines)) {
    return result(dataFile, 'data', dataLines, { custom: true, migrated })
  }

  /*
   * 数据目录里是自动生成的默认值，但用户仍按旧文档改了 models/ 里的文件：
   * 以用户的显式自定义为准，并迁移到用户数据目录。
   */
  if (dataExists && isDefaultLines(dataLines)
    && legacyExists && legacyLines.length > 0
    && !isLegacyDefaultLines(legacyLines) && !isDefaultLines(legacyLines)) {
    if (writeKeywordFile(dataFile, legacyLines)) {
      return result(dataFile, 'legacy-custom', legacyLines, { custom: true, migrated })
    }
    return result(legacyFile, 'legacy-custom', legacyLines, { custom: true, migrated })
  }
  if (dataExists && isDefaultLines(dataLines)) {
    return result(dataFile, 'data', dataLines, { custom: false, migrated })
  }

  // 没有可用的用户数据文件：旧位置若已自定义，先尊重它。
  if (legacyExists && legacyLines.length > 0
    && !isLegacyDefaultLines(legacyLines) && !isDefaultLines(legacyLines)) {
    if (writeKeywordFile(dataFile, legacyLines)) {
      return result(dataFile, 'legacy-custom', legacyLines, { custom: true, migrated })
    }
    return result(legacyFile, 'legacy-custom', legacyLines, { custom: true, migrated })
  }

  // 全新安装 / 旧默认升级：生成只含“小爱同学”的默认文件。
  if (writeKeywordFile(dataFile, fallbackLines)) {
    return result(dataFile, 'default', fallbackLines, { custom: false, migrated })
  }
  if (legacyExists && isDefaultLines(legacyLines)) {
    return result(legacyFile, 'legacy-default', legacyLines, { custom: false, migrated })
  }
  if (existsSync(bundledFile)) {
    return result(bundledFile, 'bundled', fallbackLines, { custom: false, migrated, fallback: true })
  }
  throw new Error(`无法准备关键词文件：数据目录不可写，且内置默认文件不存在（${bundledFile}）`)
}
