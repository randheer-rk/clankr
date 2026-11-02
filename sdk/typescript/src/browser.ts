// A tiny headless-Chromium driver over the DevTools protocol, for agents built on
// clankr/agent-browser. No dependencies: Node's built-in WebSocket speaks CDP.
//
//   import { launch } from "@clankr/sdk/browser"
//   const b = await launch()
//   const page = await b.newPage()
//   await page.goto("data:text/html,<title>hi</title>")
//   const title = await page.evaluate("document.title")
//   const png = await page.screenshot()          // Buffer
//   await b.close()
//
// The browser runs inside the agent's sandbox, so it has the agent's network: none, unless
// the deployment grants egress. Chromium's own sandbox is off; kennel is the sandbox.

import { spawn, type ChildProcess } from "node:child_process"
import { mkdtempSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

const CHROMIUM = process.env.CLANKR_CHROMIUM ?? "/usr/bin/chromium"

class Cdp {
  private next = 0
  private pending = new Map<number, { ok: (v: any) => void; err: (e: Error) => void }>()
  private listeners = new Map<string, ((p: any) => void)[]>()
  private ws: WebSocket
  constructor(ws: WebSocket) {
    this.ws = ws
    ws.addEventListener("message", ev => {
      const m = JSON.parse(String(ev.data))
      if (m.id !== undefined) {
        const p = this.pending.get(m.id)
        this.pending.delete(m.id)
        if (m.error) p?.err(new Error(`${m.error.message} (${m.error.code})`))
        else p?.ok(m.result)
      } else {
        for (const l of this.listeners.get(`${m.sessionId ?? ""}:${m.method}`) ?? []) l(m.params)
      }
    })
  }
  send(method: string, params: object = {}, sessionId?: string): Promise<any> {
    const id = ++this.next
    return new Promise((ok, err) => {
      this.pending.set(id, { ok, err })
      this.ws.send(JSON.stringify({ id, method, params, sessionId }))
    })
  }
  once(method: string, sessionId: string): Promise<any> {
    return new Promise(ok => {
      const key = `${sessionId}:${method}`
      const l = (p: any) => {
        this.listeners.set(key, (this.listeners.get(key) ?? []).filter(x => x !== l))
        ok(p)
      }
      this.listeners.set(key, [...(this.listeners.get(key) ?? []), l])
    })
  }
  close() {
    this.ws.close()
  }
}

export class Page {
  private cdp: Cdp
  readonly sessionId: string
  constructor(cdp: Cdp, sessionId: string) {
    this.cdp = cdp
    this.sessionId = sessionId
  }
  /** Navigates and waits for the load event. */
  async goto(url: string, timeoutMs = 30000): Promise<void> {
    const loaded = this.cdp.once("Page.loadEventFired", this.sessionId)
    const r = await this.cdp.send("Page.navigate", { url }, this.sessionId)
    if (r.errorText) throw new Error(`navigate ${url}: ${r.errorText}`)
    await Promise.race([loaded, new Promise((_, err) => setTimeout(() => err(new Error(`load timeout: ${url}`)), timeoutMs))])
  }
  /** Evaluates a JS expression in the page and returns its (JSON) value. */
  async evaluate(expression: string): Promise<any> {
    const r = await this.cdp.send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }, this.sessionId)
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text)
    return r.result.value
  }
  async screenshot(): Promise<Buffer> {
    const r = await this.cdp.send("Page.captureScreenshot", { format: "png" }, this.sessionId)
    return Buffer.from(r.data, "base64")
  }
}

export class Browser {
  private cdp: Cdp
  private proc: ChildProcess
  private dir: string
  constructor(cdp: Cdp, proc: ChildProcess, dir: string) {
    this.cdp = cdp
    this.proc = proc
    this.dir = dir
  }
  async newPage(): Promise<Page> {
    const { targetId } = await this.cdp.send("Target.createTarget", { url: "about:blank" })
    const { sessionId } = await this.cdp.send("Target.attachToTarget", { targetId, flatten: true })
    await this.cdp.send("Page.enable", {}, sessionId)
    return new Page(this.cdp, sessionId)
  }
  async close(): Promise<void> {
    this.cdp.close()
    this.proc.kill("SIGKILL")
    rmSync(this.dir, { recursive: true, force: true })
  }
}

/** Starts headless Chromium (profile in a fresh temp dir) and connects over CDP. */
export async function launch(opts: { args?: string[]; timeoutMs?: number } = {}): Promise<Browser> {
  const dir = mkdtempSync(join(tmpdir(), "clankr-chromium-"))
  const proc = spawn(CHROMIUM, [
    "--headless", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage", "--no-first-run", "--no-default-browser-check",
    "--remote-debugging-port=0", "--remote-allow-origins=*", `--user-data-dir=${dir}`, ...(opts.args ?? []), "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"] })
  const url = await new Promise<string>((ok, err) => {
    let seen = ""
    const t = setTimeout(() => err(new Error(`chromium didn't start: ${seen.slice(-500)}`)), opts.timeoutMs ?? 20000)
    proc.stderr!.on("data", d => {
      seen += d
      const m = seen.match(/DevTools listening on (ws:\/\/\S+)/)
      if (m) {
        clearTimeout(t)
        ok(m[1])
      }
    })
    proc.once("exit", c => err(new Error(`chromium exited (${c}): ${seen.slice(-500)}`)))
  })
  const ws = new WebSocket(url)
  await new Promise((ok, err) => {
    ws.addEventListener("open", ok, { once: true })
    ws.addEventListener("error", () => err(new Error("can't connect to chromium")), { once: true })
  })
  return new Browser(new Cdp(ws), proc, dir)
}
