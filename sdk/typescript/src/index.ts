// Clankr agent SDK (agent runtime ABI v1.1) for Node.js. Frames over the node's Unix socket:
// a 4-byte big-endian length, then that many bytes of JSON. See sdk/python for the reference.

import { AsyncLocalStorage } from "node:async_hooks"
import { connect, type Socket } from "node:net"

export const ABI_MAJOR = 1
export const ABI_MINOR = 2
export const MAX_FRAME = 4 * 1024 * 1024
export const VERSION = "0.1.0"

type Json = null | boolean | number | string | Json[] | { [k: string]: Json }
type Frame = { type: string } & Record<string, any>

/** An error from the node or control plane; `code` is a stable Clankr code (Forbidden, Conflict, ...). */
export class ClankrError extends Error {
  code: string
  constructor(code: string, message: string) {
    super(`${code}: ${message}`)
    this.code = code
  }
}

/** Throw from a handler to fail the task without retrying. */
export class Permanent extends Error {}

export interface Task {
  runId: string
  task: string
  node: string
  attempt: number
  /** Stable across attempts; pass to external systems that deduplicate. */
  idempotencyKey: string
  input: any
}

export interface Generated { text: string; stopReason: string; usage: { inputTokens: number; outputTokens: number }; provider: string; model: string }
export interface ContextObject { key: string; version: number; value: any; writer: string; updatedAt: string }
export interface ToolResult { output: any; outputSha256: string; truncated: boolean; durationMs: number; toolVersion: string; replayed: boolean }

class Conn {
  private buf = Buffer.alloc(0)
  private waiting: ((f: Frame | null) => void)[] = []
  private queue: (Frame | null)[] = []
  private sock: Socket
  constructor(sock: Socket) {
    this.sock = sock
    sock.on("data", d => {
      this.buf = Buffer.concat([this.buf, d])
      while (this.buf.length >= 4) {
        const n = this.buf.readUInt32BE(0)
        if (n > MAX_FRAME) throw new Error(`frame of ${n} bytes exceeds ${MAX_FRAME}`)
        if (this.buf.length < 4 + n) break
        this.push(JSON.parse(this.buf.subarray(4, 4 + n).toString("utf8")))
        this.buf = this.buf.subarray(4 + n)
      }
    })
    sock.on("close", () => this.push(null))
  }
  private push(f: Frame | null) {
    const w = this.waiting.shift()
    if (w) w(f)
    else this.queue.push(f)
  }
  /** The next frame, or null once the node has gone away. */
  recv(): Promise<Frame | null> {
    if (this.queue.length) return Promise.resolve(this.queue.shift()!)
    return new Promise(r => this.waiting.push(r))
  }
  send(f: Frame): Promise<void> {
    const body = Buffer.from(JSON.stringify(f))
    if (body.length > MAX_FRAME) throw new Error(`frame of ${body.length} bytes exceeds ${MAX_FRAME}`)
    const head = Buffer.alloc(4)
    head.writeUInt32BE(body.length)
    return new Promise((ok, err) => this.sock.write(Buffer.concat([head, body]), e => (e ? err(e) : ok())))
  }
}

class Calls {
  private next = 0
  private pending = new Map<number, { ok: (v: any) => void; err: (e: Error) => void }>()
  readonly conn: Conn
  constructor(conn: Conn) {
    this.conn = conn
  }
  call(op: Record<string, Json | undefined>): Promise<any> {
    const callId = ++this.next
    return new Promise((ok, err) => {
      this.pending.set(callId, { ok, err })
      this.conn.send({ type: "call", callId, op }).catch(err)
    })
  }
  resolve(f: Frame) {
    const p = this.pending.get(f.callId)
    if (!p) return
    this.pending.delete(f.callId)
    if (f.error) p.err(new ClankrError(f.error.code ?? "Unknown", f.error.message ?? ""))
    else p.ok(f.ok ?? null)
  }
}

// the task a handler is working on, so ctx.tools can derive stable idempotency keys
const current = new AsyncLocalStorage<{ task: Task; n: number }>()

