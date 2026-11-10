// Execute the installed sources with host-owned I/O and reactive state mocked.
// No OpenCode account, shell, real tty, or user configuration is touched.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import vm from 'node:vm'
import test from 'node:test'

function load(name, enabled = true) {
  const frames = [], effects = [], listeners = new Map()
  let disposed = false, failWrite = false
  const source = readFileSync(new URL(`../crates/uniterm-server/src/connectors/${name}.js`, import.meta.url), 'utf8')
    .replace(/^import .*$/gm, '')
    .replace('export default ', 'globalThis.plugin = ')
    .replace('export const UnitermNotify = ', 'globalThis.plugin = ')
  const context = vm.createContext({
    fs: { writeFileSync(path, bytes) {
      if (failWrite) { failWrite = false; throw new Error('tty unavailable') }
      assert.equal(path, '/dev/tty')
      assert.match(bytes, /^\x1b\]777;notify;uniterm:\/\/cli-agent;[\x20-\x7e]+\x07$/)
      frames.push(JSON.parse(bytes.slice(bytes.indexOf(';{') + 1, -1)))
    } },
    process: { env: enabled ? { UNITERM: '1' } : {},
      once: (event, fn) => listeners.set(event, fn), off: event => listeners.delete(event) },
    createRoot: fn => fn(() => { disposed = true }),
    createEffect: fn => { effects.push(fn); fn() },
  })
  vm.runInContext(source, context)
  return { plugin: context.plugin, frames, listeners, failNext: () => { failWrite = true }, tick: () => { if (!disposed) effects.forEach(fn => fn()) } }
}

test('OpenCode 1 retries stay busy, unknown statuses are silent, and children retain ancestry', async () => {
  const h = load('opencode-v1')
  assert.equal(typeof h.plugin, 'function') // v1 loader calls every export
  const hooks = await h.plugin()
  const event = (type, properties) => hooks.event({ event: { type, properties } })
  await event('session.created', { info: { id: 'root' } })
  await event('session.status', { sessionID: 'root', status: { type: 'retry' } })
  assert.equal(h.frames.at(-1).event, 'prompt_submit')
  const before = h.frames.length
  await event('session.status', { sessionID: 'root', status: { type: 'future-state' } })
  assert.equal(h.frames.length, before)
  await event('permission.asked', { sessionID: 'root' })
  assert.equal(h.frames.at(-1).event, 'permission_request')
  await event('question.asked', { sessionID: 'root' })
  assert.equal(h.frames.at(-1).event, 'question')
  await event('question.replied', { sessionID: 'root' })
  assert.equal(h.frames.at(-1).event, 'tool_end')
  await event('session.created', { info: { id: 'child', parentID: 'root' } })
  await event('session.idle', { sessionID: 'child' })
  assert.equal(h.frames.at(-1).parent_session_id, 'root')
  await event('session.idle', { sessionID: 'root' })
  assert.equal(h.frames.at(-1).event, 'idle')
  assert.deepEqual([...h.listeners.keys()], ['exit'])
  await hooks.dispose()
  assert.equal(h.listeners.size, 0)
})

test('OpenCode 2 uses local selected-family state, recovers initial waits, and emits no unchanged bytes', () => {
  const h = load('opencode-tui')
  assert.equal(typeof h.plugin, 'object') // v2 schema rejects callable definitions
  assert.equal(typeof h.plugin.id, 'string')
  assert.equal(typeof h.plugin.setup, 'function')
  let route = { type: 'session', sessionID: 'root' }
  const statuses = { root: 'running', other: 'running', child: 'idle' }
  const permissions = { root: ['pending'], other: ['unrelated'] }, forms = {}
  const info = {}
  const cleanup = h.plugin.setup({
    ui: { router: { current: () => route } },
    data: { session: {
      root: id => id === 'child' ? 'root' : id,
      family: id => id === 'root' ? ['root', 'child'] : [id],
      permission: { list: id => permissions[id] }, form: { list: id => forms[id] },
      status: id => statuses[id], get: id => info[id],
    } },
  })
  assert.equal(h.frames.at(-1).event, 'permission_request')
  const count = h.frames.length
  h.tick(); h.tick()
  assert.equal(h.frames.length, count)
  permissions.root = []
  h.failNext()
  h.tick()
  assert.equal(h.frames.length, count)
  h.tick() // A failed tty write must not be cached as delivered.
  assert.equal(h.frames.at(-1).event, 'prompt_submit')
  permissions.child = ['approve child']
  h.tick()
  assert.equal(h.frames.at(-1).event, 'permission_request')
  assert.equal(h.frames.at(-1).session_id, 'root')
  permissions.child = []
  forms.child = ['answer child']
  h.tick()
  assert.equal(h.frames.at(-1).event, 'question')
  forms.child = []
  statuses.root = 'idle'
  h.tick()
  assert.equal(h.frames.at(-1).event, 'idle')
  info.root = { outcome: 'failed' }
  h.tick()
  assert.equal(h.frames.at(-1).event, 'error')
  route = { type: 'session', sessionID: 'other' }
  h.tick()
  assert.equal(h.frames.at(-1).session_id, 'other')
  assert.equal(h.frames.at(-1).event, 'permission_request')
  route = { type: 'home' }
  h.tick()
  assert.equal(h.frames.at(-1).event, 'idle')
  cleanup()
  const after = h.frames.length
  h.tick()
  assert.equal(h.frames.length, after)
  assert.equal(h.listeners.size, 0)
})

test('connectors stay inert outside Uniterm', async () => {
  for (const name of ['opencode-v1', 'opencode-tui']) {
    const h = load(name, false)
    if (typeof h.plugin === 'function') await h.plugin()
    else h.plugin.setup({})
    assert.equal(h.frames.length, 0)
    assert.equal(h.listeners.size, 0)
  }
})
