import { useState } from 'react'
import { Cpu, Globe, Moon, Play, Search, Server, Sparkles, Sun, Workflow as WorkflowIcon } from 'lucide-react'
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
        <div className="meta"><span className="pill run">{rt.type !== 'oci' ? 'Local process' : rt.class === 'microvm' ? 'MicroVM' : rt.class === 'gvisor' ? 'gVisor sandbox' : 'Sandboxed container'}</span><span><Cpu />{s.resources?.cpus ?? 1} CPU · {s.resources?.memoryMiB ?? 1024} MiB</span><span><Globe />{s.egress?.length ? `${s.egress.length} egress host${s.egress.length === 1 ? '' : 's'}` : 'No network'}</span></div>
        <div className="grid2">
          <div className="card"><h3>Runtime</h3><p className="mono" style={{ marginTop: 10, wordBreak: 'break-all' }}>{rt.image ?? (rt.command ?? []).join(' ')}</p></div>
          <div className="card"><h3>Can use</h3>
            <div className="chips">
              {[...(s.models ?? []).map((m: string) => `model: ${m}`), ...(s.contexts ?? []).map((c: string) => `context: ${c}`), ...(s.tools ?? []).map((t: string) => `tools: ${t}`), ...(s.mailboxes ?? []).map((m: string) => `mailbox: ${m}`), ...(s.volumes ?? []).map((v: any) => `volume: ${v.claim}`), ...(s.egress ?? []).map((e: string) => `egress: ${e}`)].map(x => <span key={x}>{x}</span>)}
              {!(s.models?.length || s.contexts?.length || s.tools?.length || s.mailboxes?.length || s.volumes?.length || s.egress?.length) && <span>nothing beyond its tasks</span>}
            </div>
          </div>
        </div>
        <Deployments agent={r.metadata.name} />
        <div className="card" style={{ marginTop: 16 }}><h3>Spec</h3><pre className="json" style={{ marginTop: 10 }}>{JSON.stringify(r.spec, null, 2)}</pre></div>
      </div>
    </section>
  )
}

/** Where this agent runs: its deployments, with rollout, canary and hibernation state. */
function Deployments({ agent }: { agent: string }) {
  const { ns: n } = useApp()
  const list = usePoll(() => api<{ items: Resource[] }>(`${ns(n)}/agentdeployments`), [n], 5000)
  const [error, setError] = useState<string>()
  const mine = (list.data?.items ?? []).filter(d => d.spec.agent === agent)
  if (!mine.length) return null
  const toggle = async (d: Resource) => {
    setError(undefined)
    try {
      const cur = await api<Resource>(`${ns(n)}/agentdeployments/${d.metadata.name}`)
      await api(`${ns(n)}/agentdeployments/${d.metadata.name}`, { method: 'PUT', json: { ...cur, spec: { ...cur.spec, hibernate: !cur.spec.hibernate } } })
      list.reload()
    } catch (e) { setError((e as Error).message) }
  }
  return (
    <div className="card" style={{ marginTop: 16 }}>
      <h3><span style={{ display: 'flex', gap: 8, alignItems: 'center', color: 'var(--ink)' }}><Server style={{ color: 'var(--blue)' }} />Deployments</span><span>{mine.length}</span></h3>
      {error && <div className="err">{error}</div>}
      <table className="t"><tbody>
        {mine.map(d => {
          const st = (d.status ?? {}) as Record<string, any>
          const asleep = !!d.spec.hibernate
          const phase = String(st.phase ?? 'Pending')
          const tone = phase === 'Ready' ? 'ok' : phase === 'Hibernated' ? 'off' : phase === 'RollingOut' || phase === 'Hibernating' ? 'run' : 'wait'
          const cp = (st.hibernation?.checkpoints ?? []) as { slot: number; version: number; checkpointed: boolean }[]
          return (
            <tr key={d.metadata.name}>
              <td>
                <b style={{ fontWeight: 600 }}>{d.metadata.name}</b> <span className={`pill ${tone}`} style={{ marginLeft: 6 }}>{phase}</span>
                <div className="faint" style={{ fontSize: 12, marginTop: 4 }}>
                  {asleep ? `${cp.filter(c => c.checkpointed).length} of ${cp.length} slots checkpointed` : `${st.available ?? 0} of ${String(d.spec.replicas)} ready · generation ${st.agentGeneration ?? '–'}`}
                  {st.rollout && <> · {st.rollout}</>}
                  {st.autoscale && <> · autoscaled to {st.autoscale.replicas} (queue {st.autoscale.queueDepth})</>}
                  {phase === 'Pending' && st.reason && <> · {st.reason}</>}
                </div>
                {st.canary && (
                  <div style={{ fontSize: 12, marginTop: 4, color: st.canary.phase === 'Failed' ? '#C9271D' : 'var(--ink-2)' }}>
                    Canary of generation {st.canary.generation}: {st.canary.phase}{st.canary.reason ? ` · ${st.canary.reason}` : ''}
                  </div>
                )}
              </td>
              <td style={{ width: 120, textAlign: 'right' }}>
                <button className="b sec" onClick={() => toggle(d)} title={asleep ? 'Start instances, each slot from its checkpoint' : 'Checkpoint every instance and free its compute'}>{asleep ? <><Sun />Resume</> : <><Moon />Hibernate</>}</button>
              </td>
            </tr>
          )
        })}
      </tbody></table>
    </div>
  )
}

