// uniterm://cli-agent - OpenCode 2 local terminal connector.
// The shared server cannot identify the terminal that owns a session.
import * as fs from "node:fs"
import { createEffect, createRoot } from "solid-js"

const emit = (event, session_id, task = {}) => {
  if (!process.env.UNITERM) return
  try {
    const json = JSON.stringify({ agent: "opencode", event, session_id, ...task })
      .replace(/[;\u007f-\uffff]/g, c => "\\u" + c.charCodeAt(0).toString(16).padStart(4, "0"))
    fs.writeFileSync("/dev/tty", "\x1b]777;notify;uniterm://cli-agent;" + json + "\x07")
    return true
  } catch { return false }
}

export default {
  id: "uniterm.notify.tui",
  setup(ctx) {
    if (!process.env.UNITERM) return
    let selected
    let last
    const tasks = new Map()
    const observeTasks = root => {
      const messages = ctx.data.session.message?.list(root)
      if (!Array.isArray(messages) || !messages.length) return
      let state = tasks.get(root)
      if (!state) {
        if (tasks.size >= 64) tasks.delete(tasks.keys().next().value)
        state = { last: messages.at(-1)?.id, turn: undefined, pending: undefined }
        tasks.set(root, state)
        // Cached history is not new input. On first discovery only an
        // unfinished current root assignment is eligible for capture.
        if (ctx.data.session.status(root) !== "running") return
        for (let i = messages.length - 1; i >= Math.max(0, messages.length - 128); i--) {
          const message = messages[i]
          if (message.type === "idle") return
          if (message.type === "user") {
            if (typeof message.text === "string" && Buffer.byteLength(message.text, "utf8") <= 16384) {
              if (emit("prompt_submit", root, { prompt: message.text, turn_id: message.id })) state.turn = message.id
              else state.pending = message
            }
            return
          }
        }
        return
      }
      if (state.pending) {
        if (!emit("prompt_submit", root, { prompt: state.pending.text, turn_id: state.pending.id })) return
        state.turn = state.pending.id
        state.pending = undefined
      }
      let start = messages.length
      while (start > Math.max(0, messages.length - 128) && messages[start - 1]?.id !== state.last) start--
      for (let i = start; i < messages.length; i++) {
        const message = messages[i]
        if (message.type === "user" && typeof message.text === "string" && Buffer.byteLength(message.text, "utf8") <= 16384) {
          if (!emit("prompt_submit", root, { prompt: message.text, turn_id: message.id })) return
          state.turn = message.id
        } else if (message.type === "idle" && state.turn && ["succeeded", "failed", "interrupted"].includes(message.outcome)) {
          if (!emit("idle", root, { turn_id: state.turn,
            turn_complete: message.outcome === "succeeded", turn_interrupted: message.outcome !== "succeeded" })) return
          state.turn = undefined
        }
        state.last = message.id
      }
    }
    const report = (event, id) => {
      const key = JSON.stringify([event, id])
      if (key === last) return
      if (emit(event, id)) last = key
    }
    const dispose = createRoot(dispose => {
      createEffect(() => {
        const route = ctx.ui.router.current()
        if (route.type === "session") selected = route.sessionID
        else if (route.type === "home") selected = undefined
        if (!selected) { report("idle"); return }
        const root = ctx.data.session.root(selected)
        observeTasks(root)
        const family = ctx.data.session.family(root)
        // A blocked child needs the human at this terminal; a child's idle or
        // completion must never finish the root's running turn.
        if (family.some(id => ctx.data.session.permission.list(id)?.length)) {
          report("permission_request", root)
        } else if (family.some(id => ctx.data.session.form.list(id)?.length)) {
          report("question", root)
        } else if (ctx.data.session.status(root) === "running") {
          report("prompt_submit", root)
        } else {
          report(ctx.data.session.get(root)?.outcome === "failed" ? "error" : "idle", root)
        }
      })
      return dispose
    })
    // Do not intercept signals: installing SIGINT handlers changes host exit
    // behavior. Kernel process watches cover abnormal termination.
    const end = () => emit("session_end", selected)
    process.once("exit", end)
    return () => { dispose(); process.off("exit", end) }
  },
}
