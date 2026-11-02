import { useState } from 'react'
import { Search, ShieldAlert, ShieldCheck } from 'lucide-react'
import { api } from '../api'
import { actorName, clock, usePoll } from '../lib'

interface AuditEvent { seq: number; at: string; actor: string; action: string; kind?: string; namespace?: string; name?: string; decision: string; reason?: string; hash: string }

/** The hash-chained audit log (cluster-wide; needs cluster-admin). Newest first. */
export function Audit() {
  const [q, setQ] = useState('')
  const [denied, setDenied] = useState(false)
  const log = usePoll(() => api<AuditEvent[]>('/v1/audit?latest=true&limit=1000'), [], 6000)
  const rows = (log.data ?? []).filter(e => (!denied || e.decision === 'deny') && (!q || `${e.actor} ${e.action} ${e.kind ?? ''} ${e.namespace ?? ''} ${e.name ?? ''} ${e.reason ?? ''}`.toLowerCase().includes(q.toLowerCase())))
  return (
    <section className="detail">
      <div className="tb">
        <span className="title">Audit log</span>
        <span className="sp" />
        <label className="search" style={{ margin: 0, width: 280 }}><Search /><input value={q} onChange={e => setQ(e.target.value)} placeholder="Filter by actor, action, object" /></label>
        <div className="seg" style={{ margin: 0 }}><button className={!denied ? 'on' : ''} onClick={() => setDenied(false)}>All</button><button className={denied ? 'on' : ''} onClick={() => setDenied(true)}>Denied</button></div>
      </div>
      <div className="body">
        {log.error && <div className="err" style={{ marginTop: 0 }}>{log.error.includes('access') ? 'The audit log is cluster-wide: it needs cluster-admin (a binding in clankr-system).' : log.error}</div>}
        <p className="faint" style={{ marginBottom: 10 }}>Every change, decision and denial, in a hash chain that clankrd verifies. {rows.length} of {log.data?.length ?? 0} records shown.</p>
        <table className="t">
          <thead><tr><th style={{ width: 60 }}>#</th><th style={{ width: 90 }}>Time</th><th style={{ width: 200 }}>Who</th><th>What</th><th style={{ width: 220 }}>On</th></tr></thead>
          <tbody>
            {rows.slice(0, 400).map(e => (
              <tr key={e.seq}>
                <td className="mono faint">{e.seq}</td>
                <td className="faint">{clock(e.at)}</td>
                <td>{actorName(e.actor)}<div className="faint" style={{ fontSize: 11.5 }}>{e.actor.split(':')[0]}</div></td>
                <td>
                  <span style={{ display: 'inline-flex', gap: 6, alignItems: 'center' }}>
                    {e.decision === 'deny' ? <ShieldAlert style={{ width: 14, height: 14, color: 'var(--red)' }} /> : <ShieldCheck style={{ width: 14, height: 14, color: 'var(--green-ink)' }} />}
                    <b style={{ fontWeight: 500 }}>{e.action}</b>
                  </span>
                  {e.reason && <div className="faint" style={{ fontSize: 12 }}>{e.reason}</div>}
                </td>
                <td className="faint">{e.kind ? `${e.kind} ${e.namespace ? `${e.namespace}/` : ''}${e.name ?? ''}` : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  )
}
