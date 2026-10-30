/**
 * 关键词热重载回归（StreamingPipeline.keywordsChanged / reloadKws）。
 *
 * 场景（PR #2 评审问题 5）：管线是缓存的，用户改了关键词文件后，
 * 「关闭再开启语音唤醒」必须真正加载新配置；没改则不能白重建模型；
 * 新文件加载失败时不能假装生效（旧实例继续服务 + ok:false）。
 *
 * 用法：node tools/verify-keyword-reload.mjs
 */
import assert from 'node:assert/strict'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { StreamingPipeline } from '../packages/voice/stream.mjs'
import { DEFAULT_KEYWORD_LINE } from '../packages/voice/keywords.mjs'

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

const dataDir = mkdtempSync(join(tmpdir(), 'cua-va-reload-'))
process.env.CUA_VA_DATA_DIR = dataDir

try {
  const pipeline = new StreamingPipeline()
  const file = pipeline.keywordInfo.file
  const initialLabels = pipeline.keywordInfo.labels
  const initialSignature = pipeline.keywordSignature

  check('初始加载为默认「小爱同学」', () => {
    assert.deepEqual(initialLabels, ['小爱同学'])
  })

  check('未修改时 keywordsChanged()=false（不白重建模型）', () => {
    assert.equal(pipeline.keywordsChanged(), false)
  })

  check('修改关键词文件后 keywordsChanged()=true', () => {
    writeFileSync(file, 'f ǎ g uó @法国\n', 'utf8')
    assert.equal(pipeline.keywordsChanged(), true)
  })

  check('reloadKws 后加载的是新配置', () => {
    const reload = pipeline.reloadKws()
    assert.equal(reload.ok, true)
    assert.equal(reload.rebuilt, true)
    assert.deepEqual(reload.keywordInfo.labels, ['法国'])
    assert.deepEqual(pipeline.keywordInfo.labels, ['法国'])
    assert.equal(pipeline.keywordsChanged(), false, '重载后签名应重新一致')
  })

  check('重载后恢复默认词，再次重载生效（用户恢复默认场景）', () => {
    writeFileSync(file, `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
    assert.equal(pipeline.keywordsChanged(), true)
    const reload = pipeline.reloadKws()
    assert.equal(reload.ok, true)
    assert.deepEqual(pipeline.keywordInfo.labels, ['小爱同学'])
  })

  check('关键词文件损坏：reloadKws 明确失败且旧实例继续服务', () => {
    // 非法行（无音素、纯注释文本）→ KeywordSpotter 构造抛 invalid stof argument
    writeFileSync(file, '# 这是注释，解析器会炸\n', 'utf8')
    assert.equal(pipeline.keywordsChanged(), true)
    const reload = pipeline.reloadKws()
    assert.equal(reload.ok, false, '必须如实报告失败')
    assert.equal(reload.rebuilt, false)
    assert.ok(typeof reload.error === 'string' && reload.error !== '')
    assert.deepEqual(pipeline.keywordInfo.labels, ['小爱同学'], '旧实例仍在服务（旧配置），不能假装新配置生效')
  })

  check('修坏文件后重载成功（失败不是粘性状态）', () => {
    writeFileSync(file, `${DEFAULT_KEYWORD_LINE}\n`, 'utf8')
    const reload = pipeline.reloadKws()
    assert.equal(reload.ok, true)
    assert.deepEqual(pipeline.keywordInfo.labels, ['小爱同学'])
  })

  check('disableKws 管线：无 KWS 也安全（changed=false, reload no-op）', () => {
    const bare = new StreamingPipeline({ disableKws: true })
    assert.equal(bare.keywordsChanged(), false)
    const reload = bare.reloadKws()
    assert.deepEqual(reload, { ok: true, rebuilt: false, keywordInfo: null })
  })

  // 主进程接线静态检查：关麦再开必须触发重载；失败必须走异常路径（不假装生效）。
  // 本脚本在两套发行版各自有一份，这条检查也随之各自验证自己的 main.mjs。
  check('main.mjs 开麦边界已接 keywordsChanged/reloadKws（fail-loud）', () => {
    const mainFile = new URL('../apps/shell/main.mjs', import.meta.url)
    assert.ok(existsSync(mainFile), '找不到本树的 apps/shell/main.mjs')
    const src = readFileSync(mainFile, 'utf8')
    assert.match(src, /else if \(pipeline\.keywordsChanged\(\)\)/, '缓存管线的开麦路径必须检查关键词变化')
    assert.match(src, /const reload = pipeline\.reloadKws\(\)/, '必须调用 reloadKws')
    assert.match(src, /if \(!reload\.ok\) throw new Error\(/, '重载失败必须抛出（走既有错误路径），不能静默当成成功')
    // 反例：不允许"失败后继续开麦"
    assert.doesNotMatch(src, /reloadKws\(\)[\s\S]{0,200}?if \(!reload\.ok\)[\s\S]{0,80}?return \{ ok: true/, '不能把重载失败吞掉')
  })

  check('HUD 唤醒提示接线：只在加载成功后推 hud:wakeHint', () => {
    const mainSrc = readFileSync(new URL('../apps/shell/main.mjs', import.meta.url), 'utf8')
    const preloadSrc = readFileSync(new URL('../apps/shell/preload.mjs', import.meta.url), 'utf8')
    const appSrc = readFileSync(new URL('../apps/shell/renderer/app.js', import.meta.url), 'utf8')
    // 主进程：pushWakeHint 恰好在"首载成功"与"重载成功"两处被调用（不含定义处）
    const calls = mainSrc.match(/(?<!function )\bpushWakeHint\(\)/g) ?? []
    assert.equal(calls.length, 2, `pushWakeHint 应恰好调用两次（首载+重载成功），实际 ${calls.length}`)
    assert.match(mainSrc, /win\.webContents\.send\('hud:wakeHint', wakeHintLabels\)/)
    // 重载失败路径不得推新提示：throw 在 pushWakeHint 之前
    assert.match(mainSrc, /if \(!reload\.ok\) throw[\s\S]{0,200}?pushWakeHint\(\)/, '失败必须先 throw，成功才推提示')
    // preload 暴露 onWakeHint；renderer 用它渲染 done 文案，且 done 不再硬编码词
    assert.match(preloadSrc, /onWakeHint/)
    assert.match(appSrc, /window\.hud\.onWakeHint/)
    assert.match(appSrc, /喊\$\{wakeWordPhrase\(wakeHintLabels\)\}/)
    assert.doesNotMatch(appSrc, /需要我做什么，喊「[^$]/, 'done 文案不得再硬编码具体唤醒词')
  })
} finally {
  rmSync(dataDir, { recursive: true, force: true })
}

const failed = results.filter((r) => !r.pass)
console.log('='.repeat(62))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
process.exitCode = failed.length > 0 ? 1 : 0
