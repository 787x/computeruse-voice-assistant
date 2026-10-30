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
// 用字符码拼分隔符：这个模板字符串里的反斜杠会被再转义一次，写 '\\\\n' 落盘就成了字面量，行永远拆不开
const TAB = String.fromCharCode(9)
const NL = String.fromCharCode(10)
function record(url) {
  try { if (OUT) fs.appendFileSync(OUT, new Date().toISOString() + TAB + 'URL' + TAB + url + NL) } catch {}
}
function recordBody(url, body) {
  // 只留带 accessToken 的响应，避免日志膨胀
  if (typeof body !== 'string' || body.indexOf('accessToken') < 0) return
  try { if (OUT) fs.appendFileSync(OUT, new Date().toISOString() + TAB + 'BODY' + TAB + url + TAB + body.slice(0, 6000) + NL) } catch {}
}
const original = https.request
https.request = function (...args) {
  let url = ''
  try {
    const first = args[0]
    if (typeof first === 'string') url = first
    else if (first && typeof first === 'object') url = 'https://' + (first.hostname ?? first.host ?? '') + (first.path ?? '')
    record(url)
  } catch {}
  const request = original.apply(this, args)
  try {
    request.on('response', (response) => {
      let body = ''
      response.on('data', (chunk) => {
        if (body.length < 6000) body += String(chunk)
      })
      response.on('end', () => recordBody(url, body))
    })
  } catch {}
  return request
}
const originalFetch = globalThis.fetch
if (typeof originalFetch === 'function') {
  globalThis.fetch = function (input, init) {
    const url = typeof input === 'string' ? input : input?.url
    try { record(url) } catch {}
    return originalFetch.call(this, input, init).then((response) => {
      try {
        response.clone().text().then((body) => recordBody(url, body)).catch(() => {})
      } catch {}
      return response
    })
  }
}
`
const API_HOST = 'https://openncm.music.163.com'
/*
 * 身份缓存与 spy 日志放在"配置目录"里。
 * 打包后 process.cwd() 是安装目录（Program Files 下，普通权限写不进去），
 * 所以必须跟着应用的数据目录走：主进程会设 CUA_VA_CONFIG_DIR。
 */
function configDir() {
  return process.env.CUA_VA_CONFIG_DIR ?? join(process.cwd(), '.config')
}
function identityFile() {
  return join(configDir(), 'netease-identity.json')
}
function spyFileDefault() {
  return join(configDir(), 'ncm-spy.log')
}

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
  /*
   * 设备身份缓存 6 小时。之前只有 10 分钟，一过就重新抓；抓一次要起 CLI 跑 user info，
   * 冷启动 11 秒（它每次都先拉 manifest），用户体感就是"点首歌半天没反应"。
   * accessToken 失效时 callApi 会自动重抓一次，所以放宽是安全的。
   */
  if (cached !== null && isIdentityFresh(cached)) return cached

  const entry = options.cliEntry ?? join(process.env.APPDATA ?? '', 'npm', 'node_modules', '@music163', 'ncm-cli', 'dist', 'index.js')
  if (!existsSync(entry)) throw new Error('找不到 ncm-cli 入口，无法抓取设备身份')
  /*
   * 注入用的 spy 脚本在**运行时写到临时目录**，不从仓库里读。
   * 因为打包后代码在 asar 里，而 NODE_OPTIONS --require 是另一个 node 进程去读它，
   * 外部进程读不了 asar（跟 cua.ps1 同一个坑）。写临时文件最省事。
   */
  const spy = join(tmpdir(), `ncm-spy-${process.pid}.cjs`)
  writeFileSync(spy, SPY_SOURCE, 'utf8')

  try { writeFileSync(spyFileDefault(), '', 'utf8') } catch { /* 目录可能不存在 */ }
  // ELECTRON_RUN_AS_NODE：打包后 execPath 是助手自己，不加这个就会"又启动一次助手"
  const env = { ...process.env, NODE_OPTIONS: `--require ${spy}`, NCM_SPY_OUT: spyFileDefault(), ELECTRON_RUN_AS_NODE: '1' }
  for (const key of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy']) delete env[key]
  /*
   * 探针：play 是内置命令，且一定会发一条带 accessToken 的请求。
   * （search 是服务端 manifest 才下发的动态命令，未登录时根本没有，不能用。）
   * 抓到身份后立刻停掉播放，别真出声。
   */
  const probeSong = options.probeSong ?? { encryptedId: 'F7B1A29060E1C21101D9EB4973DA1A8F', originalId: '1330348068' }
  spawnSync(process.execPath, [entry, 'play', '--song', '--encrypted-id', probeSong.encryptedId, '--original-id', probeSong.originalId], { encoding: 'utf8', windowsHide: true, timeout: 30000, env })
  try { spawnSync(process.execPath, [entry, 'stop'], { encoding: 'utf8', windowsHide: true, timeout: 10000, env }) } catch { /* 停了就行 */ }

  const identity = readIdentityFromSpyFile(spyFileDefault())
  if (identity === null) throw new Error('没抓到带 accessToken 的请求')
  try { writeFileSync(identityFile(), JSON.stringify(identity), 'utf8') } catch { /* 缓存失败不影响使用 */ }
  return identity
}

function readCachedIdentity() {
  try {
    const parsed = JSON.parse(readFileSync(identityFile(), 'utf8'))
    if (typeof parsed?.accessToken === 'string' && parsed.accessToken !== '') return parsed
    return null
  } catch {
    return null
  }
}

/**
 * 从 spy 日志解析设备身份。两条路：
 *   1. 请求 URL 里带 accessToken（play 探针）；
 *   2. 响应体里带 accessToken（登录换 token 的那次响应）。
 * 登录流程通常只有第 2 种，所以两条都必须认。
 */
export function readIdentityFromSpyFile(spyFile) {
  if (!existsSync(spyFile)) return null
  const lines = readFileSync(spyFile, 'utf8').split('\n').filter(Boolean)
  let device = null
  let accessToken = ''
  let refreshToken = ''
  let expireTime = 0
  for (const line of lines) {
    const parts = line.split('\t')
    const kind = parts[1]
    if (kind === 'URL') {
      const url = parts[2] ?? ''
      if (!url.includes('accessToken=')) continue
      try {
        const query = new URL(url).searchParams
        const token = query.get('accessToken') ?? ''
        if (token !== '') accessToken = token
        const rawDevice = query.get('device')
        if (rawDevice !== null && rawDevice !== '') device = JSON.parse(rawDevice)
      } catch { /* 解析失败就跳过 */ }
    } else if (kind === 'BODY') {
      const body = parts[3] ?? ''
      /*
       * 登录换 token 的响应：
       *   {"data":{"accessToken":{"accessToken":"…","refreshToken":"…","expireTime":86400}}}
       * 要点：后出现的覆盖先出现的（登录流程里先有一次带旧 token 的 profile 请求）；
       * 顺手把 refreshToken 和 expireTime 存下来，将来做自动续期要用。
       */
      const matches = [...body.matchAll(/"accessToken"\s*:\s*"([^"]{8,})"/gu)]
      if (matches.length > 0) accessToken = matches.at(-1)[1]
      const refresh = body.match(/"refreshToken"\s*:\s*"([^"]{8,})"/u)
      if (refresh !== null) refreshToken = refresh[1]
      const expire = body.match(/"expireTime"\s*:\s*(\d+)/u)
      if (expire !== null) expireTime = Number(expire[1])
    }
  }
  if (accessToken === '') return null
  if (device === null) {
    device = {
      deviceType: 'openapi', os: 'ncmcli', appVer: '0.1.7', channel: 'ncmcli',
      model: 'Windows_x64_cli', brand: 'ncmcli', osVer: '10.0.26300',
      clientIp: '', deviceId: 'ncmcli_' + process.pid,
    }
  }
  const now = Date.now()
  return {
    at: now,
    device,
    accessToken,
    // 服务端给的有效期（秒）；登录响应里的 expireTime 实测是 86400 = 24 小时
    refreshToken,
    expireAt: expireTime > 0 ? now + expireTime * 1000 : 0,
  }
}

/**
 * 给别的子进程准备"抓请求"的环境（登录流程用）。
 * 返回 { env, spyFile, read }，read() 会把抓到的 accessToken + device 存成身份缓存。
 */
export function makeSpyEnv(tag = 'login') {
  const spy = join(tmpdir(), `ncm-spy-${tag}-${process.pid}.cjs`)
  writeFileSync(spy, SPY_SOURCE, 'utf8')
  const spyFile = join(configDir(), `ncm-spy-${tag}.log`)
  try { writeFileSync(spyFile, '', 'utf8') } catch { /* 目录可能不存在 */ }
  const env = { ...process.env, NODE_OPTIONS: `--require ${spy}`, NCM_SPY_OUT: spyFile, ELECTRON_RUN_AS_NODE: '1' }
  for (const key of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy']) delete env[key]

  const read = () => {
    const identity = readIdentityFromSpyFile(spyFile)
    if (identity === null) return null
    try { writeFileSync(identityFile(), JSON.stringify(identity), 'utf8') } catch { /* 写不进去也不影响本次 */ }
    return identity
  }

  return { env, spyFile, read }
}

/** 身份还有效吗：优先按服务端给的 expireAt 判断，没有就按 24 小时兜底 */
function isIdentityFresh(identity) {
  if (identity === null) return false
  if (typeof identity.expireAt === 'number' && identity.expireAt > 0) {
    return Date.now() < identity.expireAt - 10 * 60 * 1000
  }
  return Date.now() - (identity.at ?? 0) < 24 * 60 * 60 * 1000
}
/** 手上有没有可用的登录身份（device + accessToken）。有就说明登录过，能直连调接口。 */
export function hasIdentity() {
  return readCachedIdentity() !== null
}
/** 调一次开放平台接口 */
/** 401/未授权：清掉缓存的身份，下一次调用重新抓 */
function isAuthError(body) {
  const code = Number(body?.code)
  return code === 401 || code === 403 || /未授权|accessToken|登录已过期|expired/iu.test(String(body?.message ?? ''))
}

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
  if (isAuthError(body) && options.retried !== true) {
    // 身份过期就重抓一次再试；只重试一次，避免死循环
    try { writeFileSync(identityFile(), JSON.stringify({ at: 0 }), 'utf8') } catch { /* 缓存文件不可写就算了 */ }
    try {
      return await callApi(path, bizContent, { ...options, retried: true })
    } catch {
      // 身份也换不出来：这时候只能让人重新登录一次，别再抛底层报错
      throw new Error('网易云登录已过期，请到「设置 → 网易云音乐 → 登录网易云」重新扫码')
    }
  }
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
