import { createContext, useContext, useEffect, useState } from 'react'
import { Bot, CirclePlay, Gauge, Inbox as InboxIcon, LogOut, Plus, Sparkles, Workflow } from 'lucide-react'
import { api, getToken, ns, setToken, type Approval, type Principal, type Run } from './api'
import { titleCase, usePoll } from './lib'
import { Inbox } from './pages/Inbox'
import { Runs } from './pages/Runs'
import { Budgets } from './pages/Budgets'
import { Agents, Workflows } from './pages/Resources'
import { Login } from './pages/Login'

export interface Ctx {
  ns: string
  me: Principal
  /** Pending approvals the signed-in principal can decide (server-filtered: group, not self). */
  mine: Approval[]
  refresh: () => void
}
const AppCtx = createContext<Ctx>(null as never)
export const useApp = () => useContext(AppCtx)

const NS_KEY = 'clankr.ns', RECENT_KEY = 'clankr.recentNs'
const read = (k: string, d: string) => { try { return localStorage.getItem(k) ?? d } catch { return d } }
const write = (k: string, v: string) => { try { localStorage.setItem(k, v) } catch { /* ignore */ } }

/** `#/runs/<id>` → ['runs', '<id>'] */
function useRoute(): [string[], (to: string) => void] {
  const parse = () => location.hash.replace(/^#\/?/, '').split('/').filter(Boolean).map(decodeURIComponent)
  const [r, setR] = useState(parse)
  useEffect(() => {
    const on = () => setR(parse())
    addEventListener('hashchange', on)
    return () => removeEventListener('hashchange', on)
  }, [])
  return [r, (to: string) => { location.hash = to }]
}

export function App() {
  const [token, setTok] = useState(getToken())
  useEffect(() => {
    const out = () => { setToken(null); setTok(null) }
    addEventListener('clankr:unauthorized', out)
    return () => removeEventListener('clankr:unauthorized', out)
  }, [])
  if (!token) return <Login onToken={t => { setToken(t); setTok(t) }} />
  return <Shell onSignOut={() => { setToken(null); setTok(null) }} />
}

const DOTS = ['#0A84FF', '#30D158', '#BF5AF2', '#FF9F0A', '#FF375F', '#64D2FF']
/** A workspace keeps its colour wherever it appears. */
const dot = (n: string) => DOTS[[...n].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7) % DOTS.length]

function Shell({ onSignOut }: { onSignOut: () => void }) {
  const [route, go] = useRoute()
  const [current, setCurrent] = useState(read(NS_KEY, 'default'))
  const [recent, setRecent] = useState<string[]>(() => JSON.parse(read(RECENT_KEY, '["default"]')))
  const [adding, setAdding] = useState(false)
  const me = usePoll(() => api<Principal>('/v1/whoami'), [], 60000)
  const mine = usePoll(() => api<Approval[]>(`${ns(current)}/approvals?state=pending&mine=true`), [current])
  const runs = usePoll(() => api<Run[]>(`${ns(current)}/runs?limit=100`), [current], 8000)

  const switchNs = (n: string) => {
    setCurrent(n); write(NS_KEY, n)
    // stable order: new workspaces are appended, switching doesn't reshuffle the sidebar
    const r = recent.includes(n) ? recent : [...recent, n].slice(-8)
    setRecent(r); write(RECENT_KEY, JSON.stringify(r))
    go('/runs')
  }
  if (!me.data) return <div className="empty">{me.error ?? 'Loading…'}</div>

  const [page = 'runs', id] = route
  const ctx: Ctx = { ns: current, me: me.data, mine: mine.data ?? [], refresh: () => { mine.reload(); runs.reload() } }
  const running = (runs.data ?? []).filter(r => r.status === 'Running').length
  const nav = (key: string, label: string, icon: React.ReactNode, count?: number, hot?: boolean) => (
    <button className={`it ${page === key ? 'on' : ''}`} onClick={() => go(`/${key}`)}>{icon}{label}{count ? <span className={`c ${hot ? 'hot' : ''}`}>{count}</span> : null}</button>
  )
  const who = me.data.kind === 'owner' ? 'Owner' : me.data.id ?? me.data.name ?? me.data.kind
  const role = me.data.kind === 'owner' ? 'Break-glass admin' : me.data.kind === 'serviceAccount' ? `Service account · ${me.data.namespace}` : (me.data.groups ?? []).join(', ') || 'Signed in'

  return (
    <AppCtx.Provider value={ctx}>
      <div className={`app ${page === 'budgets' ? '' : 'three'}`}>
        <aside className="side">
          <div className="ws"><div className="logo"><Bot /></div><div><b>{titleCase(current)}</b><span>clankr</span></div></div>
          {nav('inbox', 'Inbox', <InboxIcon />, ctx.mine.length, true)}
          {nav('runs', 'Runs', <CirclePlay />, running)}
          {nav('agents', 'Agents', <Sparkles />)}
          {nav('workflows', 'Workflows', <Workflow />)}
          <div className="grp">Govern</div>
          {nav('budgets', 'Budgets', <Gauge />)}
          <div className="grp" style={{ display: 'flex', justifyContent: 'space-between' }}>Workspaces<button className="faint" onClick={() => setAdding(a => !a)} title="Open a workspace"><Plus style={{ width: 13, height: 13 }} /></button></div>
          {adding && (
            <form className="addns" onSubmit={e => { e.preventDefault(); const v = new FormData(e.currentTarget).get('n')?.toString().trim(); if (v) { switchNs(v); setAdding(false) } }}>
              <input name="n" placeholder="namespace" autoFocus />
            </form>
          )}
          {recent.map(n => (
            <button key={n} className={`it ${n === current ? 'on' : ''}`} onClick={() => switchNs(n)}><span className="dot" style={{ background: dot(n) }} />{titleCase(n)}</button>
          ))}
          <div className="me">
            <div className="av">{who.slice(0, 2).toUpperCase()}</div>
            <div style={{ minWidth: 0 }}><b style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{who}</b><div className="muted" style={{ fontSize: 11.5 }}>{role}</div></div>
            <button className="ibtn" title="Sign out" onClick={onSignOut}><LogOut /></button>
          </div>
        </aside>
        {page === 'inbox' && <Inbox id={id} go={go} />}
        {page === 'runs' && <Runs id={id} go={go} runs={runs.data} error={runs.error} reload={runs.reload} />}
        {page === 'budgets' && <Budgets />}
        {page === 'agents' && <Agents id={id} go={go} />}
        {page === 'workflows' && <Workflows id={id} go={go} />}
      </div>
    </AppCtx.Provider>
  )
}
