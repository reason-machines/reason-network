// reason network for pi: written by `reason network setup`, removed by `reason uninstall`.
// Gives each session reason network's tools (deferred: kept out of the model's context until it looks for them), tells
// reason network whether the session is busy and what it is called, and delivers teammates' messages into it.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"
import { execFile } from "node:child_process"

const ENGINE = "__ENGINE_EXE__"

export default function (pi: ExtensionAPI) {
  let id = ""
  let cwd = process.cwd()
  let busy = false
  let delivering = false
  let timer: ReturnType<typeof setInterval> | undefined

  // windowsHide: on Windows a pi without a console (started by an editor or a GUI) would flash a window per call.
  const hook = (args: string[]) =>
    new Promise<string>((resolve) =>
      execFile(
        ENGINE,
        ["hook", "pi", ...args, "--session", id],
        { cwd, timeout: 10_000, windowsHide: true },
        (err, out) => resolve(err ? "" : String(out).trim()),
      ),
    )
  // Mid-run, only what should interrupt it ("post-tool-use"); once idle, everything ("poll").
  const deliver = async (event: "post-tool-use" | "poll") => {
    if (!id || delivering) return
    delivering = true
    try {
      const text = await hook([event])
      if (text) pi.sendUserMessage(text, busy ? { deliverAs: "steer" } : undefined)
    } finally {
      delivering = false
    }
  }

  pi.on("session_start", async (_event, ctx) => {
    id = ctx.sessionManager.getSessionId()
    cwd = ctx.cwd
    pi.registerMcpServer("reason-network", {
      command: ENGINE,
      args: ["mcp"],
      env: { REASON_NETWORK_AGENT: "pi", REASON_NETWORK_SESSION: id },
      exposure: "deferred",
      description: "Message teammates' coding agents and see what each of their sessions is working on",
    })
    await hook(["status", "--state", "idle"])
    const name = ctx.sessionManager.getSessionName()
    if (name) await hook(["doing", "--doing", name])
    timer ??= setInterval(() => {
      if (!busy) void deliver("poll")
    }, 5_000)
  })
  pi.on("session_info_changed", async (event) => {
    await hook(["doing", "--doing", event.name ?? ""])
  })
  pi.on("agent_start", async () => {
    busy = true
    await hook(["status", "--state", "busy"])
  })
  pi.on("turn_end", async () => {
    await deliver("post-tool-use")
  })
  pi.on("agent_settled", async () => {
    busy = false
    await hook(["status", "--state", "idle"])
    await deliver("poll")
  })
  pi.on("session_shutdown", async () => {
    if (timer) clearInterval(timer)
    timer = undefined
    await hook(["session-end"])
  })
}
