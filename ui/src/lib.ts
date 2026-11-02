import { useCallback, useEffect, useRef, useState } from 'react'
import type { Graph, HistoryEntry, Json, Run } from './api'

/** Fetches now and every `ms`, keeping the last good value. `reload` refetches immediately. */
export function usePoll<T>(load: () => Promise<T>, deps: unknown[], ms = 4000) {
  const [data, setData] = useState<T | undefined>()
  const [error, setError] = useState<string | undefined>()
  const loadRef = useRef(load)
  loadRef.current = load
  const reload = useCallback(() => {
    loadRef.current().then(d => { setData(d); setError(undefined) }, e => setError(e?.status === 403 ? 'You don’t have access to this in this workspace.' : String(e.message ?? e)))
  }, [])
  useEffect(() => {
    setData(undefined)
    reload()
    const t = setInterval(() => { if (!document.hidden) reload() }, ms)
    return () => clearInterval(t)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)
  return { data, error, reload }
}

export const titleCase = (s: string) => s.replace(/[-_]+/g, ' ').replace(/^\w/, c => c.toUpperCase())

/** "Research report: durability" from workflow + a descriptive input field, when there is one. */
export function runTitle(r: Run) {
  const input = r.state.data.input
  if (input && typeof input === 'object' && !Array.isArray(input)) {
    for (const k of ['title', 'topic', 'name', 'subject']) {
      const v = (input as Record<string, Json>)[k]
      if (typeof v === 'string' && v) return `${titleCase(r.workflow)}: ${v}`
    }
  }
  return titleCase(r.workflow)
}

export const shortId = (id: string) => id.slice(0, 8)

export function ago(iso: string, now = Date.now()) {
  const s = Math.max(0, (now - new Date(iso).getTime()) / 1000)
  if (s < 60) return 'now'
  if (s < 3600) return `${Math.floor(s / 60)}m`
  if (s < 86400) return `${Math.floor(s / 3600)}h`
  return `${Math.floor(s / 86400)}d`
}

export function until(iso: string, now = Date.now()) {
  const s = (new Date(iso).getTime() - now) / 1000
  if (s <= 0) return 'due'
  if (s < 3600) return `${Math.ceil(s / 60)}m`
  if (s < 86400) return `${Math.round(s / 3600)}h`
  return `${Math.round(s / 86400)}d`
}

export function duration(ms: number) {
  const s = Math.round(ms / 1000)
  if (s < 60) return `${s}s`
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`
}

export const runDuration = (r: Run) => duration((r.status === 'Running' ? Date.now() : new Date(r.updatedAt).getTime()) - new Date(r.createdAt).getTime())

export const clock = (iso: string) => new Date(iso).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })

export const isToday = (iso: string) => new Date(iso).toDateString() === new Date().toDateString()

export const money = (usd: number | null | undefined, digits = 2) => (usd == null ? '—' : `$${usd.toFixed(usd > 0 && usd < 0.01 ? 4 : digits)}`)

export const compact = (n: number) => (n >= 1e6 ? `${+(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${+(n / 1e3).toFixed(1)}k` : `${n}`)

export const actorName = (a?: string | null) => {
  if (!a) return 'someone'
  if (a === 'owner') return 'Owner'
  const [kind, rest] = a.split(/:(.*)/s)
  if (kind === 'serviceaccount') return rest.split('/')[1] ?? rest
  if (kind === 'user') return rest
  return rest ?? a
}

export type NodeState = 'done' | 'running' | 'waiting' | 'failed' | 'todo'

/** What each graph node is doing right now, read off the folded run state. */
export function nodeStates(r: Run): Record<string, NodeState> {
  const out: Record<string, NodeState> = {}
  const waiting = new Set(Object.values(r.state.waits))
  const running = new Set(Object.values(r.state.tasks).map(t => t.node))
  for (const id of Object.keys(r.graph.nodes)) {
    if (waiting.has(id)) out[id] = 'waiting'
    else if (running.has(id)) out[id] = 'running'
    else if ((r.state.visits[id] ?? 0) > 0) out[id] = 'done'
    else out[id] = 'todo'
  }
  if (r.status === 'Failed') {
    // the last entered node that didn't produce a result is the one that failed
    const failed = Object.keys(r.state.visits).find(id => out[id] === 'done' && !(id in r.state.data) && r.graph.nodes[id]?.type === 'agent')
    if (failed) out[failed] = 'failed'
  }
  return out
}

export function summary(r: Run): { label: string; tone: 'wait' | 'run' | 'ok' | 'bad' | 'off' } {
  if (r.status === 'Succeeded') return { label: 'Completed', tone: 'ok' }
  if (r.status === 'Failed') return { label: 'Failed', tone: 'bad' }
  if (r.status === 'Cancelled') return { label: 'Cancelled', tone: 'off' }
  const waits = Object.entries(r.state.waits)
  const approval = waits.find(([sig]) => sig.startsWith('approve.'))
  if (approval) return { label: `Waiting for ${titleCase(approval[1]).toLowerCase()}`, tone: 'wait' }
  if (waits.length) return { label: `Waiting on ${waits[0][0]}`, tone: 'wait' }
  const nodes = [...new Set(Object.values(r.state.tasks).map(t => t.node))]
  return { label: nodes.length ? `Running ${nodes.map(n => titleCase(n).toLowerCase()).join(', ')}` : 'Running', tone: 'run' }
}

/** Retries per node, counted from failed task attempts in the history. */
export function retries(history: HistoryEntry[] | undefined) {
  const out: Record<string, number> = {}
  for (const h of history ?? []) {
    if (h.event.type === 'TaskFailed') {
      const node = String(h.event.task).split('#')[0]
      out[node] = (out[node] ?? 0) + 1
    }
  }
  return out
}

/** Graph layout: column = longest path from the entrypoint, rows centred per column. */
export function layout(g: Graph) {
  const edges: [string, string][] = []
  for (const [id, n] of Object.entries(g.nodes)) {
    for (const t of [...(n.next ?? []), n.then, n.else, n.onReject]) if (t && g.nodes[t]) edges.push([id, t])
  }
  const col: Record<string, number> = { [g.entrypoint]: 0 }
  // longest-path layering, bounded so loops can't spin forever
  for (let i = 0; i < Object.keys(g.nodes).length; i++) {
    let changed = false
    for (const [a, b] of edges) {
      if (col[a] !== undefined && (col[b] === undefined || col[b] < col[a] + 1) && col[a] + 1 < Object.keys(g.nodes).length) {
        col[b] = col[a] + 1
        changed = true
      }
    }
    if (!changed) break
  }
  for (const id of Object.keys(g.nodes)) col[id] ??= 0
  const byCol: string[][] = []
  for (const [id, c] of Object.entries(col)) (byCol[c] ??= []).push(id)
  const rows = Math.max(...byCol.map(c => c?.length ?? 0))
  const pos: Record<string, { col: number; row: number }> = {}
  byCol.forEach((ids, c) => ids?.forEach((id, i) => { pos[id] = { col: c, row: i + (rows - ids.length) / 2 } }))
  return { pos, edges, cols: byCol.length, rows }
}

/** Human sentence for a history event, or null for bookkeeping ones. */
export function describe(e: HistoryEntry['event']): { text: string; detail?: string; tone: 'ok' | 'wait' | 'run' | 'bad' | 'off' } | null {
  const node = (t: Json) => titleCase(String(t).split('#')[0])
  switch (e.type) {
    case 'RunStarted': return { text: 'Run started', detail: e.input && typeof e.input === 'object' ? JSON.stringify(e.input) : undefined, tone: 'run' }
    case 'TaskCompleted': return { text: `${node(e.task)} finished`, tone: 'ok' }
    case 'TaskFailed': return { text: `${node(e.task)} failed${e.retryable ? ', will retry' : ''}`, detail: String(e.error), tone: 'bad' }
    case 'TaskOutcomeUnknown': return { text: `${node(e.task)} may or may not have finished`, detail: 'Its worker went away mid-task; needs a decision', tone: 'wait' }
    case 'WaitStarted': return { text: String(e.signal).startsWith('approve.') ? `Paused for ${titleCase(String(e.node)).toLowerCase()}` : `Waiting for signal ${e.signal}`, tone: 'wait' }
    case 'SignalReceived': {
      const p = (e.payload ?? {}) as Record<string, Json>
      if (String(e.signal).startsWith('approve.')) {
        const by = actorName(p.by as string)
        return p.approved ? { text: `Approved by ${by}`, detail: (p.comment as string) || undefined, tone: 'ok' } : { text: p.reason ? 'Approval timed out' : `Sent back by ${by}`, detail: (p.comment as string) || undefined, tone: 'bad' }
      }
      return { text: `Signal ${e.signal}`, tone: 'run' }
    }
    case 'TimerFired': return { text: 'Timer fired', tone: 'run' }
    case 'RunCompleted': return { text: 'Run completed', tone: 'ok' }
    case 'RunFailed': return { text: 'Run failed', detail: String(e.reason), tone: 'bad' }
    case 'RunCancelled': return { text: 'Run cancelled', detail: String(e.reason), tone: 'off' }
    case 'RunForked': return { text: `Forked from run ${String(e.from).slice(0, 8)} at #${e.at}`, detail: e.patch && Object.keys(e.patch).length ? `Changed: ${Object.keys(e.patch).join(', ')}` : undefined, tone: 'run' }
    default: return null
  }
}

export const tone = {
  ok: { bg: '#E3F9E8', fg: '#1E9E44' },
  wait: { bg: '#FFF4E0', fg: '#FF9F0A' },
  run: { bg: '#E5F1FF', fg: '#0A84FF' },
  bad: { bg: '#FFE9E7', fg: '#FF453A' },
  off: { bg: '#F2F2F7', fg: '#8E8E93' },
}

/** Short readable preview of run data for approvals: long strings as prose, the rest as facts. */
export function subjectParts(subject: Record<string, Json>) {
  const prose: { key: string; text: string }[] = []
  const facts: { key: string; value: string }[] = []
  const walk = (v: Json, key: string, depth: number) => {
    if (typeof v === 'string') (v.length > 80 ? prose : facts).push(v.length > 80 ? { key, text: v } as never : { key, value: v } as never)
    else if (typeof v === 'number' || typeof v === 'boolean') facts.push({ key, value: String(v) })
    else if (Array.isArray(v)) facts.push({ key, value: `${v.length} item${v.length === 1 ? '' : 's'}` })
    else if (v && depth < 2) for (const [k, x] of Object.entries(v)) walk(x, key ? `${key}.${k}` : k, depth + 1)
  }
  for (const [k, v] of Object.entries(subject)) if (k !== 'input') walk(v, k, 0)
  return { prose, facts }
}
