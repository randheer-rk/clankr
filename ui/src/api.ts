// Thin client for the clankrd REST API. The bearer token (owner, ServiceAccount or OIDC ID
// token) lives in localStorage; clankrd enforces everything server-side.

export type Json = null | boolean | number | string | Json[] | { [k: string]: Json }

export interface ApiError { code: string; message: string; field?: string }

export interface Principal { kind: 'owner' | 'user' | 'serviceAccount' | 'node'; id?: string; name?: string; namespace?: string; groups?: string[] }

export interface PendingTask { node: string; attempt: number }
export interface RunState {
  status: RunStatus
  data: Record<string, Json>
  steps: number
  visits: Record<string, number>
  tasks: Record<string, PendingTask>
  timers: Record<string, string>
  waits: Record<string, string>
  unknown?: Record<string, PendingTask>
  error?: string
}
export type RunStatus = 'Running' | 'Succeeded' | 'Failed' | 'Cancelled'
export interface GraphNode { type: string; next?: string[]; approverGroup?: string; agentRef?: string; toolRef?: string; then?: string; else?: string; onReject?: string }
export interface Graph { entrypoint: string; nodes: Record<string, GraphNode> }
export interface Run {
  runId: string; namespace: string; workflow: string; workflowGeneration: number; graph: Graph; startedBy?: string
  status: RunStatus; seq: number; state: RunState; createdAt: string; updatedAt: string
  forkedFrom?: string
  forkedAt?: number
}
export interface HistoryEntry { seq: number; at: string; event: { type: string } & Record<string, Json> }
export interface Approval {
  id: string; namespace: string; runId: string; node: string; visit: number; approverGroup: string; requestedBy?: string
  subject: Record<string, Json>; digest: string; state: 'pending' | 'approved' | 'rejected' | 'expired' | 'cancelled'
  deadline?: string; decidedBy?: string; decidedAt?: string; comment?: string; createdAt: string
}
export interface Spend { calls: number; inputTokens: number; outputTokens: number; costUSD: number | null }
export interface Usage {
  namespace: string
  quotas: { resource: string; hard: number; used: number }[]
  budgets: { name: string; period: 'day' | 'month' | 'total'; window: string; tokens?: Meter; costUSD?: Meter }[]
  modelSpendThisMonth: { claim: string; calls: number; inputTokens: number; outputTokens: number; costUSD: number | null }[]
  spendByDay: { day: string; tokens: number; costUSD: number | null }[]
  /** costUSD alerts carry micro-dollars. */
  alerts: { at: string; budget: string; window: string; metric: 'tokens' | 'costUSD'; threshold: number; used: number; limit: number }[]
}
export interface ToolCall { at: string; claim: string; tool: string; toolVersion: string; method: string; state: string; task: string; approval?: string; durationMs?: number; truncated: boolean; output?: Json; outputSha256?: string; error?: string; actor: string }
export interface Meter { limit: number; used: number; reserved: number }
export interface Resource { kind: string; metadata: { name: string; namespace: string; labels?: Record<string, string>; creationTimestamp?: string; resourceVersion?: number }; spec: Record<string, Json>; status?: Record<string, Json> }

const TOKEN = 'clankr.token'
export const getToken = () => { try { return localStorage.getItem(TOKEN) } catch { return null } }
export const setToken = (t: string | null) => { try { t ? localStorage.setItem(TOKEN, t) : localStorage.removeItem(TOKEN) } catch { /* private mode */ } }

export class HttpError extends Error {
  constructor(public status: number, public body: ApiError) { super(body.message) }
}

export async function api<T>(path: string, init: RequestInit & { json?: unknown } = {}): Promise<T> {
  const headers: Record<string, string> = { accept: 'application/json' }
  const token = getToken()
  if (token) headers.authorization = `Bearer ${token}`
  if (init.json !== undefined) headers['content-type'] = 'application/json'
  const r = await fetch(path, { ...init, headers, body: init.json !== undefined ? JSON.stringify(init.json) : init.body })
  if (r.status === 401) window.dispatchEvent(new Event('clankr:unauthorized'))
  if (!r.ok) {
    const body = await r.json().catch(() => ({ code: 'Error', message: `HTTP ${r.status}` }))
    throw new HttpError(r.status, body)
  }
  return (r.status === 204 ? undefined : await r.json()) as T
}

export const ns = (n: string) => `/v1/namespaces/${encodeURIComponent(n)}`
