import { useState } from 'react'
import { Check, Clock, FileText, GitBranch, Hand, Search, TimerOff, Undo2, Users, Workflow, X } from 'lucide-react'
import { api, ns, type Approval, type Run } from '../api'
import { useApp } from '../App'
import { actorName, ago, clock, layout, nodeStates, runTitle, shortId, subjectParts, titleCase, tone, until, usePoll } from '../lib'
import { useDecide } from './Runs'

const decidedIcon = (a: Approval) =>
  a.state === 'approved' ? { t: tone.ok, i: <Check /> } : a.state === 'rejected' ? { t: tone.off, i: <Undo2 /> } : a.state === 'expired' ? { t: tone.off, i: <TimerOff /> } : { t: tone.off, i: <X /> }

export function Inbox({ id, go }: { id?: string; go: (to: string) => void }) {
  const { ns: n, mine } = useApp()
  const [q, setQ] = useState('')
  const all = usePoll(() => api<Approval[]>(`${ns(n)}/approvals?limit=200`), [n], 5000)
  const mineIds = new Set(mine.map(a => a.id))
  const match = (a: Approval) => !q || `${a.node} ${a.runId} ${a.approverGroup}`.toLowerCase().includes(q.toLowerCase())
  const list = (all.data ?? []).filter(match)
  const waitingMe = list.filter(a => a.state === 'pending' && mineIds.has(a.id))
  const waitingOthers = list.filter(a => a.state === 'pending' && !mineIds.has(a.id))
  const decided = list.filter(a => a.state !== 'pending').slice(0, 30)
  const selected = list.find(a => a.id === id) ?? waitingMe[0] ?? waitingOthers[0] ?? decided[0]

  const row = (a: Approval) => {
    const pending = a.state === 'pending'
    const d = decidedIcon(a)
    return (
      <button key={a.id} className={`row ${a.id === selected?.id ? 'on' : ''}`} onClick={() => go(`/inbox/${a.id}`)}>
        <span className="st" style={pending ? { background: tone.wait.bg, color: tone.wait.fg } : { background: d.t.bg, color: d.t.fg }}>{pending ? <Hand /> : d.i}</span>
        <div style={{ minWidth: 0 }}>
          <b>{titleCase(a.node)} · run {shortId(a.runId)}</b>
          <div className="s">{pending ? `${a.approverGroup} · requested by ${actorName(a.requestedBy)}` : `${titleCase(a.state)}${a.decidedBy ? ` by ${actorName(a.decidedBy)}` : ''}${a.comment ? ` · “${a.comment}”` : ''}`}</div>
        </div>
        {pending ? <span className="due">{a.deadline ? until(a.deadline) : ''}</span> : <span className="t">{ago(a.decidedAt ?? a.createdAt)}</span>}
      </button>
    )
  }

  return (
    <>
      <section className="list">
        <div className="bar"><h2>Inbox</h2></div>
        <label className="search"><Search /><input value={q} onChange={e => setQ(e.target.value)} placeholder="Search approvals" /></label>
        <div className="scroll">
          {!all.data && <div className="empty">{all.error ?? 'Loading…'}</div>}
          {all.data && waitingMe.length === 0 && <div className="empty" style={{ padding: '24px 20px' }}>Nothing waiting for you.</div>}
          {waitingMe.length > 0 && <><div className="sec">Waiting for you · {waitingMe.length}</div>{waitingMe.map(row)}</>}
          {waitingOthers.length > 0 && <><div className="sec">Waiting for others</div>{waitingOthers.map(row)}</>}
          {decided.length > 0 && <><div className="sec">Decided</div>{decided.map(row)}</>}
        </div>
      </section>
      {selected ? <ApprovalDetail key={selected.id} a={selected} canDecide={mineIds.has(selected.id)} onDone={all.reload} go={go} /> : <section className="detail"><div className="empty">Nothing to review</div></section>}
    </>
  )
}

