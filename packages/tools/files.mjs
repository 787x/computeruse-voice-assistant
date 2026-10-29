/**
 * 文件操作（抄自 cherry-remote-app 的 `file` 动作，改成给模型用）。
 *
 * 为什么一个工具带 action 而不是拆成五个：模型对"同一类操作的多个动词"
 * 很容易选错工具名（实测过 read_clipboard / type 都吃过这个亏）；
 * 一个工具 + action 枚举，参数校验更集中，权限也只需要按 action 分级。
 *
 * 安全：读放行、写/删必须问。删除还额外挡住盘符根目录与 Windows 目录——
 * 这两处一旦误删就不是"重装软件"能解决的。
 */
import { createHash } from 'node:crypto'
import { copyFile, cp, mkdir, readdir, readFile, rm, stat, unlink, writeFile, rename } from 'node:fs/promises'
import { dirname, isAbsolute, join, resolve } from 'node:path'

export const FILES_DEFAULTS = {
  maxBytes: 200 * 1024,
  maxEntries: 200,
}

export const FILES_TOOL = {
  type: 'function',
  function: {
    name: 'files',
    description: '本地文件操作：list（列目录）/ read（读文本）/ write（写文件）/ copy / move / delete / info。'
      + '路径用 Windows 绝对路径。读文件默认放行，写入与删除会先向用户确认。',
    parameters: {
      type: 'object',
      properties: {
        action: { type: 'string', enum: ['list', 'read', 'write', 'copy', 'move', 'delete', 'info'] },
        path: { type: 'string', description: '目标路径（绝对路径）' },
        dest: { type: 'string', description: 'copy / move 的目标路径' },
        content: { type: 'string', description: 'write 要写入的文本' },
        recursive: { type: 'boolean', description: 'list 是否递归' },
      },
      required: ['action', 'path'],
      additionalProperties: false,
    },
  },
}

/** 危险路径：盘符根、Windows 目录、用户目录根 */
function assertSafeTarget(path) {
  const resolved = resolve(path)
  if (/^[a-zA-Z]:\\?$/u.test(resolved)) throw new Error(`拒绝对盘符根目录操作：${resolved}`)
  const lower = resolved.toLowerCase()
  const blocked = [process.env.SystemRoot?.toLowerCase(), 'c:\\windows', 'c:\\program files', 'c:\\program files (x86)']
    .filter((item) => typeof item === 'string' && item !== '')
  for (const item of blocked) {
    if (lower === item.replace(/\\$/u, '')) throw new Error(`拒绝对系统目录本身操作：${resolved}`)
  }
  const home = process.env.USERPROFILE?.toLowerCase()
  if (home !== undefined && lower === home) throw new Error(`拒绝操作用户目录根：${resolved}`)
}

export async function runFiles(args = {}, options = {}) {
  const cfg = { ...FILES_DEFAULTS, ...options }
  const action = String(args.action ?? '')
  const target = String(args.path ?? '')
  if (target === '') throw new Error('files 需要 path')
  const path = isAbsolute(target) ? target : resolve(target)

  if (action === 'list') {
    const recursive = args.recursive === true
    const entries = await walk(path, recursive, cfg.maxEntries)
    const lines = entries.map((entry) => `${entry.type === 'dir' ? '[目录]' : '[文件]'} ${entry.path}${entry.type === 'file' ? `  ${entry.size} 字节` : ''}`)
    return {
      text: `列出 ${path}（${entries.length} 项${entries.length >= cfg.maxEntries ? '，已截断' : ''}）：\n${lines.join('\n')}`,
      data: { path, entries },
    }
  }

  if (action === 'read') {
    const buffer = await readFile(path)
    const slice = buffer.subarray(0, cfg.maxBytes)
    const text = slice.toString('utf8')
    const truncated = buffer.length > cfg.maxBytes
    return {
      text: `读取 ${path}（${buffer.length} 字节${truncated ? `，只取前 ${cfg.maxBytes}` : ''}）：\n${text}`,
      data: { path, bytes: buffer.length, truncated, sha256: createHash('sha256').update(buffer).digest('hex').slice(0, 16) },
    }
  }

  if (action === 'write') {
    const content = String(args.content ?? '')
    await mkdir(dirname(path), { recursive: true })
    await writeFile(path, content, 'utf8')
    return { text: `已写入 ${path}（${Buffer.byteLength(content, 'utf8')} 字节）`, data: { path, ok: true } }
  }

  if (action === 'copy' || action === 'move') {
    const dest = String(args.dest ?? '')
    if (dest === '') throw new Error(`${action} 需要 dest`)
    const info = await stat(path)
    if (action === 'move') {
      await mkdir(dirname(dest), { recursive: true })
      await rename(path, dest)
    } else if (info.isDirectory()) {
      await cp(path, dest, { recursive: true })
    } else {
      await copyFile(path, dest)
    }
    return { text: `${action === 'move' ? '已移动' : '已复制'} ${path} → ${dest}`, data: { ok: true } }
  }

  if (action === 'delete') {
    assertSafeTarget(path)
    const info = await stat(path).catch(() => null)
    if (info === null) throw new Error(`路径不存在：${path}`)
    if (info.isDirectory()) await rm(path, { recursive: true, force: false })
    else await unlink(path)
    return { text: `已删除 ${info.isDirectory() ? '目录' : '文件'} ${path}`, data: { path, ok: true } }
  }

  if (action === 'info') {
    const info = await stat(path)
    return {
      text: `路径：${resolve(path)}\n类型：${info.isDirectory() ? '目录' : '文件'}\n大小：${info.size} 字节\n修改时间：${info.mtime.toISOString()}`,
      data: { path, isDirectory: info.isDirectory(), size: info.size, mtime: info.mtime.toISOString() },
    }
  }

  throw new Error(`未知 action：${action}`)
}

async function walk(root, recursive, limit) {
  const out = []
  const queue = [root]
  while (queue.length > 0 && out.length < limit) {
    const dir = queue.shift()
    let entries
    try {
      entries = await readdir(dir, { withFileTypes: true })
    } catch {
      continue
    }
    const sorted = entries.sort((a, b) => Number(b.isDirectory()) - Number(a.isDirectory()) || a.name.localeCompare(b.name))
    for (const entry of sorted) {
      if (out.length >= limit) break
      const full = join(dir, entry.name)
      if (entry.isDirectory()) {
        out.push({ type: 'dir', path: full, size: 0 })
        if (recursive) queue.push(full)
      } else {
        const info = await stat(full).catch(() => null)
        out.push({ type: 'file', path: full, size: info?.size ?? 0 })
      }
    }
  }
  return out
}
