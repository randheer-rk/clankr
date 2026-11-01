import { useEffect, useRef, useState } from 'react'
import { Check, Hand, LoaderCircle, Send, X } from 'lucide-react'
import type { Run } from './api'
import { layout, nodeStates, titleCase, type NodeState } from './lib'

const NODE_W = 120, ROW = 54, PAD_Y = 8

const look: Record<NodeState, { cls: string; bg: string; icon: React.ReactNode }> = {
  done: { cls: '', bg: '#30D158', icon: <Check /> },
  running: { cls: 'run', bg: '#0A84FF', icon: <LoaderCircle /> },
  waiting: { cls: 'wait', bg: '#FF9F0A', icon: <Hand /> },
  failed: { cls: 'bad', bg: '#FF453A', icon: <X /> },
  todo: { cls: 'todo', bg: '#C7C7CC', icon: <Send /> },
}

/** The run's pinned graph with live node states. */
export function Flow({ run, retries, you }: { run: Run; retries: Record<string, number>; you?: boolean }) {
  const { pos, edges, cols, rows } = layout(run.graph)
  const st = nodeStates(run)
  // spread columns across the card, within readable bounds
  const ref = useRef<HTMLDivElement>(null)
  const [avail, setAvail] = useState(800)
  useEffect(() => {
    const o = new ResizeObserver(([e]) => setAvail(e.contentRect.width))
    if (ref.current) o.observe(ref.current)
    return () => o.disconnect()
  }, [])
  const COL = Math.max(NODE_W + 16, Math.min(190, (avail - NODE_W) / Math.max(cols - 1, 1)))
  const width = cols * COL + 40
  const height = rows * ROW + PAD_Y * 2
  const y = (row: number) => PAD_Y + row * ROW + 18
  return (
    <div className="flow" ref={ref} style={{ height }}>
      <svg width={width} height={height}>
        {edges.map(([a, b], i) => {
          const pa = pos[a], pb = pos[b]
          const back = pb.col <= pa.col
          const x1 = pa.col * COL + NODE_W - 8, y1 = y(pa.row), x2 = pb.col * COL, y2 = y(pb.row)
          const d = back
            ? `M${x1} ${y1} C ${x1 + 40} ${y1 + 40}, ${x2 - 40} ${y2 + 40}, ${x2} ${y2}`
            : `M${x1} ${y1} C ${x1 + 24} ${y1}, ${x2 - 24} ${y2}, ${x2} ${y2}`
          const hot = st[b] === 'waiting' || st[b] === 'running'
          return <path key={i} d={d} fill="none" stroke={hot ? (st[b] === 'waiting' ? '#FF9F0A' : '#0A84FF') : '#D1D1D6'} strokeWidth={hot ? 2 : 1.5} strokeDasharray={st[b] === 'todo' ? '3 4' : undefined} />
        })}
      </svg>
      {Object.entries(pos).map(([id, p]) => {
        const s = look[st[id]]
        const r = retries[id]
        return (
          <div key={id} className={`node ${s.cls}`} style={{ left: p.col * COL, top: y(p.row) - 18 }} title={`${run.graph.nodes[id].type} · ${st[id]}`}>
            <span className="ni" style={{ background: s.bg }}>{s.icon}</span>
            {titleCase(id)}
            {r ? <small>↻{r}</small> : null}
            {st[id] === 'waiting' && you ? <small>you</small> : null}
          </div>
        )
      })}
    </div>
  )
}
