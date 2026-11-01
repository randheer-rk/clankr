import { useState } from 'react'
import { Cpu, Play, Search, Sparkles, Workflow as WorkflowIcon } from 'lucide-react'
import { api, ns, type Resource, type Run } from '../api'
import { useApp } from '../App'
import { Flow } from '../Flow'
import { ago, titleCase, usePoll } from '../lib'

function ResourceList({ plural, title, icon, id, go, render }: {
  plural: string; title: string; icon: React.ReactNode; id?: string; go: (to: string) => void
  render: (r: Resource) => React.ReactNode
}) {
  const { ns: n } = useApp()
  const [q, setQ] = useState('')
  const list = usePoll(() => api<{ items: Resource[] }>(`${ns(n)}/${plural}`), [n, plural], 10000)
  const items = (list.data?.items ?? []).filter(r => !q || r.metadata.name.includes(q))
  const selected = items.find(r => r.metadata.name === id) ?? items[0]
  return (
    <>
      <section className="list">
        <div className="bar"><h2>{title}</h2></div>
        <label className="search"><Search /><input value={q} onChange={e => setQ(e.target.value)} placeholder={`Search ${title.toLowerCase()}`} /></label>
        <div className="scroll">
          {!list.data && <div className="empty">{list.error ?? 'Loading…'}</div>}
          {list.data && items.length === 0 && <div className="empty">None in this workspace. Create them with <span className="mono">clank apply</span>.</div>}
          {items.map(r => (
            <button key={r.metadata.name} className={`row ${r === selected ? 'on' : ''}`} onClick={() => go(`/${plural}/${r.metadata.name}`)}>
              <span className="st" style={{ background: '#E5F1FF', color: 'var(--blue)' }}>{icon}</span>
              <div style={{ minWidth: 0 }}><b>{titleCase(r.metadata.name)}</b><div className="s">{r.metadata.name}</div></div>
              <span className="t">{r.metadata.creationTimestamp ? ago(r.metadata.creationTimestamp) : ''}</span>
            </button>
          ))}
        </div>
      </section>
      {selected ? render(selected) : <section className="detail"><div className="empty">Nothing selected</div></section>}
    </>
  )
}

export function Agents({ id, go }: { id?: string; go: (to: string) => void }) {
  return <ResourceList plural="agents" title="Agents" icon={<Sparkles />} id={id} go={go} render={r => <AgentDetail key={r.metadata.name} r={r} />} />
}

function AgentDetail({ r }: { r: Resource }) {
  const s = r.spec as Record<string, any>
  const rt = s.runtime ?? {}
  return (
    <section className="detail">
      <div className="tb"><span className="title">Agent</span></div>
      <div className="body">
        <div className="crumb"><Sparkles style={{ width: 14, height: 14, color: 'var(--ink-3)' }} />{r.metadata.name}<span className="faint">· revision {r.metadata.resourceVersion}</span></div>
        <h1>{titleCase(r.metadata.name)}</h1>
        <div className="meta"><span className="pill run">{rt.type === 'oci' ? 'Sandboxed container' : 'Local process'}</span><span><Cpu />{s.resources?.cpus ?? 1} CPU · {s.resources?.memoryMiB ?? 1024} MiB</span></div>
        <div className="grid2">
          <div className="card"><h3>Runtime</h3><p className="mono" style={{ marginTop: 10, wordBreak: 'break-all' }}>{rt.image ?? (rt.command ?? []).join(' ')}</p></div>
          <div className="card"><h3>Can use</h3>
            <div className="chips">
              {[...(s.models ?? []).map((m: string) => `model: ${m}`), ...(s.contexts ?? []).map((c: string) => `context: ${c}`), ...(s.mailboxes ?? []).map((m: string) => `mailbox: ${m}`), ...(s.volumes ?? []).map((v: any) => `volume: ${v.claim}`)].map(x => <span key={x}>{x}</span>)}
              {!(s.models?.length || s.contexts?.length || s.mailboxes?.length || s.volumes?.length) && <span>nothing beyond its tasks</span>}
            </div>
          </div>
        </div>
        <div className="card" style={{ marginTop: 16 }}><h3>Spec</h3><pre className="json" style={{ marginTop: 10 }}>{JSON.stringify(r.spec, null, 2)}</pre></div>
      </div>
    </section>
  )
}

export function Workflows({ id, go }: { id?: string; go: (to: string) => void }) {
  return <ResourceList plural="workflows" title="Workflows" icon={<WorkflowIcon />} id={id} go={go} render={r => <WorkflowDetail key={r.metadata.name} r={r} go={go} />} />
}

function WorkflowDetail({ r, go }: { r: Resource; go: (to: string) => void }) {
  const { ns: n, refresh } = useApp()
  const [input, setInput] = useState('{\n  \n}')
  const [error, setError] = useState<string>()
  const preview = { graph: r.spec, state: { visits: {}, waits: {}, tasks: {}, data: {} }, status: 'Running' } as unknown as Run
  const start = async () => {
    setError(undefined)
    let parsed
    try { parsed = JSON.parse(input || 'null') } catch { return setError('Input must be valid JSON') }
    try {
      const run = await api<Run>(`${ns(n)}/workflows/${r.metadata.name}/runs`, { method: 'POST', json: { input: parsed } })
      refresh()
      go(`/runs/${run.runId}`)
    } catch (e) { setError((e as Error).message) }
  }
  return (
    <section className="detail">
      <div className="tb"><span className="title">Workflow</span></div>
      <div className="body">
        <div className="crumb"><WorkflowIcon style={{ width: 14, height: 14, color: 'var(--ink-3)' }} />{r.metadata.name}</div>
        <h1>{titleCase(r.metadata.name)}</h1>
        <div className="card" style={{ marginTop: 16 }}><h3>Steps<span>{Object.keys((r.spec as any).nodes ?? {}).length} nodes</span></h3><Flow run={preview} retries={{}} /></div>
        <div className="card" style={{ marginTop: 16 }}>
          <h3>Start a run<span>input is available to every step as <span className="mono">$.input</span></span></h3>
          <textarea value={input} onChange={e => setInput(e.target.value)} spellCheck={false} className="mono" style={{ width: '100%', height: 110, marginTop: 10, border: 0, borderRadius: 10, background: 'var(--fill)', padding: 12, outline: 'none', resize: 'vertical' }} />
          {error && <div className="err">{error}</div>}
          <div style={{ marginTop: 10 }}><button className="b pri" onClick={start}><Play />Start run</button></div>
        </div>
      </div>
    </section>
  )
}
