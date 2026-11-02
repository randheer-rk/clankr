import { useState } from 'react'
import { Camera, Database, History, RotateCcw } from 'lucide-react'
import { api, ns, type Resource } from '../api'
import { useApp } from '../App'
import { actorName, ago, clock, titleCase, usePoll } from '../lib'

interface Obj { key: string; version: number; value: unknown; writer: string; updatedAt: string }
interface Snapshot { name: string; seq: number; createdBy: string; createdAt: string }

/** Context claims: what agents remember, every version kept. Snapshot it, look back, restore. */
export function Memory() {
  const { ns: n } = useApp()
  const claims = usePoll(() => api<{ items: Resource[] }>(`${ns(n)}/contextclaims`), [n], 15000)
  const [picked, setPicked] = useState<string>()
  const claim = picked ?? claims.data?.items[0]?.metadata.name
  const [asOf, setAsOf] = useState<string>()
  const base = `${ns(n)}/contexts/${claim}`
  const keys = usePoll(() => (claim ? api<Obj[]>(`${base}/keys?limit=500${asOf ? `&snapshot=${encodeURIComponent(asOf)}` : ''}`) : Promise.resolve([] as Obj[])), [n, claim, asOf], 8000)
  const snaps = usePoll(() => (claim ? api<Snapshot[]>(`${base}/snapshots`) : Promise.resolve([] as Snapshot[])), [n, claim], 15000)
  const [name, setName] = useState('')
  const [note, setNote] = useState<{ ok?: string; err?: string }>({})

  const act = async (f: () => Promise<string>) => {
    setNote({})
    try { setNote({ ok: await f() }); snaps.reload(); keys.reload() } catch (e) { setNote({ err: (e as Error).message }) }
  }
  const take = () => act(async () => { await api(`${base}/snapshots`, { method: 'POST', json: { name } }); setName(''); return `Snapshot ${name} taken` })
  const restore = (s: string) => {
    if (!confirm(`Put ${claim} back the way snapshot ${s} recorded it? Keys written since are rewritten or deleted (agents holding old versions get a conflict).`)) return
    act(async () => { const r = await api<{ written: number; deleted: number }>(`${base}/snapshots/${encodeURIComponent(s)}/restore`, { method: 'POST' }); setAsOf(undefined); return `Restored: ${r.written} keys rewritten, ${r.deleted} deleted` })
  }

  return (
    <>
      <section className="list">
        <div className="bar"><h2>Memory</h2></div>
        <div className="scroll">
          {!claims.data && <div className="empty">{claims.error ?? 'Loading…'}</div>}
          {claims.data?.items.length === 0 && <div className="empty">No ContextClaims in this workspace.</div>}
          {claims.data?.items.map(c => (
            <button key={c.metadata.name} className={`row ${c.metadata.name === claim ? 'on' : ''}`} onClick={() => { setPicked(c.metadata.name); setAsOf(undefined) }}>
              <span className="st" style={{ background: '#F1EBFF', color: '#8E4EC6' }}><Database /></span>
              <div style={{ minWidth: 0 }}><b>{titleCase(c.metadata.name)}</b><div className="s">{c.metadata.name}</div></div>
            </button>
          ))}
        </div>
      </section>
      <section className="detail">
        <div className="tb"><span className="title">{claim ? titleCase(claim) : 'Memory'}</span><span className="sp" />{asOf && <button className="b sec" onClick={() => setAsOf(undefined)}>Back to now</button>}</div>
        {claim && (
          <div className="body">
            {note.err && <div className="err" style={{ marginTop: 0 }}>{note.err}</div>}
            {note.ok && <div className="ask" style={{ gridTemplateColumns: '1fr', padding: 12, marginTop: 0 }}><b style={{ fontSize: 13 }}>{note.ok}</b></div>}
            <div className="grid2" style={{ gridTemplateColumns: '1.4fr 1fr', marginTop: note.ok || note.err ? 14 : 0 }}>
              <div className="card">
                <h3>{asOf ? `As of snapshot ${asOf}` : 'Keys now'}<span>{keys.data?.length ?? 0} keys</span></h3>
                {keys.error && <p className="err">{keys.error}</p>}
                <table className="t"><tbody>
                  {keys.data?.map(o => (
                    <tr key={o.key}>
                      <td style={{ width: '32%' }}><b style={{ fontWeight: 500 }}>{o.key}</b><div className="faint" style={{ fontSize: 12 }}>v{o.version} · {actorName(o.writer)} · {ago(o.updatedAt)}</div></td>
                      <td><pre className="mono" style={{ margin: 0, whiteSpace: 'pre-wrap', wordBreak: 'break-word', fontSize: 12, maxHeight: 120, overflow: 'auto' }}>{JSON.stringify(o.value, null, 1)}</pre></td>
                    </tr>
                  ))}
                </tbody></table>
                {keys.data?.length === 0 && <p className="muted" style={{ marginTop: 8 }}>Empty{asOf ? ' at that snapshot' : ''}.</p>}
              </div>
              <div className="card" style={{ alignSelf: 'start' }}>
                <h3><span style={{ display: 'flex', gap: 8, alignItems: 'center', color: 'var(--ink)' }}><History style={{ color: 'var(--blue)' }} />Snapshots</span><span>every write is kept</span></h3>
                <div style={{ display: 'flex', gap: 8, marginTop: 10 }}>
                  <input className="note" style={{ height: 34 }} value={name} onChange={e => setName(e.target.value.replace(/\s/g, '-'))} placeholder="before-migration" />
                  <button className="b pri" disabled={!name} onClick={take}><Camera />Snapshot</button>
                </div>
                <table className="t"><tbody>
                  {snaps.data?.map(s => (
                    <tr key={s.name} className={asOf === s.name ? 'on' : ''}>
                      <td><button className="faint" style={{ color: 'var(--ink)', fontWeight: 500 }} onClick={() => setAsOf(s.name)}>{s.name}</button><div className="faint" style={{ fontSize: 12 }}>{clock(s.createdAt)} · {actorName(s.createdBy)}</div></td>
                      <td style={{ width: 40 }}><button className="ibtn" title="Restore this snapshot" onClick={() => restore(s.name)}><RotateCcw /></button></td>
                    </tr>
                  ))}
                </tbody></table>
                {snaps.data?.length === 0 && <p className="faint" style={{ marginTop: 10, fontSize: 12 }}>No snapshots yet. Take one before risky changes; restore puts every key back.</p>}
              </div>
            </div>
          </div>
        )}
      </section>
    </>
  )
}
