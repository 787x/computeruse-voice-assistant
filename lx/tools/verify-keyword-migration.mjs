/**
 * 关键词解析/迁移单元测试（packages/voice/keywords.mjs）。
 *
 * 覆盖升级场景的四种关键行为：
 *   1. 全新安装：生成只含「小爱同学」的默认文件；
 *   2. 旧安装升级：默认「你好狼宝」文件被迁移为新默认词；
 *   3. 用户自定义：绝不覆盖，原样保留并迁移到用户数据目录；
 *   4. 显式指定：直接使用指定文件，不读写默认位置。
 *
 * 用法：node tools/verify-keyword-migration.mjs
 */
import assert from 'node:assert/strict'
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import {
  DEFAULT_KEYWORD_LINE,
  DEFAULT_WAKE_WORD,
  resolveKeywordsFile,
} from '../packages/voice/keywords.mjs'

const results = []
function check(name, fn) {
  try {
    fn()
    results.push({ name, pass: true })
    console.log(`✓ ${name}`)
  } catch (error) {
    results.push({ name, pass: false, error: error.message })
    console.log(`✗ ${name}\n  ${error.message}`)
  }
}

function scenario() {
  const dir = mkdtempSync(join(tmpdir(), 'cua-va-kwtest-'))
  const dataFile = join(dir, 'data', 'kws-keywords.txt')
  const legacyFile = join(dir, 'models', 'kws-keywords.txt')
  const bundledFile = join(dir, 'bundled.txt')
  writeFileSync(bundledFile, `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
  return {
    dir,
    resolve: (extra = {}) => resolveKeywordsFile({ dataFile, legacyFile, bundledFile, ...extra }),
    cleanup: () => rmSync(dir, { recursive: true, force: true }),
  }
}

// 1) 全新安装：无数据文件、无旧文件 → 生成新默认。
{
  const s = scenario()
  check('全新安装生成「小爱同学」默认文件', () => {
    const info = s.resolve()
    assert.ok(info.file.endsWith(join('data', 'kws-keywords.txt')), `应落在数据目录: ${info.file}`)
    assert.equal(info.source, 'default')
    assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    assert.equal(existsSync(info.file), true)
    assert.equal(readFileSync(info.file, 'utf8').trim(), DEFAULT_KEYWORD_LINE)
  })
  s.cleanup()
}

// 2) 旧安装升级：models 下是旧默认词 → 迁移为新默认词，且不留旧词。
{
  const s = scenario()
  mkdirSync(join(s.dir, 'models'), { recursive: true })
  writeFileSync(join(s.dir, 'models', 'kws-keywords.txt'),
    'n ǐ h ǎo l áng b ǎo @你好狼宝\nn ǐ h ǎo n áng b ǎo @你好狼宝\n', 'utf8')
  check('旧默认「你好狼宝」文件升级为「小爱同学」', () => {
    const info = s.resolve()
    assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    assert.equal(info.migrated, true)
    const written = readFileSync(join(s.dir, 'models', 'kws-keywords.txt'), 'utf8')
    assert.ok(!written.includes('你好狼宝'), '旧默认文件也应被更新')
  })
  s.cleanup()
}

// 3) 用户自定义（数据目录）不被覆盖。
{
  const s = scenario()
  mkdirSync(join(s.dir, 'data'), { recursive: true })
  writeFileSync(join(s.dir, 'data', 'kws-keywords.txt'), 'j iǎng y ǒu b ó @蒋友伯\n', 'utf8')
  check('用户自定义关键词不被覆盖', () => {
    const info = s.resolve()
    assert.equal(info.custom, true)
    assert.deepEqual(info.labels, ['蒋友伯'])
    assert.equal(readFileSync(join(s.dir, 'data', 'kws-keywords.txt'), 'utf8').trim(), 'j iǎng y ǒu b ó @蒋友伯')
  })
  s.cleanup()
}

// 3b) 用户自定义（旧 models 位置、数据目录仍是默认值）被保留并迁移。
{
  const s = scenario()
  mkdirSync(join(s.dir, 'data'), { recursive: true })
  mkdirSync(join(s.dir, 'models'), { recursive: true })
  writeFileSync(join(s.dir, 'data', 'kws-keywords.txt'), `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
  writeFileSync(join(s.dir, 'models', 'kws-keywords.txt'), 'zh ōu w àng j ūn @周望军\n', 'utf8')
  check('旧位置的自定义关键词被迁移保留', () => {
    const info = s.resolve()
    assert.equal(info.custom, true)
    assert.deepEqual(info.labels, ['周望军'])
    assert.ok(info.file.endsWith(join('data', 'kws-keywords.txt')))
  })
  s.cleanup()
}

// 3c) 混合文件（旧词 + 用户新增词）整体视为自定义，绝不改写。
{
  const s = scenario()
  mkdirSync(join(s.dir, 'models'), { recursive: true })
  writeFileSync(join(s.dir, 'models', 'kws-keywords.txt'),
    'n ǐ h ǎo l áng b ǎo @你好狼宝\nf ǎ g uó @法国\n', 'utf8')
  check('含旧词但非纯默认的文件视为自定义并保留', () => {
    const info = s.resolve()
    assert.equal(info.custom, true)
    assert.deepEqual(info.labels, ['你好狼宝', '法国'])
  })
  s.cleanup()
}

// 4) 显式指定文件不读写默认位置。
{
  const s = scenario()
  const explicit = join(s.dir, 'explicit.txt')
  writeFileSync(explicit, `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
  check('显式 keywordsFile 直接使用', () => {
    const info = s.resolve({ keywordsFile: explicit })
    assert.equal(info.file, explicit)
    assert.equal(info.source, 'explicit')
    assert.equal(existsSync(join(s.dir, 'data', 'kws-keywords.txt')), false)
  })
  check('显式文件不存在时报错', () => {
    assert.throws(() => s.resolve({ keywordsFile: join(s.dir, 'missing.txt') }), /关键词文件不存在/)
  })
  s.cleanup()
}

// 5) 内置默认文件的音素必须与官方「小爱同学」条目一致。
{
  check('仓库内置默认文件与官方音素一致', () => {
    const file = new URL('../packages/voice/kws-keywords.default.txt', import.meta.url)
    const text = readFileSync(file, 'utf8').replace(/\r\n?/g, '\n').trim()
    assert.equal(text, DEFAULT_KEYWORD_LINE)
  })
}

// 6) 打包布局冒烟：CUA_VA_ROOT 指向 resources 根，全新 userData，
//    旧安装遗留 models/kws-keywords.txt（旧默认词）。对应 extraResources 布局。
{
  const s = scenario()
  const prevRoot = process.env.CUA_VA_ROOT
  const prevData = process.env.CUA_VA_DATA_DIR
  try {
    const resources = join(s.dir, 'resources')
    mkdirSync(join(resources, 'models'), { recursive: true })
    writeFileSync(join(resources, 'kws-keywords.default.txt'), `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
    writeFileSync(join(resources, 'models', 'kws-keywords.txt'), 'n ǐ h ǎo l áng b ǎo @你好狼宝\n', 'utf8')
    process.env.CUA_VA_ROOT = resources
    process.env.CUA_VA_DATA_DIR = join(s.dir, 'userData', 'data')
    check('打包根 + 旧遗留文件：首启即新默认词', () => {
      const info = resolveKeywordsFile()
      assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
      assert.ok(info.file.endsWith(join('data', 'kws-keywords.txt')), info.file)
      assert.equal(info.migrated, true)
    })
    check('二次解析幂等（不重复迁移）', () => {
      const again = resolveKeywordsFile()
      assert.deepEqual(again.labels, [DEFAULT_WAKE_WORD])
      assert.equal(again.migrated, false)
    })
  } finally {
    if (prevRoot === undefined) delete process.env.CUA_VA_ROOT
    else process.env.CUA_VA_ROOT = prevRoot
    if (prevData === undefined) delete process.env.CUA_VA_DATA_DIR
    else process.env.CUA_VA_DATA_DIR = prevData
  }
  s.cleanup()
}

const failed = results.filter((r) => !r.pass)
console.log('='.repeat(62))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
process.exitCode = failed.length > 0 ? 1 : 0
