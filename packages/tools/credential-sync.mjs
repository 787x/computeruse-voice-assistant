import { createHash } from 'node:crypto'
/** One application-wide queue for settings and Agent credential synchronization. */
export class CredentialSync {
  #store
  #sync
  #queue = Promise.resolve()
  #pending = new Set()
  #active = null
  #closed = false
  #generation = 0
  constructor(store, synchronize) { this.#store = store; this.#sync = synchronize }
  sync(signal) {
    const generation = this.#generation
    const pending = this.#queue.then(async () => {
      if (this.#closed || generation !== this.#generation) throw new DOMException('Credential sync closed', 'AbortError')
      signal?.throwIfAborted()
      const controller = new AbortController()
      this.#active = controller
      const abort = () => controller.abort()
      signal?.addEventListener('abort', abort, { once: true })
      try {
        for (;;) {
          controller.signal.throwIfAborted()
          const config = await this.#store.loadConfig()
          controller.signal.throwIfAborted()
          const credentials = config.netease ?? {}
          const targetHash = createHash('sha256').update(`${String(credentials.appId ?? '').trim()}:${String(credentials.privateKey ?? '').trim()}`).digest('hex').slice(0, 16)
          if (credentials.syncedHash !== targetHash) {
            try {
              // Any partial CLI mutation invalidates the old version's success cache.
              await this.#store.saveConfig({ netease: { syncedHash: '' } },
                { signal: controller.signal, expectedCredentials: credentials })
            } catch (error) {
              if (error.code === 'STALE_CREDENTIALS') continue
              throw error
            }
          }
          controller.signal.throwIfAborted()
          const result = await this.#sync(credentials, { syncedHash: credentials.syncedHash, signal: controller.signal })
          controller.signal.throwIfAborted()
          try {
            await this.#store.saveConfig({ netease: { syncedHash: result.hash } },
              { signal: controller.signal, expectedCredentials: credentials })
          } catch (error) {
            if (error.code === 'STALE_CREDENTIALS') continue
            throw error
          }
          const latest = await this.#store.loadConfig()
          controller.signal.throwIfAborted()
          if (latest.netease?.appId !== credentials.appId || latest.netease?.privateKey !== credentials.privateKey) continue
          return result
        }
      } finally {
        signal?.removeEventListener('abort', abort)
        this.#active = null
      }
    })
    this.#queue = pending.catch(() => {})
    this.#pending.add(pending)
    pending.then(() => this.#pending.delete(pending), () => this.#pending.delete(pending))
    return pending
  }
  cancelPending() {
    this.#generation++
    this.#active?.abort()
  }
  async close() {
    this.#closed = true
    this.#active?.abort()
    let timer
    await Promise.race([Promise.allSettled([...this.#pending]), new Promise(resolve => { timer = setTimeout(resolve, 1000) })]).finally(() => clearTimeout(timer))
    return { ok: this.#pending.size === 0, remaining: this.#pending.size }
  }
}
