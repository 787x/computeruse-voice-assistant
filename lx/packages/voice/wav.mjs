/**
 * WAV 读写工具。
 *
 * 为什么不按固定偏移读：真实音频常在 RIFF 里夹带 LIST/INFO 等块，
 * `data` 不一定在 44 字节处（本项目的官方测试音频就是这种，实测踩过）。
 * 必须按块遍历。
 */
import { readFileSync, writeFileSync } from 'node:fs'

/**
 * @param {string} path
 * @returns {{samples: Float32Array, sampleRate: number, channels: number, bitsPerSample: number}}
 */
export function readWav(path) {
  const buf = readFileSync(path)
  if (buf.toString('ascii', 0, 4) !== 'RIFF' || buf.toString('ascii', 8, 12) !== 'WAVE') {
    throw new Error(`不是 WAV 文件: ${path}`)
  }

  let offset = 12
  let fmt = null
  let data = null

  while (offset + 8 <= buf.length) {
    const id = buf.toString('ascii', offset, offset + 4)
    const size = buf.readUInt32LE(offset + 4)
    const body = offset + 8

    if (id === 'fmt ') {
      fmt = {
        format: buf.readUInt16LE(body),
        channels: buf.readUInt16LE(body + 2),
        sampleRate: buf.readUInt32LE(body + 4),
        bitsPerSample: buf.readUInt16LE(body + 14),
      }
    } else if (id === 'data') {
      data = { body, size }
    }
    // 块按偶数字节对齐
    offset = body + size + (size % 2)
  }

  if (fmt === null) throw new Error(`缺少 fmt 块: ${path}`)
  if (data === null) throw new Error(`缺少 data 块: ${path}`)

  const { channels, sampleRate, bitsPerSample } = fmt
  const bytesPerSample = bitsPerSample / 8
  const frames = Math.floor(data.size / (bytesPerSample * channels))
  const samples = new Float32Array(frames)

  for (let i = 0; i < frames; i += 1) {
    // 只取第一声道
    const at = data.body + i * bytesPerSample * channels
    if (bitsPerSample === 16) samples[i] = buf.readInt16LE(at) / 32768
    else if (bitsPerSample === 32) samples[i] = buf.readInt32LE(at) / 2147483648
    else if (bitsPerSample === 8) samples[i] = (buf.readUInt8(at) - 128) / 128
    else throw new Error(`不支持的位深: ${bitsPerSample}`)
  }

  return { samples, sampleRate, channels, bitsPerSample }
}

/** 写 16-bit 单声道 PCM WAV */
export function writeWav(path, samples, sampleRate) {
  const n = samples.length
  const buffer = Buffer.alloc(44 + n * 2)
  buffer.write('RIFF', 0)
  buffer.writeUInt32LE(36 + n * 2, 4)
  buffer.write('WAVE', 8)
  buffer.write('fmt ', 12)
  buffer.writeUInt32LE(16, 16)
  buffer.writeUInt16LE(1, 20)
  buffer.writeUInt16LE(1, 22)
  buffer.writeUInt32LE(sampleRate, 24)
  buffer.writeUInt32LE(sampleRate * 2, 28)
  buffer.writeUInt16LE(2, 32)
  buffer.writeUInt16LE(16, 34)
  buffer.write('data', 36)
  buffer.writeUInt32LE(n * 2, 40)
  for (let i = 0; i < n; i += 1) {
    const v = Math.max(-1, Math.min(1, samples[i] ?? 0))
    buffer.writeInt16LE(Math.round(v * 32767), 44 + i * 2)
  }
  writeFileSync(path, buffer)
}
