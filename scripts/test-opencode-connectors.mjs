// Execute the installed sources with host-owned I/O and reactive state mocked.
// No OpenCode account, shell, real tty, or user configuration is touched.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import vm from 'node:vm'
import test from 'node:test'

function load(name, enabled = true) {
  const frames = [], effects = [], listeners = new Map()
  let disposed = false, failWrite = false
  const raw = name === 'pi'
    ? readFileSync(new URL('../crates/uniterm-server/src/connectors.rs', import.meta.url), 'utf8').match(/const EXTENSION: &str = r#"([\s\S]*?)"#;/)[1]
    : readFileSync(new URL(`../crates/uniterm-server/src/connectors/${name}.js`, import.meta.url), 'utf8')
  const source = raw
    .replace('event: string, context?: any, task: any = {}', 'event, context, task = {}')
    .replace('pi: ExtensionAPI', 'pi')
    .replace('let outcome: string | undefined', 'let outcome')
    .replace(/^import .*$/gm, '')
    .replace('export default ', 'globalThis.plugin = ')
    .replace('export const UnitermNotify = ', 'globalThis.plugin = ')
  const context = vm.createContext({
    Buffer,
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

test('OpenCode task evidence captures root user text and explicit outcomes only', async () => {
  const h = load('opencode-v1')
  const hooks = await h.plugin()
  await hooks['chat.message']({ sessionID: 'root' }, { message: { id: 'user-1' }, parts: [
    { type: 'text', text: 'Fix login' }, { type: 'text', text: 'private generated context', synthetic: true },
  ] })
  assert.equal(h.frames.at(-1).prompt, 'Fix login')
  assert.equal(h.frames.at(-1).turn_id, 'user-1')
  await hooks.event({ event: { type: 'session.idle', properties: { sessionID: 'root' } } })
  assert.equal(h.frames.at(-1).turn_complete, undefined)
  await hooks.event({ event: { type: 'message.updated', properties: { info: {
    role: 'assistant', sessionID: 'root', parentID: 'user-1', time: { completed: 1 }, finish: 'stop',
  } } } })
  assert.equal(h.frames.at(-1).turn_complete, true)

  const t = load('opencode-tui')
  let status = 'idle'
  const messages = [{ id: 'old', type: 'user', text: 'old private history' }]
  const cleanup = t.plugin.setup({
    ui: { router: { current: () => ({ type: 'session', sessionID: 'root' }) } },
    data: { session: {
      root: id => id, family: id => [id], permission: { list: () => [] }, form: { list: () => [] },
      status: () => status, get: () => ({}), message: { list: () => messages },
    } },
  })
  assert.equal(t.frames.some(frame => frame.prompt), false)
  messages.push({ id: 'user-2', type: 'user', text: 'Repair;界\nlogin' })
  status = 'running'
  t.tick()
  assert.equal(t.frames.find(frame => frame.prompt)?.prompt, 'Repair;界\nlogin')
  const count = t.frames.length
  t.tick(); t.tick()
  assert.equal(t.frames.length, count)
  status = 'idle'
  t.tick()
  assert.equal(t.frames.at(-1).turn_complete, undefined)
  messages.push({ id: 'result', type: 'idle', outcome: 'succeeded' })
  t.tick()
  assert.equal(t.frames.at(-1).turn_complete, true)
  assert.equal(t.frames.at(-1).turn_id, 'user-2')
  cleanup()
})

test('Pi tasks use accepted user starts and final settled outcomes', () => {
  const h = load('pi')
  const handlers = new Map()
  h.plugin({ on: (event, handler) => handlers.set(event, handler) })
  const context = { sessionManager: { getSessionId: () => 'root', getSessionFile: () => '/tmp/root.jsonl' } }
  const emit = (name, event) => handlers.get(name)(event, context)
  emit('before_agent_start', { messageType: 'custom', message: { content: 'private context' } })
  assert.equal(h.frames.length, 0)
  emit('before_agent_start', { messageType: 'user', prompt: 'Fix login' })
  assert.equal(h.frames.at(-1).prompt, 'Fix login')
  emit('agent_before_settle', { outcome: 'completed' })
  emit('agent_settled', { aborted: false })
  assert.equal(h.frames.at(-1).turn_complete, true)
  emit('before_agent_start', { messageType: 'user', prompt: 'Fix tests' })
  emit('agent_before_settle', { outcome: 'error' })
  emit('agent_settled', { aborted: false })
  assert.equal(h.frames.at(-1).turn_interrupted, true)
  emit('agent_settled', { aborted: true })
  assert.equal(h.frames.at(-1).turn_interrupted, true)
})