export class ContextClaim {
  private calls: Calls
  readonly claim: string
  constructor(calls: Calls, claim: string) {
    this.calls = calls
    this.claim = claim
  }
  /** The object, or null if the key doesn't exist. */
  get(key: string): Promise<ContextObject | null> {
    return this.calls.call({ op: "contextGet", claim: this.claim, key })
  }
  /** ifVersion: undefined = overwrite, 0 = create only, N = only if currently N (else ClankrError "Conflict"). */
  put(key: string, value: Json, opts: { ifVersion?: number } = {}): Promise<ContextObject> {
    return this.calls.call({ op: "contextPut", claim: this.claim, key, value, ifVersion: opts.ifVersion })
  }
  list(prefix = ""): Promise<ContextObject[]> {
    return this.calls.call({ op: "contextList", claim: this.claim, prefix })
  }
}

/** A leased mailbox message. At-least-once: ack when done, nack to retry. */
export class Message {
  readonly id: string
  readonly epoch: number
  readonly sender: string
  readonly correlationId?: string
  readonly payload: any
  readonly deliveries: number
  readonly mailbox: string
  /** Set on a service request: the call is waiting for a reply. */
  readonly replyTo?: string
  private calls: Calls
  constructor(calls: Calls, mailbox: string, raw: any) {
    this.calls = calls
    this.mailbox = mailbox
    this.id = raw.id
    this.epoch = raw.leaseEpoch
    this.sender = raw.sender
    this.correlationId = raw.correlationId ?? undefined
    this.payload = raw.payload
    this.deliveries = raw.deliveries
    this.replyTo = raw.replyTo ?? undefined
  }
  async ack(): Promise<void> {
    await this.calls.call({ op: "mailboxAck", mailbox: this.mailbox, id: this.id, epoch: this.epoch })
  }
  async nack(opts: { delaySeconds?: number; reason?: string } = {}): Promise<void> {
    await this.calls.call({ op: "mailboxNack", mailbox: this.mailbox, id: this.id, epoch: this.epoch, delaySeconds: opts.delaySeconds ?? 0, reason: opts.reason ?? "" })
  }
}

export class Mailbox {
  private calls: Calls
  readonly name: string
  constructor(calls: Calls, name: string) {
    this.calls = calls
    this.name = name
  }
  send(payload: Json, opts: { correlationId?: string; dedupKey?: string; ttlSeconds?: number } = {}): Promise<{ message: any; duplicate: boolean }> {
    return this.calls.call({ op: "mailboxSend", mailbox: this.name, payload, ...opts })
  }
  async receive(opts: { max?: number; visibilitySeconds?: number; waitSeconds?: number } = {}): Promise<Message[]> {
    const raw: any[] = await this.calls.call({ op: "mailboxReceive", mailbox: this.name, max: opts.max ?? 10, visibilitySeconds: opts.visibilitySeconds ?? 60, waitSeconds: opts.waitSeconds ?? 0 })
    return raw.map(m => new Message(this.calls, this.name, m))
  }
}

export class Context {
  readonly namespace: string
  readonly agent: string
  readonly instance: string
  readonly models: { generate(claim: string, messages: { role: string; content: string }[], opts?: { system?: string; maxOutputTokens?: number }): Promise<Generated> }
  readonly tools: { call(claim: string, tool: string, method: string, input?: Json, opts?: { idempotencyKey?: string; approval?: string }): Promise<ToolResult> }
  /**
   * AgentServices by name (`clankr://agents/<namespace>/<name>`), listed in the agent's
   * `spec.calls`. `call` is request/reply and throws ClankrError("OutcomeUnknown") on timeout;
   * `send` is one-way.
   */
  readonly services: {
    call(service: string, payload: Json, opts?: { timeoutSeconds?: number; dedupKey?: string }): Promise<any>
    send(service: string, payload: Json, opts?: { dedupKey?: string }): Promise<{ message: any; duplicate: boolean }>
  }

