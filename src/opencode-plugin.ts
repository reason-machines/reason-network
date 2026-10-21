// reason network for opencode: written by `reason network setup`, removed by `reason uninstall`.
// Reports each session's busy/idle state and what it is doing (its title and todo in progress) to reason network, and
// delivers teammates' messages into idle sessions, on opencode 1.x and 2.x.
import type { Plugin } from "@opencode-ai/plugin"
import { execFile } from "node:child_process"

const ENGINE = "__ENGINE_EXE__"

// windowsHide: opencode's background server has no console on Windows, so each call (a poll every few seconds per
// idle session) would open a console window of its own that closes at once.
const run = (args: string[], cwd: string) =>
  new Promise<string>((resolve) =>
    execFile(ENGINE, args, { cwd, timeout: 10_000, windowsHide: true }, (err, out) =>
      resolve(err ? "" : String(out).trim()),
    ),
  )

// What both plugin formats share; `prompt` puts a teammate's message into a session.
const network = (prompt: (id: string, text: string) => Promise<unknown>) => {
  const idle = new Set<string>()
  const subagents = new Set<string>()
  const delivering = new Set<string>()
  const dirs = new Map<string, string>()
  const titles = new Map<string, string>()
  const tasks = new Map<string, string>()
  const doing = (id: string) => {
    const line = [titles.get(id), tasks.get(id)].filter(Boolean).join(" · ")
    void run(["hook", "opencode", "doing", "--session", id, "--doing", line], dirs.get(id)!)
  }

  const deliver = async (id: string) => {
    if (delivering.has(id) || subagents.has(id)) return
    delivering.add(id)
    try {
      const text = await run(["hook", "opencode", "poll", "--session", id], dirs.get(id)!)
      if (!text) return
      idle.delete(id)
      await prompt(id, text)
    } finally {
      delivering.delete(id)
    }
  }
  const timer = setInterval(() => idle.forEach((id) => void deliver(id)), 5_000)

  return {
    subagent: (id: string) => subagents.add(id),
    title: (id: string, title: string, dir: string) => {
      // Until opencode names it, a session is "New session - <date>".
      if (title.startsWith("New session") || titles.get(id) === title || subagents.has(id)) return
      dirs.set(id, dir)
      titles.set(id, title)
      doing(id)
    },
    todo: (id: string, now: string, dir: string) => {
      if (tasks.get(id) === now || subagents.has(id)) return
      dirs.set(id, dir)
      tasks.set(id, now)
      doing(id)
    },
    status: async (id: string, state: string, dir: string) => {
      if (subagents.has(id)) return
      dirs.set(id, dir)
      void run(["hook", "opencode", "status", "--session", id, "--state", state], dir)
      if (state === "idle") {
        idle.add(id)
        await deliver(id)
      } else idle.delete(id)
    },
    stop: () => clearInterval(timer),
    // Off the network: the session is gone, or opencode unloaded this plugin and nothing delivers to it any more.
    end: (id?: string) => {
      for (const [s, dir] of [...dirs]) {
        if (id && s !== id) continue
        idle.delete(s)
        dirs.delete(s)
        void run(["hook", "opencode", "session-end", "--session", s], dir)
      }
    },
  }
}

const ReasonNetwork: Plugin = async ({ client, directory }) => {
  const last = new Map<string, { agent?: string; model?: { providerID: string; modelID: string } }>()
  const net = network((id, text) =>
    client.session.promptAsync({ path: { id }, body: { ...(last.get(id) ?? {}), parts: [{ type: "text", text }] } }),
  )

  return {
    event: async ({ event }) => {
      const e = event as any
      if (e.type === "session.created" && e.properties?.info?.parentID) net.subagent(e.properties.info.id)
      if (e.type === "message.updated" && e.properties?.info?.role === "user") {
        const m = e.properties.info
        last.set(m.sessionID, { agent: m.agent, model: m.model })
      }
      if (e.type === "session.updated") {
        const { id, title } = e.properties?.info ?? {}
        if (id && title) net.title(id, title, directory)
      }
      if (e.type === "todo.updated") {
        const { sessionID, todos } = e.properties ?? {}
        const now = (todos ?? []).find((t: any) => t.status === "in_progress")?.content ?? ""
        if (sessionID) net.todo(sessionID, now, directory)
      }
      if (e.type === "session.status") await net.status(e.properties.sessionID, e.properties.status.type, directory)
    },
    dispose: async () => net.stop(),
  }
}

// opencode 2.x has no todo events, and a session's busy and idle come as its execution starting and ending (it
// doesn't send session.status).
const STATE: Record<string, string> = {
  "session.execution.started": "busy",
  "session.execution.succeeded": "idle",
  "session.execution.failed": "idle",
  "session.execution.interrupted": "idle",
}

// opencode 1.x calls `server`, 2.x calls `setup`.
export default {
  id: "reason-network",
  server: ReasonNetwork,
  setup: (ctx: any) => {
    // 1.x calls this too, with a context that has no events; it runs `server` instead.
    if (!ctx.event) return
    const abort = new AbortController()
    const net = network((sessionID, text) => ctx.session.prompt({ sessionID, text }))
    void (async () => {
      for await (const e of ctx.event.subscribe({ signal: abort.signal })) {
        const d = e.data ?? {}
        const dir = e.location?.directory ?? ctx.location.directory
        if (e.type === "session.created" && d.parentID) net.subagent(d.sessionID)
        if (e.type === "session.renamed") net.title(d.sessionID, d.title, dir)
        if (STATE[e.type]) void net.status(d.sessionID, STATE[e.type], dir)
        if (e.type === "session.deleted") net.end(d.sessionID)
      }
    })().catch(() => {})
    // opencode unloads a folder's plugins when it has been idle a while; its background server stays up, so the
    // daemon can't tell its sessions went away.
    return () => {
      abort.abort()
      net.stop()
      net.end()
    }
  },
}
