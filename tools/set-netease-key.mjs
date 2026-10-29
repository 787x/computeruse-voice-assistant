/**
 * 设置网易云音乐凭据（每个使用者填自己的，绝不随安装包分发）。
 *
 * 用法：
 *   node tools/set-netease-key.mjs --app-id <appId> --key-file <私钥文件>
 *   node tools/set-netease-key.mjs --show
 *   node tools/set-netease-key.mjs --clear
 *
 * 为什么要有这个脚本而不是硬编码：appId 和私钥是**使用者本人的身份**，
 * 跟模型 API Key、搜索 Key 同级。打进安装包等于把自己的账号发给所有人，
 * 而且官方按 appId 计费/限流，泄出去别人用你的额度。
 */
import { readFile, writeFile } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { createPrivateKey } from 'node:crypto'

import { loadConfig, saveConfig } from '../packages/config/store.mjs'

const argv = process.argv.slice(2)
const flag = (name) => {
  const index = argv.indexOf(name)
  return index >= 0 ? argv[index + 1] : undefined
}

if (argv.includes('--show')) {
  const config = await loadConfig()
  const netease = config.netease ?? {}
  console.log(JSON.stringify({
    appId: netease.appId === undefined || netease.appId === '' ? '(未设置)' : `${netease.appId.slice(0, 6)}…`,
    privateKey: netease.privateKey === undefined || netease.privateKey === '' ? '(未设置)' : `已设置（${netease.privateKey.length} 字符）`,
  }, null, 2))
  process.exit(0)
}

if (argv.includes('--clear')) {
  await saveConfig({ netease: { appId: '', privateKey: '' } })
  console.log('已清空网易云凭据')
  process.exit(0)
}

const appId = flag('--app-id')
const keyPath = flag('--key-file')
if (appId === undefined || keyPath === undefined) {
  console.log('用法: node tools/set-netease-key.mjs --app-id <appId> --key-file <私钥文件>')
  process.exit(1)
}
if (!existsSync(keyPath)) {
  console.log(`找不到私钥文件: ${keyPath}`)
  process.exit(1)
}

const privateKey = (await readFile(keyPath, 'utf8')).trim()
// 先验证它确实是一把合法的 RSA 私钥，别把复制错的字符串存进去
try {
  const pem = `-----BEGIN PRIVATE KEY-----\n${(privateKey.replace(/-----[^-]+-----/gu, '').replace(/\s+/gu, '').match(/.{1,64}/gu) ?? []).join('\n')}\n-----END PRIVATE KEY-----\n`
  const key = createPrivateKey({ key: pem, format: 'pem' })
  console.log(`私钥校验通过: ${key.asymmetricKeyType} ${key.asymmetricKeyDetails?.modulusLength} bit`)
} catch (error) {
  console.log(`私钥无效：${error.message}`)
  process.exit(1)
}

await saveConfig({ netease: { appId: appId.trim(), privateKey } })
console.log('已写入配置（仅本机，不随安装包分发）')