  private calls: Calls
  constructor(calls: Calls, welcome: Frame) {
    this.calls = calls
    this.namespace = welcome.namespace
    this.agent = welcome.agent
    this.instance = welcome.instance
    this.models = {
      generate: (claim, messages, opts = {}) => calls.call({ op: "generate", claim, request: { messages, system: opts.system, maxOutputTokens: opts.maxOutputTokens } }),
    }
    this.tools = {
      call: (claim, tool, method, input = {}, opts = {}) => {
        let key = opts.idempotencyKey
        if (key === undefined) {
          const cur = current.getStore()
          if (!cur) throw new ClankrError("InvalidSpec", "pass idempotencyKey when calling tools outside a task")
          key = `${cur.task.idempotencyKey}:${tool}.${method}:${++cur.n}`
        }
        return calls.call({ op: "toolCall", claim, call: { tool, method, input, idempotencyKey: key, approval: opts.approval } })
      },
    }
    this.services = {
      call: async (service, payload, opts = {}) => (await calls.call({ op: "serviceCall", service, payload, timeoutSeconds: opts.timeoutSeconds ?? 30, dedupKey: opts.dedupKey })).reply,
      send: (service, payload, opts = {}) => new Mailbox(calls, `svc:${service}`).send(payload, opts),
    }
  }

  /** A durable copy into every mailbox subscribed to `topic` (listed in `spec.publishes`). */
  publish(topic: string, payload: Json, opts: { dedupKey?: string } = {}): Promise<{ delivered: number; subscriptions: string[] }> {
    return this.calls.call({ op: "publish", topic, payload, dedupKey: opts.dedupKey })
  }

  context(claim: string): ContextClaim {
    return new ContextClaim(this.calls, claim)
  }
  mailbox(name: string): Mailbox {
    return new Mailbox(this.calls, name)
  }
  log(message: string, level = "info"): Promise<void> {
    return this.calls.conn.send({ type: "log", level, message })
  }
}

export type Handler = (task: Task, ctx: Context) => Promise<Json | void> | Json | void
export type MessageHandler = (msg: Message, ctx: Context) => Promise<Json | void> | Json | void

export class AgentApp {
  private handlers = new Map<string, Handler>()
  /** queue -> handler, for messages pushed into handlers (services and subscriptions) */
  private consumers = new Map<string, MessageHandler>()
  private hibernateHook?: (ctx: Context) => Promise<Json> | Json
  /** State checkpointed before the last hibernation (null on a fresh start), and its version. */
  checkpoint: any = null
  checkpointVersion: number | null = null

  /** Handles a workflow node; "*" catches every node without its own handler. */
  task(node: string, fn: Handler): this {
    this.handlers.set(node, fn)
    return this
  }

  /**
   * Serves AgentService `name` (its spec names this agent). The handler gets each request; what
   * it returns is the reply. Throwing Permanent replies `{error}`; anything else redelivers.
   */
  service(name: string, fn: MessageHandler): this {
    this.consumers.set(`svc:${name}`, fn)
    return this
  }

  /** Handles every message arriving in `mailbox` (in the agent's spec): acked when the handler returns, redelivered when it throws. */
  subscribe(mailbox: string, fn: MessageHandler): this {
    this.consumers.set(mailbox, fn)
    return this
  }

  /** Returns this agent's portable state when its deployment hibernates (after in-flight tasks finish). */
  onHibernate(fn: (ctx: Context) => Promise<Json> | Json): this {
    this.hibernateHook = fn
    return this
  }

  run(): void {
    this.main().catch(e => {
      console.error(e)
      process.exit(1)
    })
  }

