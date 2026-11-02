import { useState } from 'react'
import { KeyRound, Plus, ShieldCheck, Trash2, UserRound, Users } from 'lucide-react'
import { api, ns, type Resource } from '../api'
import { useApp } from '../App'
import { actorName, ago, titleCase, usePoll } from '../lib'

interface Token { id: string; description: string; createdAt: string; expiresAt?: string; revokedAt?: string; createdBy: string }

const ROLES = ['viewer', 'editor', 'approver', 'admin', 'cluster-admin']
const roleHelp: Record<string, string> = {
  viewer: 'Read resources and runs',
  editor: 'Change resources, start runs, do agent work',
  approver: 'Read, and decide approvals for their groups',
  admin: 'Everything in the workspace, including secrets, access and image promotion',
  'cluster-admin': 'Everything, everywhere (bind it in clankr-system)',
}

/** People & access: who can do what in this workspace (RoleBindings) and the service accounts
 *  machines sign in as, with their tokens. */
export function Access() {
  const { ns: n } = useApp()
  const bindings = usePoll(() => api<{ items: Resource[] }>(`${ns(n)}/rolebindings`), [n], 8000)
  const accounts = usePoll(() => api<{ items: Resource[] }>(`${ns(n)}/serviceaccounts`), [n], 8000)
  const [error, setError] = useState<string>()
  const [role, setRole] = useState('viewer')
  const [subjects, setSubjects] = useState('')
  const [newSa, setNewSa] = useState('')
  const [selected, setSelected] = useState<string>()
  const [fresh, setFresh] = useState<{ sa: string; token: string }>()
  const tokens = usePoll(() => (selected ? api<Token[]>(`${ns(n)}/serviceaccounts/${selected}/tokens`) : Promise.resolve([] as Token[])), [n, selected], 8000)

  const run = async (f: () => Promise<unknown>, after: () => void) => {
    setError(undefined)
    try { await f(); after() } catch (e) { setError((e as Error).message) }
  }
  const envelope = (kind: string, name: string, spec: unknown) => ({ apiVersion: 'clankr.io/v1alpha1', kind, metadata: { name, namespace: n }, spec })

  const addBinding = () => {
    const subs = subjects.split(/[,\s]+/).filter(Boolean)
    const name = `${role}-${Math.random().toString(36).slice(2, 7)}`
    run(() => api(`${ns(n)}/rolebindings`, { method: 'POST', json: envelope('RoleBinding', name, { role, subjects: subs }) }), () => { setSubjects(''); bindings.reload() })
  }

  return (
    <section className="detail">
      <div className="tb"><span className="title">People &amp; access</span><span className="sp" /><span className="faint">{titleCase(n)} workspace</span></div>
      <div className="body">
        {error && <div className="err" style={{ marginTop: 0, marginBottom: 14 }}>{error}</div>}
        <div className="grid2" style={{ marginTop: 0, gridTemplateColumns: '1.2fr 1fr' }}>
          <div className="card">
            <h3><span style={{ display: 'flex', gap: 8, alignItems: 'center', color: 'var(--ink)' }}><ShieldCheck style={{ color: 'var(--blue)' }} />Who can do what</span><span>{bindings.data?.items.length ?? 0} {bindings.data?.items.length === 1 ? 'binding' : 'bindings'}</span></h3>
            {bindings.error && <p className="err">{bindings.error}</p>}
            <table className="t"><tbody>
              {bindings.data?.items.map(b => (
                <tr key={b.metadata.name}>
                  <td style={{ width: 120 }}><span className="pill run">{String(b.spec.role)}</span></td>
                  <td>{(b.spec.subjects as string[]).map(s => <div key={s}>{s.startsWith('group:') ? <Users style={{ width: 13, height: 13, marginRight: 6, verticalAlign: -2 }} /> : <UserRound style={{ width: 13, height: 13, marginRight: 6, verticalAlign: -2 }} />}{s.replace(/^(user|group|serviceaccount):/, '')}<span className="faint"> · {s.split(':')[0]}</span></div>)}</td>
                  <td style={{ width: 32 }}><button className="ibtn" title="Remove" onClick={() => run(() => api(`${ns(n)}/rolebindings/${b.metadata.name}`, { method: 'DELETE' }), bindings.reload)}><Trash2 /></button></td>
                </tr>
              ))}
            </tbody></table>
            <div style={{ marginTop: 14, display: 'grid', gridTemplateColumns: '150px 1fr auto', gap: 8 }}>
              <select value={role} onChange={e => setRole(e.target.value)} className="note" style={{ height: 34 }}>{ROLES.map(r => <option key={r}>{r}</option>)}</select>
              <input className="note" style={{ height: 34 }} value={subjects} onChange={e => setSubjects(e.target.value)} placeholder="user:ana@example.com, group:editors, serviceaccount:ns/ci" />
              <button className="b pri" disabled={!subjects.trim()} onClick={addBinding}><Plus />Grant</button>
            </div>
            <p className="faint" style={{ marginTop: 8, fontSize: 12 }}>{roleHelp[role]}</p>
          </div>

          <div className="card">
            <h3><span style={{ display: 'flex', gap: 8, alignItems: 'center', color: 'var(--ink)' }}><KeyRound style={{ color: 'var(--orange)' }} />Service accounts</span><span>for CI and machines</span></h3>
            <table className="t"><tbody>
              {accounts.data?.items.map(a => (
                <tr key={a.metadata.name} style={{ cursor: 'pointer', background: selected === a.metadata.name ? '#F5F9FF' : undefined }} onClick={() => setSelected(a.metadata.name)}>
                  <td><b style={{ fontWeight: 600 }}>{a.metadata.name}</b><div className="faint" style={{ fontSize: 12 }}>{((a.spec.groups as string[]) ?? []).join(', ') || 'no groups'}</div></td>
                  <td style={{ width: 32 }}><button className="ibtn" title="Delete (revokes its tokens)" onClick={e => { e.stopPropagation(); run(() => api(`${ns(n)}/serviceaccounts/${a.metadata.name}`, { method: 'DELETE' }), accounts.reload) }}><Trash2 /></button></td>
                </tr>
              ))}
            </tbody></table>
            <div style={{ marginTop: 12, display: 'flex', gap: 8 }}>
              <input className="note" style={{ height: 34 }} value={newSa} onChange={e => setNewSa(e.target.value.toLowerCase())} placeholder="new account name" />
              <button className="b sec" disabled={!newSa} onClick={() => run(() => api(`${ns(n)}/serviceaccounts`, { method: 'POST', json: envelope('ServiceAccount', newSa, {}) }), () => { setSelected(newSa); setNewSa(''); accounts.reload() })}><Plus />Add</button>
            </div>
            {selected && (
              <div style={{ marginTop: 18 }}>
                <h3>Tokens for {selected}<button className="b sec" onClick={() => run(async () => { const t = await api<{ token: string }>(`${ns(n)}/serviceaccounts/${selected}/tokens`, { method: 'POST', json: { description: 'created in the web UI' } }); setFresh({ sa: selected, token: t.token }) }, tokens.reload)}><Plus />New token</button></h3>
                {fresh?.sa === selected && <div className="ask" style={{ gridTemplateColumns: '1fr', marginTop: 10, padding: 12 }}><div><b style={{ fontSize: 13 }}>Copy it now; it won't be shown again</b><p className="mono" style={{ wordBreak: 'break-all', color: 'var(--ink)', marginTop: 6 }}>{fresh.token}</p></div></div>}
                <table className="t"><tbody>
                  {tokens.data?.map(t => (
                    <tr key={t.id}>
                      <td className="mono">{t.id}</td>
                      <td className="faint">{t.revokedAt ? 'revoked' : t.expiresAt ? `expires ${ago(t.expiresAt)}` : 'no expiry'} · by {actorName(t.createdBy)}</td>
                      <td style={{ width: 32 }}>{!t.revokedAt && <button className="ibtn" title="Revoke" onClick={() => run(() => api(`${ns(n)}/serviceaccounts/${selected}/tokens/${t.id}`, { method: 'DELETE' }), tokens.reload)}><Trash2 /></button>}</td>
                    </tr>
                  ))}
                </tbody></table>
              </div>
            )}
          </div>
        </div>
      </div>
    </section>
  )
}
