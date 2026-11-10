// uniterm://cli-agent - OpenCode 2 local terminal connector.
// The shared server cannot identify the terminal that owns a session.
import * as fs from "node:fs"
import { createEffect, createRoot } from "solid-js"

const emit = (event, session_id) => {
  if (!process.env.UNITERM) return
  try {
    const json = JSON.stringify({ agent: "opencode", event, session_id })
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
