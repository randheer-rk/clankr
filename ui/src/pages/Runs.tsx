import { useMemo, useState } from 'react'
import { Ban, Check, CircleSlash, Clock, Coins, GitBranch, Hand, LoaderCircle, RefreshCw, Search, Workflow, X } from 'lucide-react'
import { api, ns, type Approval, type HistoryEntry, type Run, type Spend, type Usage } from '../api'
import { useApp } from '../App'
import { Flow } from '../Flow'
import { actorName, ago, clock, compact, describe, isToday, money, retries, runDuration, runTitle, shortId, subjectParts, summary, titleCase, tone, until, usePoll } from '../lib'

const statusIcon = (r: Run, needsMe: boolean) => {
  const s = summary(r)
  const t = needsMe ? tone.wait : tone[s.tone]
  const icon = needsMe || s.tone === 'wait' ? <Hand /> : s.tone === 'run' ? <LoaderCircle /> : s.tone === 'ok' ? <Check /> : s.tone === 'bad' ? <X /> : <CircleSlash />
  return <span className="st" style={{ background: t.bg, color: t.fg }}>{icon}</span>
}

export function Runs({ id, go, runs, error, reload }: { id?: string; go: (to: string) => void; runs?: Run[]; error?: string; reload: () => void }) {
  const { mine } = useApp()
  const [q, setQ] = useState('')
  const needsMe = useMemo(() => new Set(mine.map(a => a.runId)), [mine])
  const shown = (runs ?? []).filter(r => !q || `${runTitle(r)} ${r.runId}`.toLowerCase().includes(q.toLowerCase()))
  const groups: [string, Run[]][] = [
    ['Needs you', shown.filter(r => needsMe.has(r.runId))],
    ['Running', shown.filter(r => r.status === 'Running' && !needsMe.has(r.runId))],
    ['Today', shown.filter(r => r.status !== 'Running' && isToday(r.updatedAt))],
    ['Earlier', shown.filter(r => r.status !== 'Running' && !isToday(r.updatedAt))],
  ]
  const selected = id ?? shown[0]?.runId
  return (
    <>
      <section className="list">
        <div className="bar"><h2>Runs</h2><span className="sp" /><button className="ibtn" title="Refresh" onClick={reload}><RefreshCw /></button></div>
        <label className="search"><Search /><input value={q} onChange={e => setQ(e.target.value)} placeholder="Search runs" /></label>
        <div className="scroll">
          {!runs && <div className="empty">{error ?? 'Loading…'}</div>}
          {runs?.length === 0 && <div className="empty">No runs yet. Start one from Workflows.</div>}
          {groups.map(([label, rs]) => rs.length > 0 && (
            <div key={label}>
              <div className="sec">{label}</div>
              {rs.map(r => (
                <button key={r.runId} className={`row ${r.runId === selected ? 'on' : ''}`} onClick={() => go(`/runs/${r.runId}`)}>
                  {statusIcon(r, needsMe.has(r.runId))}
                  <div style={{ minWidth: 0 }}><b>{runTitle(r)}</b><div className="s">{needsMe.has(r.runId) ? 'Waiting for your review' : summary(r).label}</div></div>
                  <span className="t">{ago(r.status === 'Running' ? r.createdAt : r.updatedAt)}</span>
                </button>
              ))}
            </div>
          ))}
        </div>
      </section>
      {selected ? <RunDetail key={selected} runId={selected} onChange={reload} /> : <section className="detail"><div className="empty">Select a run</div></section>}
    </>
  )
}