function ApprovalDetail({ a, canDecide, onDone, go }: { a: Approval; canDecide: boolean; onDone: () => void; go: (to: string) => void }) {
  const { ns: n, refresh } = useApp()
  const [note, setNote] = useState('')
  const run = usePoll(() => api<Run>(`${ns(n)}/runs/${a.runId}`), [n, a.runId], 5000)
  const { decide, busy, error } = useDecide(a, () => { onDone(); refresh() })
  const { prose, facts } = subjectParts(a.subject)
  const pending = a.state === 'pending'
  const r = run.data
  const order = r ? (() => { const { pos } = layout(r.graph); return Object.keys(pos).sort((x, y) => pos[x].col - pos[y].col || pos[x].row - pos[y].row) })() : []
  const st = r ? nodeStates(r) : {}
  return (
    <section className="detail">
      <div className="tb"><span className="title">{pending ? 'Approval' : `Approval · ${a.state}`}</span><span className="sp" /><button className="b sec" onClick={() => go(`/runs/${a.runId}`)}>Open run</button></div>
      <div className="body">
        <div className="split">
          <div style={{ minWidth: 0 }}>
            {pending
              ? <div className="eyebrow"><Clock />{a.deadline ? `Closes ${new Date(a.deadline).toLocaleString([], { weekday: 'long', hour: 'numeric', minute: '2-digit' })} · then it’s sent back automatically` : 'No deadline'}</div>
              : <div className="eyebrow" style={{ color: 'var(--ink-2)' }}>{titleCase(a.state)}{a.decidedBy ? ` by ${actorName(a.decidedBy)}` : ''}{a.decidedAt ? ` at ${clock(a.decidedAt)}` : ''}</div>}
            <h1 style={{ marginTop: 8 }}>{r ? `${titleCase(a.node)}: ${runTitle(r)}` : titleCase(a.node)}</h1>
            <div className="meta">
              <span><Workflow />{r?.workflow ?? '…'}</span>
              <span><GitBranch />Started by {actorName(a.requestedBy)}</span>
              <span><Users />{a.approverGroup} decide</span>
            </div>
            {a.comment && <p className="quote">“{a.comment}” <span className="faint">— {actorName(a.decidedBy)}</span></p>}

            <div className="doc">
              <div className="dh"><FileText style={{ width: 15, height: 15 }} />What you’re approving<span className="sp faint" style={{ fontWeight: 400 }}>visit {a.visit} · digest {a.digest.slice(0, 8)}</span></div>
              <div className="db">
                {prose.length === 0 && facts.length === 0 && <p className="faint">The run produced no data before this step.</p>}
                {prose.map(p => <div key={p.key}><h4>{p.key}</h4><p>{p.text}</p></div>)}
                {facts.length > 0 && <div className="chips" style={{ marginTop: prose.length ? 14 : 0 }}>{facts.map(f => <span key={f.key} className={f.value === 'false' ? 'warn' : ''}>{f.key}: {f.value}</span>)}</div>}
              </div>
            </div>
            {error && <div className="err">{error}</div>}
          </div>

          <div>
            <div className="card">
              <h3>Where the run is<span>{r ? `${Object.keys(r.state.visits).length} of ${order.length}` : ''}</span></h3>
              <div className="mini" style={{ marginTop: 10 }}>
                {order.map(id => {
                  const s = st[id]
                  const t = s === 'done' ? tone.ok : s === 'waiting' ? tone.wait : s === 'running' ? tone.run : s === 'failed' ? tone.bad : tone.off
                  return <div key={id} className={`s ${s === 'waiting' ? 'now' : ''}`}><span className="d" style={{ background: s === 'todo' ? '#D1D1D6' : t.fg }}>{s === 'waiting' ? <Hand /> : s === 'todo' ? null : <Check />}</span>{titleCase(id)}{id === a.node && canDecide ? ' · you' : ''}</div>
                })}
              </div>
            </div>
            <div className="card" style={{ marginTop: 12 }}>
              <h3>Who can decide</h3>
              <p className="muted" style={{ marginTop: 8 }}>Members of <b>{a.approverGroup}</b> with the approver role, except whoever started the run.</p>
              <p style={{ marginTop: 8, fontWeight: 500, color: canDecide ? 'var(--green-ink)' : 'var(--ink-2)' }}>{canDecide ? 'You can decide this.' : pending ? 'You can’t decide this one.' : 'Already decided.'}</p>
            </div>
          </div>
        </div>
      </div>
      {pending && canDecide && (
        <form className="actionbar" onSubmit={e => { e.preventDefault(); decide(true, note) }}>
          <input className="note" value={note} onChange={e => setNote(e.target.value)} placeholder="Add a note to your decision…" />
          <button type="button" className="b sec lg" disabled={busy} onClick={() => decide(false, note)}>Send back</button>
          <button className="b pri lg" disabled={busy}>Approve</button>
        </form>
      )}
    </section>
  )
}
