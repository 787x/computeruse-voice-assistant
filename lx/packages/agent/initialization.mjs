/** Owns the shared Agent initialization, its consumers and native resources. */
export class AgentInitialization {
  #loading = null
  #runtime = null
  #closed = false
  #jobs = new Set()
  #disposals = new WeakMap()
  #resources = new Set()
  #publish
  #diagnostic
  constructor({ publish = () => {}, diagnostic = () => {} } = {}) {
    this.#publish = publish
    this.#diagnostic = diagnostic
  }
  get runtime() { return this.#runtime }
  get loading() { return this.#loading !== null }

  ensure(signal, build) {
    if (signal?.aborted) return Promise.reject(aborted())
    if (this.#closed) return Promise.reject(aborted())
    if (this.#runtime !== null) return Promise.resolve(this.#runtime)
    let job = this.#loading
    if (job === null || job.controller.signal.aborted) {
      job = { controller: new AbortController(), users: new Set(), resources: new Set(), committed: false }
      this.#loading = job
      this.#jobs.add(job)
      const checkpoint = () => {
        if (this.#closed || job.controller.signal.aborted) throw aborted()
      }
      job.promise = Promise.resolve().then(() => {
        checkpoint()
        return build({ signal: job.controller.signal, checkpoint, own: resource => {
          job.resources.add(resource)
          this.#resources.add(resource)
          if (this.#closed || job.controller.signal.aborted) {
            void this.#dispose(resource)
            throw aborted()
          }
          return resource
        } })
      }).then(runtime => {
        checkpoint()
        this.#runtime = runtime
        this.#publish(runtime)
        job.committed = true
        return runtime
      }).finally(async () => {
        if (!job.committed) {
          // A publication failure must not leave an unusable cache entry.
          if (this.#runtime?.toolkit && job.resources.has(this.#runtime.toolkit)) {
            this.#runtime = null
            this.#publish(null)
          }
          await Promise.all([...job.resources].map(resource => this.#dispose(resource)))
        }
        if (this.#loading === job) this.#loading = null
        this.#jobs.delete(job)
      })
    }
    const user = {}
    job.users.add(user)
    return new Promise((resolve, reject) => {
      const release = () => {
        signal?.removeEventListener('abort', onAbort)
        job.controller.signal.removeEventListener('abort', onAbort)
        job.users.delete(user)
        if (!job.committed && job.users.size === 0) {
          job.controller.abort()
          // Dispose now, without waiting for imports or warmup to finish.
          for (const resource of job.resources) void this.#dispose(resource)
          if (this.#loading === job) this.#loading = null
        }
      }
      const onAbort = () => { release(); reject(aborted()) }
      signal?.addEventListener('abort', onAbort, { once: true })
      job.controller.signal.addEventListener('abort', onAbort, { once: true })
      job.promise.then(value => {
        release()
        if (signal?.aborted || this.#closed) reject(aborted())
        else resolve(value)
      }, error => { release(); reject(error) })
      if (signal?.aborted) onAbort()
    })
  }

  #dispose(resource) {
    let promise = this.#disposals.get(resource)
    if (!promise) {
      promise = Promise.resolve().then(() => resource.dispose()).then(() => {
        this.#resources.delete(resource)
        return { resource }
      }).catch(error => {
        // Keep ownership on failure so close() can retry; never orphan it silently.
        this.#disposals.delete(resource)
        try { this.#diagnostic(error) } catch { /* diagnostics must not reject detached cleanup */ }
        return { resource, error }
      })
      this.#disposals.set(resource, promise)
    }
    return promise
  }

  async close() {
    this.#closed = true // Revoke publication and acquisition before any await.
    const resources = new Set(this.#resources)
    for (const job of this.#jobs) {
      job.controller.abort()
      for (const resource of job.resources) resources.add(resource)
    }
    if (this.#runtime?.toolkit) resources.add(this.#runtime.toolkit)
    this.#runtime = null
    this.#loading = null
    this.#publish(null)
    let failures = []
    for (let attempt = 0; attempt < 2; attempt++) {
      const results = await Promise.all([...resources].map(resource => this.#dispose(resource)))
      failures = results.filter(result => result.error)
      resources.clear()
      for (const result of failures) resources.add(result.resource)
      if (!failures.length) break
    }
    return { ok: failures.length === 0, remaining: failures.length,
      errors: failures.map(result => `${result.resource.constructor?.name ?? "Resource"}: ${result.error.message ?? String(result.error)}`) }
  }
}
function aborted() { return new DOMException('Agent initialization cancelled', 'AbortError') }