function RunDetail({ runId, onChange }: { runId: string; onChange: () => void }) {
  const { ns: n, refresh, mine } = useApp()
  const [tab, setTab] = useState<'overview' | 'history' | 'data' | 'cost'>('overview')
  const run = usePoll(() => api<Run>(`${ns(n)}/runs/${runId}`), [n, runId], 3000)
  const history = usePoll(() => api<HistoryEntry[]>(`${ns(n)}/runs/${runId}/history`), [n, runId], 3000)
  const spend = usePoll(() => api<Spend>(`${ns(n)}/runs/${runId}/spend`), [n, runId], 10000)
  const approvals = usePoll(() => api<Approval[]>(`${ns(n)}/approvals?state=pending`), [n, runId], 3000)
  const r = run.data
  if (!r) return <section className="detail"><div className="empty">{run.error ?? 'Loading…'}</div></section>
  const s = summary(r)
  const pending = approvals.data?.find(a => a.runId === r.runId)
  const done = () => { run.reload(); history.reload(); approvals.reload(); refresh(); onChange() }
  const cancel = async () => {
    if (!confirm('Cancel this run? Pending work is abandoned.')) return
    await api(`${ns(n)}/runs/${r.runId}/cancel`, { method: 'POST', json: {} }).catch(e => alert(e.message))
    done()
  }
  return (
    <section className="detail">
      <div className="tb">
        <div className="seg">
          {(['overview', 'history', 'data', 'cost'] as const).map(t => <button key={t} className={tab === t ? 'on' : ''} onClick={() => setTab(t)}>{titleCase(t)}</button>)}
        </div>
        {r.status === 'Running' && <button className="ibtn" title="Cancel run" onClick={cancel}><Ban /></button>}
      </div>
      <div className="body">
        <div className="crumb"><Workflow style={{ width: 14, height: 14, color: 'var(--ink-3)' }} />{r.workflow} <span className="faint">· run {shortId(r.runId)} · generation {r.workflowGeneration}</span></div>
        <h1>{runTitle(r)}</h1>
        <div className="meta">
          <span className={`pill ${s.tone}`}>{s.label}</span>
          <span><Clock />{runDuration(r)}</span>
          <span><Coins />{spend.data?.calls ? `${money(spend.data.costUSD)} · ${compact(spend.data.inputTokens + spend.data.outputTokens)} tokens` : 'No model spend yet'}</span>
          <span><GitBranch />Started by {actorName(r.startedBy)}</span>
        </div>
        {r.state.error && <div className="err">{r.state.error}</div>}
        {pending && <ApprovalBanner a={pending} onDone={done} />}

        {tab === 'overview' && (
          <>
            <div className="card" style={{ marginTop: 16 }}>
              <h3>Flow<span>{Object.keys(r.state.visits).length} of {Object.keys(r.graph.nodes).length} steps reached</span></h3>
              <Flow run={r} retries={retries(history.data)} you={!!pending && mine.some(a => a.id === pending.id)} />
            </div>
            <div className="grid2">
              <div className="card">
                <h3>Activity<button className="faint" onClick={() => setTab('history')}>See all</button></h3>
                <Activity history={history.data} limit={5} />
              </div>
              <BudgetCard />
            </div>
          </>
        )}
        {tab === 'history' && <div className="card" style={{ marginTop: 16 }}><h3>History<span>{history.data?.length ?? 0} events · append-only</span></h3><Activity history={history.data} raw /></div>}
        {tab === 'data' && <div style={{ marginTop: 16 }}><pre className="json">{JSON.stringify(r.state.data, null, 2)}</pre></div>}
        {tab === 'cost' && (
          <div className="grid2">
            <div className="card"><h3>Model spend<span>this run</span></h3><div className="kv"><b>{money(spend.data?.costUSD)}</b><b style={{ textAlign: 'right' }}>{spend.data?.calls ?? 0} <small>calls</small></b></div></div>
            <div className="card"><h3>Tokens<span>in / out</span></h3><div className="kv"><b>{compact(spend.data?.inputTokens ?? 0)} <small>in</small></b><b style={{ textAlign: 'right' }}>{compact(spend.data?.outputTokens ?? 0)} <small>out</small></b></div></div>
          </div>
        )}
      </div>
    </section>
  )
}

