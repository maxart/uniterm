// uniterm connector: reports OpenCode lifecycle over OSC 777.
// Installed by uniterm (Agents > Setup...); safe to delete.
import * as fs from "node:fs"

const emit = (event, session_id, parent_session_id) => {
  if (!process.env.UNITERM) return
  try {
    fs.writeFileSync(
      "/dev/tty",
      "\x1b]777;notify;uniterm://cli-agent;" + JSON.stringify({
        agent: "opencode", event, session_id, parent_session_id,
      }).replace(/[;\u007f-\uffff]/g, c => "\\u" + c.charCodeAt(0).toString(16).padStart(4, "0")) + "\x07",
    )
  } catch {}
}

export const UnitermNotify = async () => {
  if (!process.env.UNITERM) return {}
  emit("session_start")
  let ended = false
  let rootSession
  const parents = new Map()
  const report = (event, id) => {
    const parent = parents.get(id)
    if (!parent && id) rootSession = id
    emit(event, id || rootSession, parent)
  }
  const end = () => {
    if (!ended) {
      ended = true
      report("session_end", rootSession)
    }
  }
  // session.deleted does not fire on /exit or ^D; catch the process end too.
  process.once("exit", end)
  return {
    dispose: async () => process.off("exit", end),
    event: async ({ event }) => {
      const t = event?.type
      const id = event.properties?.sessionID || event.properties?.info?.id
      if (t === "session.created") {
        const info = event.properties?.info
        if (info?.parentID) parents.set(info.id, info.parentID)
        report("session_start", id)
      }
      else if (t === "session.status") {
        const status = event.properties?.status?.type
        if (status === "idle") report("idle", id)
        else if (status === "busy" || status === "retry") report("prompt_submit", id)
      }
      else if (t === "session.idle") report("idle", id)
      else if (t === "session.error") report("error", id)
      else if (t === "session.deleted" && id === rootSession) end()
      else if (t === "permission.asked") report("permission_request", id)
      else if (t === "question.asked") report("question", id)
      else if (["permission.replied", "question.replied", "question.rejected"].includes(t)) report("tool_end", id)
    },
    "chat.message": async (input) => report("prompt_submit", input?.sessionID),
    "tool.execute.before": async (input) => report("tool_start", input?.sessionID),
    "tool.execute.after": async (input) => report("tool_end", input?.sessionID),
  }
}