  private async main(): Promise<void> {
    const path = process.env.CLANKR_SOCKET
    if (!path) {
      console.error("CLANKR_SOCKET is not set; agents are started by clankr-node")
      process.exit(2)
    }
    const sock = connect(path)
    await new Promise<void>((ok, err) => sock.once("connect", ok).once("error", err))
    const conn = new Conn(sock)
    await conn.send({ type: "hello", abiMajor: ABI_MAJOR, abiMinor: ABI_MINOR, sdk: `typescript/${VERSION}` })
    const welcome = await conn.recv()
    if (!welcome) return
    if (welcome.type === "reject") {
      console.error(`node rejected this agent: ${welcome.reason}`)
      process.exit(1)
    }
    const calls = new Calls(conn)
    const ctx = new Context(calls, welcome)
    this.checkpoint = welcome.checkpoint ?? null
    this.checkpointVersion = welcome.checkpointVersion ?? null
    await conn.send({ type: "ready" })

    const consuming = { on: true }
    for (const [queue, fn] of this.consumers) void this.consume(calls, ctx, queue, fn, consuming)
    const running = new Set<Promise<void>>()
    const track = (p: Promise<void>) => {
      running.add(p)
      p.finally(() => running.delete(p))
    }
    const settle = (graceMs: number) => Promise.race([Promise.allSettled([...running]), new Promise(r => setTimeout(r, graceMs))])
    // frames are read continuously, so replies reach handlers even while we wait on them below
    let stopping: Frame | null = null
    while (!stopping) {
      const f = await conn.recv()
      if (!f) return
      if (f.type === "deliver") track(this.handle(conn, ctx, f))
      else if (f.type === "reply") calls.resolve(f)
      else if (f.type === "shutdown" || f.type === "hibernate") stopping = f
    }
    consuming.on = false
    const pump = (async () => {
      for (let f = await conn.recv(); f; f = await conn.recv()) if (f.type === "reply") calls.resolve(f)
    })()
    if (stopping.type === "shutdown") {
      await settle(stopping.graceMs ?? 5000)
    } else {
      await settle((stopping.graceMs ?? 30000) / 2)
      const state = this.hibernateHook ? await this.hibernateHook(ctx) : null
      await conn.send({ type: "checkpoint", state: state ?? null })
    }
    sock.end()
    void pump
  }

  /** Long-polls `queue`, one message at a time, and runs `fn` on each, in order. */
  private async consume(calls: Calls, ctx: Context, queue: string, fn: MessageHandler, consuming: { on: boolean }): Promise<void> {
    const box = new Mailbox(calls, queue)
    const service = queue.startsWith("svc:") ? queue.slice(4) : null
    const reply = (m: Message, payload: Json | void) => calls.call({ op: "serviceReply", service, requestId: m.id, payload: payload ?? null })
    while (consuming.on) {
      let batch: Message[]
      try {
        batch = await box.receive({ max: 1, visibilitySeconds: 60, waitSeconds: 20 })
      } catch (e) {
        await ctx.log(`receive from ${queue}: ${e}`, "warn")
        await new Promise(r => setTimeout(r, 2000))
        continue
      }
      for (const m of batch) {
        try {
          const result = await fn(m, ctx)
          if (service && m.replyTo) await reply(m, result)
          await m.ack()
        } catch (e) {
          try {
            if (e instanceof Permanent) {
              if (service && m.replyTo) await reply(m, { error: e.message })
              await m.ack()
            } else if (!(e instanceof ClankrError)) {
              await m.nack({ delaySeconds: 1, reason: String(e) })
            } else {
              await ctx.log(`${queue}: ${e.message}`, "warn") // e.g. StaleLease: it went to another instance
            }
          } catch {
            // the lease is gone; the message comes back by itself
          }
        }
      }
    }
  }

  private async handle(conn: Conn, ctx: Context, d: Frame): Promise<void> {
    const task: Task = { runId: d.runId, task: d.task, node: d.node, attempt: d.attempt, idempotencyKey: d.idempotencyKey, input: d.input }
    const fn = this.handlers.get(task.node) ?? this.handlers.get("*")
    try {
      if (!fn) throw new Permanent(`no handler for node ${JSON.stringify(task.node)}`)
      const result = await current.run({ task, n: 0 }, () => fn(task, ctx))
      await conn.send({ type: "complete", deliveryId: d.deliveryId, result: result ?? null })
    } catch (e) {
      const permanent = e instanceof Permanent
      const msg = permanent ? (e as Error).message : `${(e as Error)?.name ?? "Error"}: ${(e as Error)?.message ?? e}`
      await conn.send({ type: "fail", deliveryId: d.deliveryId, error: msg, retryable: !permanent })
    }
  }
}
