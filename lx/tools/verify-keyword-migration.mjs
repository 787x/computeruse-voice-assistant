/**
 * 关键词解析/迁移回归（packages/voice/keywords.mjs）。
 *
 * 覆盖 PR #2 评审修复后的语义：
 *   1. 全新安装：生成只含「小爱同学」的默认文件；
 *   2. 旧默认升级：**逐行等于已知旧默认内容**才迁移；
 *   3. 同标签但改过音素/顺序/行数的文件 = 用户自定义，绝不迁移；
 *   4. 新位置有效配置永远优先：旧残留不得再次覆盖（二次启动、手动恢复默认）；
 *   5. 从不原地改写旧文件；
 *   6. 迁移写入失败/数据目录写不进 → 只读兜底 + warning，不丢配置；
 *   7. 数据文件存在但读不动 → 明确报错，绝不静默覆盖；
 *   8. 显式指定不读写默认位置；
 *   9. 打包布局冒烟；
 *  10. 签名（keywordFileSignature）可区分"改过/没改过"。
 *
 * 用法：node tools/verify-keyword-migration.mjs
 */
import assert from 'node:assert/strict'
import {
  chmodSync,
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
  LEGACY_DEFAULT_LINES,
  isLegacyDefaultLines,
  keywordFileSignature,
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

const LEGACY_DEFAULT_TEXT = `${LEGACY_DEFAULT_LINES.join('\n')}\n`
/** 两行历史形态（开发机本机版，签名 2）。 */
const LEGACY_TWO_LINE_TEXT = 'n ǐ h ǎo l áng b ǎo @你好狼宝\nn ǐ h ǎo n áng b ǎo @你好狼宝\n'
/**
 * 真实历史安装包里的旧默认文件（v0.1.0/v0.2.0 setup.exe 解包后逐字节一致，
 * 38 字节、LF 结尾的单行）。回归必须用这组字节，而不是测试自己构造的形态。
 */
const VERIFIED_RELEASE_BYTES = new Uint8Array([
  0x6e, 0x20, 0xc7, 0x90, 0x20, 0x68, 0x20, 0xc7, 0x8e, 0x6f, 0x20, 0x6c, 0x20, 0xc3, 0xa1, 0x6e,
  0x67, 0x20, 0x62, 0x20, 0xc7, 0x8e, 0x6f, 0x20, 0x40, 0xe4, 0xbd, 0xa0, 0xe5, 0xa5, 0xbd, 0xe7,
  0x8b, 0xbc, 0xe5, 0xae, 0x9d, 0x0a,
])

function scenario() {
  const dir = mkdtempSync(join(tmpdir(), 'cua-va-kwtest-'))
  const dataFile = join(dir, 'data', 'kws-keywords.txt')
  const legacyFile = join(dir, 'models', 'kws-keywords.txt')
  const bundledFile = join(dir, 'bundled.txt')
  writeFileSync(bundledFile, `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
  return {
    dir,
    dataFile,
    legacyFile,
    upgradeBackupFile: join(dir, 'data', 'kws-keywords.pre-upgrade.txt'),
    resolve: (extra = {}) => resolveKeywordsFile({ dataFile, legacyFile, bundledFile, upgradeBackupFile: join(dir, 'data', 'kws-keywords.pre-upgrade.txt'), ...extra }),
    writeBackup: (text) => { mkdirSync(join(dir, 'data'), { recursive: true }); writeFileSync(join(dir, 'data', 'kws-keywords.pre-upgrade.txt'), text, 'utf8') },
    writeData: (text) => { mkdirSync(join(dir, 'data'), { recursive: true }); writeFileSync(dataFile, text, 'utf8') },
    writeLegacy: (text) => { mkdirSync(join(dir, 'models'), { recursive: true }); writeFileSync(legacyFile, text, 'utf8') },
    writeLegacyBytes: (bytes) => { mkdirSync(join(dir, 'models'), { recursive: true }); writeFileSync(legacyFile, Buffer.from(bytes)) },
    cleanup: () => { try { chmodSync(join(dir, 'models'), 0o755) } catch { /* */ } rmSync(dir, { recursive: true, force: true }) },
  }
}

// 1) 全新安装
{
  const s = scenario()
  check('全新安装生成「小爱同学」默认文件', () => {
    const info = s.resolve()
    assert.ok(info.file.endsWith(join('data', 'kws-keywords.txt')), `应落在数据目录: ${info.file}`)
    assert.equal(info.source, 'default')
    assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    assert.equal(info.migrated, false)
    assert.equal(readFileSync(info.file, 'utf8').trim(), DEFAULT_KEYWORD_LINE)
  })
  s.cleanup()
}

// 2) 旧默认升级：迁移到新位置；旧文件保持原样（不再原地覆盖）
{
  const s = scenario()
  // 用真实发布安装包里的字节（不是测试自己构造的形态）
  s.writeLegacyBytes(VERIFIED_RELEASE_BYTES)
  check('真实发布版旧默认（单行 38 字节）迁移为「小爱同学」', () => {
    const info = s.resolve()
    assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    assert.equal(info.migrated, true)
    assert.equal(info.file, s.dataFile)
  })
  check('迁移保留旧文件原样', () => {
    assert.deepEqual([...readFileSync(s.legacyFile)], [...VERIFIED_RELEASE_BYTES])
  })
  check('两行历史形态同样识别为旧默认', () => {
    const t = scenario()
    try {
      t.writeLegacy(LEGACY_DEFAULT_TEXT)
      const info = t.resolve()
      assert.equal(info.migrated, true)
      assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    } finally {
      t.cleanup()
    }
  })
  check('迁移完成后，残留旧默认文件不得再次覆盖新配置', () => {
    s.writeData(DEFAULT_KEYWORD_LINE + '\n') // 模拟用户手动恢复默认词后重启
    const again = s.resolve()
    assert.deepEqual(again.labels, [DEFAULT_WAKE_WORD])
    assert.equal(again.migrated, false)
    assert.equal(again.file, s.dataFile)
  })
  s.cleanup()
}

// 3) 同标签但自定义：改音素 / 只留 n 变体 / 调换顺序 / 加自造词，都不算旧默认。
//    注意："只留 l 变体单行"恰恰是真实发布版的旧默认（见场景 2），不能列在这里。
{
  const cases = [
    ['同标签、改音素（声调不同）', ['n ǐ h ǎo l āng b ǎo @你好狼宝']],
    ['同标签、只留 n 变体', ['n ǐ h ǎo n áng b ǎo @你好狼宝']],
    ['两变体但调换顺序', ['n ǐ h ǎo n áng b ǎo @你好狼宝', 'n ǐ h ǎo l áng b ǎo @你好狼宝']],
    ['旧默认 + 用户加词', ['n ǐ h ǎo l áng b ǎo @你好狼宝', 'f ǎ g uó @法国']],
    ['用户把两个旧变体都改了音素', ['n ǐ h ǎo l áŋ b ǎo @你好狼宝', 'n ǐ h ǎo n áŋ b ǎo @你好狼宝']],
  ]
  for (const [name, lines] of cases) {
    const s = scenario()
    s.writeLegacy(lines.join('\n') + '\n')
    check(`旧位置「${name}」按用户自定义保留，不迁移成新默认`, () => {
      assert.equal(isLegacyDefaultLines(lines), false, `${name} 不该被判定为旧默认`)
      const info = s.resolve()
      assert.equal(info.custom, true)
      assert.deepEqual(info.labels, [...new Set(lines.map((l) => l.slice(l.lastIndexOf('@') + 1)))])
      assert.equal(readFileSync(s.legacyFile, 'utf8'), lines.join('\n') + '\n', '旧文件原样保留')
    })
    s.cleanup()
  }
}

// 4) 新位置有效配置优先（数据目录自定义 + 旧位置残留默认/自定义）
{
  const s = scenario()
  s.writeData('j iǎng y ǒu b ó @蒋友伯\n')
  s.writeLegacy(LEGACY_DEFAULT_TEXT)
  check('数据目录自定义优先于旧残留', () => {
    const info = s.resolve()
    assert.equal(info.custom, true)
    assert.deepEqual(info.labels, ['蒋友伯'])
    assert.equal(readFileSync(s.dataFile, 'utf8').trim(), 'j iǎng y ǒu b ó @蒋友伯')
  })
  s.cleanup()
}
{
  const s = scenario()
  s.writeData(DEFAULT_KEYWORD_LINE + '\n')
  s.writeLegacy('zh ōu w àng j ūn @周望军\n')
  check('数据目录默认词优先（旧位置自定义不再夺权）', () => {
    const info = s.resolve()
    assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    assert.equal(info.custom, false)
  })
  s.cleanup()
}
{
  const s = scenario()
  s.writeLegacy('zh ōu w àng j ūn @周望军\n')
  check('旧位置自定义迁移到数据目录，二次启动不重复迁移', () => {
    const info = s.resolve()
    assert.equal(info.source, 'legacy-custom')
    assert.equal(info.migrated, true)
    assert.deepEqual(info.labels, ['周望军'])
    assert.equal(readFileSync(s.legacyFile, 'utf8').trim(), 'zh ōu w àng j ūn @周望军')
    const again = s.resolve()
    assert.equal(again.source, 'data')
    assert.equal(again.migrated, false)
    assert.deepEqual(again.labels, ['周望军'])
  })
  s.cleanup()
}

// 5) 数据目录写不进（Windows 上 chmod 目录挡不住建文件，用"路径被文件占用"确定性模拟）：
//    旧自定义 → 只读使用 + warning；旧默认 → 只读内置默认，绝不使用旧词
{
  const s = scenario()
  s.writeLegacy('zh ōu w àng j ūn @周望军\n')
  writeFileSync(join(s.dir, 'data'), 'blocker', 'utf8') // 用普通文件占住"数据目录"路径
  check('数据目录不可写：自定义迁移退化为只读旧文件（配置不丢）', () => {
    const info = s.resolve()
    assert.deepEqual(info.labels, ['周望军'])
    assert.equal(info.file, s.legacyFile)
    assert.ok(info.warning !== '', '必须带 warning')
  })
  s.cleanup()
}
{
  const s = scenario()
  s.writeLegacy(LEGACY_DEFAULT_TEXT)
  writeFileSync(join(s.dir, 'data'), 'blocker', 'utf8')
  check('数据目录不可写 + 旧默认：退化为只读内置默认（新词，绝不回落旧默认）', () => {
    const info = s.resolve()
    assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    assert.equal(info.file, join(s.dir, 'bundled.txt'))
    assert.ok(info.warning !== '')
  })
  s.cleanup()
}

// 6) 数据文件读不动 → 明确报错，不静默覆盖
{
  const s = scenario()
  mkdirSync(s.dataFile, { recursive: true }) // 目录冒充文件 → 读取抛 EISDIR/EPERM
  check('数据位置损坏（目录冒充文件）：明确报错而非静默重写', () => {
    assert.throws(() => s.resolve(), /无法读取|中止/)
    assert.equal(existsSync(join(s.dataFile, 'kws-keywords.txt')), false)
  })
  s.cleanup()
}
{
  const s = scenario()
  s.writeData('   \n\n') // 空文件 = 无效配置
  s.writeLegacy(LEGACY_DEFAULT_TEXT)
  check('空的数据文件视为无效，重新生成默认', () => {
    const info = s.resolve()
    assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    assert.equal(readFileSync(s.dataFile, 'utf8').trim(), DEFAULT_KEYWORD_LINE)
  })
  s.cleanup()
}

// 7) 显式指定
{
  const s = scenario()
  const explicit = join(s.dir, 'explicit.txt')
  writeFileSync(explicit, `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
  check('显式 keywordsFile 直接使用', () => {
    const info = s.resolve({ keywordsFile: explicit })
    assert.equal(info.file, explicit)
    assert.equal(info.source, 'explicit')
    assert.equal(existsSync(s.dataFile), false)
  })
  check('显式文件不存在时报错', () => {
    assert.throws(() => s.resolve({ keywordsFile: join(s.dir, 'missing.txt') }), /关键词文件不存在/)
  })
  s.cleanup()
}

// 8) 内置默认文件与官方音素一致
{
  check('仓库内置默认文件与官方音素一致', () => {
    const file = new URL('../packages/voice/kws-keywords.default.txt', import.meta.url)
    const text = readFileSync(file, 'utf8').replace(/\r\n?/g, '\n').trim()
    assert.equal(text, DEFAULT_KEYWORD_LINE)
  })
}

// 9) 打包布局冒烟：CUA_VA_ROOT=resources 根 + 全新 userData + 旧遗留默认
{
  const s = scenario()
  const prevRoot = process.env.CUA_VA_ROOT
  const prevData = process.env.CUA_VA_DATA_DIR
  try {
    const resources = join(s.dir, 'resources')
    mkdirSync(join(resources, 'models'), { recursive: true })
    writeFileSync(join(resources, 'kws-keywords.default.txt'), `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
    writeFileSync(join(resources, 'models', 'kws-keywords.txt'), LEGACY_DEFAULT_TEXT, 'utf8')
    process.env.CUA_VA_ROOT = resources
    process.env.CUA_VA_DATA_DIR = join(s.dir, 'userData', 'data')
    const { resolveKeywordsFile: resolveDefault } = await import('../packages/voice/keywords.mjs')
    check('打包根 + 旧遗留默认：首启生成新默认词', () => {
      const info = resolveDefault()
      assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
      assert.ok(info.file.endsWith(join('data', 'kws-keywords.txt')), info.file)
      assert.equal(info.migrated, true)
    })
    check('二次解析幂等、不重复迁移', () => {
      const again = resolveDefault()
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

// 10) 签名：关键词修改前后必须不同（重新开麦重载的判据）
{
  const s = scenario()
  check('keywordFileSignature 能区分修改前后', () => {
    const before = s.resolve().signature
    s.writeData('j iǎng y ǒu b ó @蒋友伯\n')
    const after = keywordFileSignature(s.dataFile)
    assert.notEqual(before, after)
    assert.equal(after, keywordFileSignature(s.dataFile))
    assert.equal(keywordFileSignature(join(s.dir, 'nope.txt')), 'unreadable')
  })
  s.cleanup()
}

// 11) NSIS 升级前备份（installer.nsh customInit 写入 → 运行期消费一次）
{
  const s = scenario()
  s.writeBackup('zh ōu w àng j ūn @周望军\n')
  check('升级备份里的自定义词被消费为生效配置，备份删除', () => {
    const info = s.resolve()
    assert.equal(info.source, 'upgrade-backup')
    assert.deepEqual(info.labels, ['周望军'])
    assert.equal(info.custom, true)
    assert.equal(existsSync(s.upgradeBackupFile), false, '备份必须一次性消费')
    assert.equal(readFileSync(s.dataFile, 'utf8').trim(), 'zh ōu w àng j ūn @周望军')
  })
  check('备份消费后二次启动不再重复迁移', () => {
    const again = s.resolve()
    assert.equal(again.source, 'data')
    assert.equal(again.migrated, false)
    assert.deepEqual(again.labels, ['周望军'])
  })
  s.cleanup()
}
{
  const s = scenario()
  s.writeBackup(LEGACY_DEFAULT_TEXT) // 旧版默认词也被备份了
  check('升级备份是旧默认词：丢弃并生成新默认，不留残留', () => {
    const info = s.resolve()
    assert.deepEqual(info.labels, [DEFAULT_WAKE_WORD])
    assert.equal(existsSync(s.upgradeBackupFile), false)
  })
  s.cleanup()
}
{
  const s = scenario()
  s.writeData('j iǎng y ǒu b ó @蒋友伯\n') // 数据目录已有有效配置
  s.writeBackup(LEGACY_DEFAULT_TEXT)       // 上次中断升级留下的备份
  check('数据目录有效配置优先于升级备份（备份不夺权、不误删数据配置）', () => {
    const info = s.resolve()
    assert.equal(info.source, 'data')
    assert.deepEqual(info.labels, ['蒋友伯'])
    assert.equal(readFileSync(s.dataFile, 'utf8').trim(), 'j iǎng y ǒu b ó @蒋友伯')
  })
  s.cleanup()
}

const failed = results.filter((r) => !r.pass)
console.log('='.repeat(62))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
process.exitCode = failed.length > 0 ? 1 : 0
