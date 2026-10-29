/**
 * 网易云开放平台直连：签自己的请求，绕开 ncm-cli 那个不稳定的播放守护进程。
 *
 * 为什么这么做：ncm-cli 0.1.7 的 PlayerDaemon 在 Windows 上时好时坏
 * （日志「daemon 无响应（3s 超时）」），但它的**读接口**一直正常。
 * 所以我们把它的"合法设备身份"（device JSON + accessToken）抓出来一次，
 * 之后自己签名调 playurl，拿到音频 URL 直接交给 mpv。
 *
 * 身份怎么来的：`.tmp/ncm-spy.cjs` 通过 NODE_OPTIONS=--require 注入 CLI 进程，
 * 记录它发出的请求 URL；CLI 会把 device、accessToken、sign 全拼在 query 里。
 * 这不是抓包解密，只是让它把自己的参数日志化。
 */
import { spawnSync } from 'node:child_process'
import { createPrivateKey, sign as cryptoSign } from 'node:crypto'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

/** 注入到 ncm-cli 进程里的脚本源码：把请求 URL 追加写到文件 */
const SPY_SOURCE = `
const fs = require('node:fs')
const https = require('node:https')
const OUT = process.env.NCM_SPY_OUT
function record(url) {
  try { if (OUT) fs.appendFileSync(OUT, new Date().toISOString() + '\\t' + url + '\\n') } catch {}
}
const original = https.request
https.request = function (...args) {
  try {
    const first = args[0]
    if (typeof first === 'string') record(first)
    else if (first && typeof first === 'object') record('https://' + (first.hostname ?? first.host ?? '') + (first.path ?? ''))
  } catch {}
  return original.apply(this, args)
}
const originalFetch = globalThis.fetch
if (typeof originalFetch === 'function') {
  globalThis.fetch = function (input, init) {
    try { record(typeof input === 'string' ? input : input?.url) } catch {}
    return originalFetch.call(this, input, init)
  }
}
`
const API_HOST = 'https://openncm.music.163.com'
const IDENTITY_FILE = join(process.cwd(), '.config', 'netease-identity.json')
const SPY_FILE = join(process.cwd(), '.config', 'ncm-spy.log')

/** 裸 base64 私钥 → PKCS8 PEM */
export function toPem(raw) {
  const body = String(raw ?? '').trim().replace(/-----[^-]+-----/gu, '').replace(/\s+/gu, '')
  const wrapped = body.match(/.{1,64}/gu)?.join('\n') ?? body
  return `-----BEGIN PRIVATE KEY-----\n${wrapped}\n-----END PRIVATE KEY-----\n`
}

/** 待签名串：剔除 sign/空值，按 key ASCII 升序，k=v 用 & 拼 */
export function signContent(params) {
  return Object.keys(params)
    .filter((key) => key !== 'sign')
    .filter((key) => params[key] !== undefined && params[key] !== null && params[key] !== '')
    .sort()
    .map((key) => `${key}=${params[key]}`)
    .join('&')
}

export function rsaSign(content, privateKeyRaw) {
  return cryptoSign('RSA-SHA256', Buffer.from(content, 'utf8'), createPrivateKey(toPem(privateKeyRaw))).toString('base64')
}

/** 拼参数 + 签名 */
export function signedParams({ appId, privateKey, bizContent, device, accessToken }) {
  const params = {
    appId,
    signType: 'RSA_SHA256',
    timestamp: String(Date.now()),
    device: JSON.stringify(device),
    bizContent: JSON.stringify(bizContent ?? {}),
  }
  if (typeof accessToken === 'string' && accessToken !== '') params.accessToken = accessToken
  params.sign = rsaSign(signContent(params), privateKey)
  return params
}

function toQuery(params) {
  return Object.entries(params)
    .filter(([, value]) => value !== undefined && value !== null && value !== '')
    .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`)
    .join('&')
}

/**
 * 抓一次 CLI 的合法身份（device + accessToken）。
 * 缓存 10 分钟，过期就再抓一次（accessToken 会轮换）。
 */
export function captureIdentity(options = {}) {
  const cached = readCachedIdentity()
  if (cached !== null && Date.now() - cached.at < 10 * 60 * 1000) return cached

  const entry = options.cliEntry ?? join(process.env.APPDATA ?? '', 'npm', 'node_modules', '@music163', 'ncm-cli', 'dist', 'index.js')
  if (!existsSync(entry)) throw new Error('找不到 ncm-cli 入口，无法抓取设备身份')
  /*
   * 注入用的 spy 脚本在**运行时写到临时目录**，不从仓库里读。
   * 因为打包后代码在 asar 里，而 NODE_OPTIONS --require 是另一个 node 进程去读它，
   * 外部进程读不了 asar（跟 cua.ps1 同一个坑）。写临时文件最省事。
   */
  const spy = join(tmpdir(), `ncm-spy-${process.pid}.cjs`)
  writeFileSync(spy, SPY_SOURCE, 'utf8')

  try { writeFileSync(SPY_FILE, '', 'utf8') } catch { /* 目录可能不存在 */ }
  const env = { ...process.env, NODE_OPTIONS: `--require ${spy}`, NCM_SPY_OUT: SPY_FILE }
  for (const key of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy']) delete env[key]
  spawnSync(process.execPath, [entry, 'user', 'info'], { encoding: 'utf8', windowsHide: true, timeout: 30000, env })

  const text = existsSync(SPY_FILE) ? readFileSync(SPY_FILE, 'utf8') : ''
  const urls = text.split('\n').map((line) => line.split('\t')[1]).filter((url) => typeof url === 'string' && url.includes('accessToken='))
  const last = urls.at(-1)
  if (last === undefined) throw new Error('没抓到带 accessToken 的请求')

  const query = new URL(last).searchParams
  const identity = {
    at: Date.now(),
    device: JSON.parse(query.get('device') ?? '{}'),
    accessToken: query.get('accessToken') ?? '',
  }
  try { writeFileSync(IDENTITY_FILE, JSON.stringify(identity), 'utf8') } catch { /* 缓存失败不影响使用 */ }
  return identity
}

function readCachedIdentity() {
  try {
    const parsed = JSON.parse(readFileSync(IDENTITY_FILE, 'utf8'))
    if (typeof parsed?.accessToken === 'string' && parsed.accessToken !== '') return parsed
    return null
  } catch {
    return null
  }
}

/** 调一次开放平台接口 */
export async function callApi(path, bizContent, options = {}) {
  const identity = options.identity ?? captureIdentity(options)
  const params = signedParams({
    appId: options.appId,
    privateKey: options.privateKey,
    bizContent,
    device: identity.device,
    accessToken: identity.accessToken,
  })
  const url = `${API_HOST}${path}?${toQuery(params)}`
  const response = await fetch(url, { signal: AbortSignal.timeout(options.timeoutMs ?? 20000) })
  const text = await response.text()
  let body = null
  try { body = JSON.parse(text) } catch { /* 保留文本 */ }
  return { status: response.status, body, text }
}

/** 取播放地址 */
export async function getPlayUrl(songId, options = {}) {
  const bitrate = Number(options.bitrate ?? 320)
  const result = await callApi('/openapi/music/basic/song/playurl/get/v2', { songId, bitrate }, options)
  const data = result.body?.data ?? null
  return {
    url: data?.url ?? null,
    level: data?.level ?? null,
    br: data?.br ?? 0,
    size: data?.size ?? 0,
    message: result.body?.message ?? null,
    subCode: result.body?.subCode ?? null,
    raw: result,
  }
}