export function Workflows({ id, go }: { id?: string; go: (to: string) => void }) {
  return <ResourceList plural="workflows" title="Workflows" icon={<WorkflowIcon />} id={id} go={go} render={r => <WorkflowDetail key={r.metadata.name} r={r} go={go} />} />
}

function WorkflowDetail({ r, go }: { r: Resource; go: (to: string) => void }) {
  const { ns: n, refresh } = useApp()
  const [input, setInput] = useState('{\n  \n}')
  const [cluster, setCluster] = useState('')
  const [error, setError] = useState<string>()
  const remote = usePoll(() => api<{ runId: string; workflow: string; cluster: string; region: string; status: string; createdAt: string }[]>(`${ns(n)}/remoteruns`), [n], 10000)
  const mineRemote = (remote.data ?? []).filter(x => x.workflow === r.metadata.name)
  const preview = { graph: r.spec, state: { visits: {}, waits: {}, tasks: {}, data: {} }, status: 'Running' } as unknown as Run
  const start = async () => {
    setError(undefined)
    let parsed
    try { parsed = JSON.parse(input || 'null') } catch { return setError('Input must be valid JSON') }
    try {
      const run = await api<Run>(`${ns(n)}/workflows/${r.metadata.name}/runs`, { method: 'POST', json: { input: parsed, cluster: cluster.trim() || undefined } })
      refresh()
      if (cluster.trim()) remote.reload()
      else go(`/runs/${run.runId}`)
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
          <div style={{ marginTop: 10, display: 'flex', gap: 8, alignItems: 'center' }}>
            <button className="b pri" onClick={start}><Play />{cluster.trim() ? `Start on ${cluster.trim()}` : 'Start run'}</button>
            <input className="note" style={{ height: 34, maxWidth: 240 }} value={cluster} onChange={e => setCluster(e.target.value)} placeholder="here, or a cluster name" />
          </div>
        </div>
        {mineRemote.length > 0 && (
          <div className="card" style={{ marginTop: 16 }}>
            <h3>Runs on other clusters<span>placed under this workspace's PlacementPolicy</span></h3>
            <table className="t"><tbody>
              {mineRemote.map(x => (
                <tr key={x.runId}>
                  <td className="mono">{x.runId.slice(0, 8)}</td>
                  <td><Globe style={{ width: 13, height: 13, marginRight: 6, verticalAlign: -2 }} />{x.cluster}<span className="faint"> · {x.region}</span></td>
                  <td><span className={`pill ${x.status === 'Succeeded' ? 'ok' : x.status === 'Running' ? 'run' : 'bad'}`}>{x.status}</span></td>
                  <td className="faint" style={{ textAlign: 'right' }}>{ago(x.createdAt)}</td>
                </tr>
              ))}
            </tbody></table>
          </div>
        )}
      </div>
    </section>
  )
}
