/**
 * 写入搜索 API Key（Tavily）。
 *
 * 用法：node tools/set-search-key.mjs tvly-xxxxxxxx
 *
 * 走 store.saveConfig，所以和界面写配置是同一条路径：临时文件 + rename，
 * 不会写一半留下坏配置。Key 在 .config/agent.json 里是明文（已 gitignore），
 * 也可以改用环境变量 TAVILY_API_KEY（优先级更高，不落盘）。
 */
import { saveConfig, loadPublicConfig } from '../packages/config/store.mjs'

const key = process.argv[2]
if (!key) {
  console.error('用法: node tools/set-search-key.mjs <tavily-key>\n清除: node tools/set-search-key.mjs --clear')
  process.exit(2)
}

const patch = key === '--clear'
  ? { search: { provider: 'tavily', apiKey: null } }
  : { search: { provider: 'tavily', apiKey: key.trim() } }

const saved = await saveConfig(patch)
console.log(JSON.stringify({
  provider: saved.search?.provider,
  apiKey: saved.search?.apiKey,
  hasApiKey: saved.search?.hasApiKey,
  configFile: '.config/agent.json',
}, null, 1))