export function Activity({ history, limit, raw }: { history?: HistoryEntry[]; limit?: number; raw?: boolean }) {
  if (!history) return <div className="empty">Loading…</div>
  const items = [...history].reverse().map(h => ({ h, d: describe(h.event) })).filter(x => raw || x.d)
  return (
    <div style={{ marginTop: 6 }}>
      {items.slice(0, limit).map(({ h, d }) => {
        const t = tone[d?.tone ?? 'off']
        return (
          <div className="act" key={h.seq}>
            <span className="ai" style={{ background: t.bg, color: t.fg }}>{d?.tone === 'ok' ? <Check /> : d?.tone === 'bad' ? <X /> : d?.tone === 'wait' ? <Hand /> : <LoaderCircle />}</span>
            <p>{d?.text ?? h.event.type}{raw && <span className="faint mono" style={{ marginLeft: 8 }}>#{h.seq} {h.event.type}</span>}{d?.detail && <small>{d.detail}</small>}</p>
            <time>{clock(h.at)}</time>
          </div>
        )
      })}
    </div>
  )
}

function BudgetCard() {
  const { ns: n } = useApp()
  const u = usePoll(() => api<Usage>(`${ns(n)}/usage`), [n], 15000)
  const b = u.data?.budgets.find(x => x.costUSD || x.tokens)
  return (
    <div className="card">
      <h3>{titleCase(n)} budget<span>{b ? (b.period === 'day' ? 'Today' : b.period === 'month' ? 'This month' : 'All time') : ''}</span></h3>
      {!b ? <p className="faint" style={{ marginTop: 10 }}>No budget set for this workspace.</p> : (
        <>
          <div className="kv">
            {b.costUSD && <b>{money(b.costUSD.used)} <small>of {money(b.costUSD.limit, 0)}</small></b>}
            {b.tokens && <b style={{ textAlign: 'right' }}>{compact(b.tokens.used)} <small>of {compact(b.tokens.limit)} tokens</small></b>}
          </div>
          <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 14 }}>
            {b.costUSD && <div className="meter"><i style={{ width: `${Math.min(100, (100 * b.costUSD.used) / Math.max(b.costUSD.limit, 1e-9))}%`, background: 'var(--blue)' }} /></div>}
            {b.tokens && <div className="meter"><i style={{ width: `${Math.min(100, (100 * b.tokens.used) / Math.max(b.tokens.limit, 1))}%`, background: 'var(--orange)' }} /></div>}
          </div>
        </>
      )}
    </div>
  )
}

/** Approve / send back, bound to the digest of the request the person is looking at. */
export function useDecide(a: Approval, onDone: () => void) {
  const { ns: n } = useApp()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const decide = async (approve: boolean, comment = '') => {
    setBusy(true); setError(undefined)
    try {
      await api(`${ns(n)}/approvals/${a.id}/${approve ? 'approve' : 'reject'}`, { method: 'POST', json: { digest: a.digest, comment } })
      onDone()
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }
  return { decide, busy, error }
}

function ApprovalBanner({ a, onDone }: { a: Approval; onDone: () => void }) {
  const { mine } = useApp()
  const canDecide = mine.some(x => x.id === a.id)
  const { decide, busy, error } = useDecide(a, onDone)
  const { prose, facts } = subjectParts(a.subject)
  return (
    <div className={`ask ${canDecide ? '' : 'muted'}`}>
      <div className="ic"><Hand /></div>
      <div style={{ minWidth: 0 }}>
        <b>{canDecide ? `Approve ${titleCase(a.node).toLowerCase()}?` : `Waiting for ${a.approverGroup}`}</b>
        <p>{a.approverGroup} decide{a.deadline ? ` · closes in ${until(a.deadline)}` : ''} · {canDecide ? 'your approval covers exactly this version' : 'not yours to decide (other group, or you started the run)'}</p>
        {prose[0] && <p className="quote">“{prose[0].text.length > 280 ? `${prose[0].text.slice(0, 280)}…` : prose[0].text}”</p>}
        {facts.length > 0 && <div className="chips">{facts.slice(0, 5).map(f => <span key={f.key} className={f.value === 'false' ? 'warn' : ''}>{f.key}: {f.value}</span>)}</div>}
        {error && <div className="err">{error}</div>}
      </div>
      {canDecide && <div className="btns"><button className="b sec" disabled={busy} onClick={() => decide(false)}>Send back</button><button className="b pri" disabled={busy} onClick={() => decide(true)}>Approve</button></div>}
    </div>
  )
}
