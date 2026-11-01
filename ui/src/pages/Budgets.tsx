import { CalendarDays, CalendarRange, Cpu, HardDrive, Infinity as Inf, MemoryStick, Play, Boxes } from 'lucide-react'
import { api, ns, type Usage } from '../api'
import { useApp } from '../App'
import { compact, money, titleCase, usePoll } from '../lib'

const PALETTE = ['#0A84FF', '#5E5CE6', '#30B0C7', '#FF9F0A', '#BF5AF2', '#C7C7CC']

const quotaLabel: Record<string, [string, React.ReactNode, string?]> = {
  activeRuns: ['Runs at the same time', <Play />, 'new runs are refused above this'],
  cpus: ['CPUs', <Cpu />, 'across all agent deployments'],
  memoryMiB: ['Memory', <MemoryStick />, 'MiB'],
  instances: ['Agent instances', <Boxes />],
  volumeMiB: ['Volume storage', <HardDrive />, 'MiB'],
}

export function Budgets() {
  const { ns: n } = useApp()
  const u = usePoll(() => api<Usage>(`${ns(n)}/usage`), [n], 10000)
  if (!u.data) return <section className="detail"><div className="empty">{u.error ?? 'Loading…'}</div></section>
  const d = u.data
  const monthly = d.budgets.find(b => b.period === 'month' && b.costUSD)
  const monthSpend = d.modelSpendThisMonth.reduce((s, c) => s + (c.costUSD ?? 0), 0)
  const totalCalls = d.modelSpendThisMonth.reduce((s, c) => s + c.calls, 0)
  return (
    <section className="detail">
      <div className="tb"><span className="title">Budgets &amp; limits</span><span className="sp" /><span className="faint">{titleCase(n)} workspace · refreshed live</span></div>
      <div className="body">
        <div className="hero">
          <div className="card">
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start' }}>
              <div>
                <div className="muted" style={{ fontWeight: 500 }}>Model spend this month</div>
                <div className="big">{money(monthSpend)} {monthly?.costUSD && <small>of {money(monthly.costUSD.limit, 0)}</small>}</div>
                <Forecast spent={monthSpend} limit={monthly?.costUSD?.limit} />
              </div>
              <div className="faint" style={{ fontSize: 12 }}>{totalCalls} model calls</div>
            </div>
            <SpendChart days={d.spendByDay} limit={monthly?.costUSD?.limit} />
          </div>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
            {d.budgets.length === 0 && <div className="card"><h3>No budgets</h3><p className="muted" style={{ marginTop: 8 }}>Add an <span className="mono">AgentBudget</span> to cap spend per day or month. Calls that would go over are refused before they’re sent.</p></div>}
            {d.budgets.map(b => {
              const m = b.costUSD ?? b.tokens!
              const pct = Math.min(100, (100 * (m.used + m.reserved)) / Math.max(m.limit, 1e-9))
              const color = pct > 90 ? 'var(--red)' : b.period === 'day' ? 'var(--blue)' : 'var(--orange)'
              return (
                <div className="card" key={b.name}>
                  <h3><span style={{ display: 'flex', alignItems: 'center', gap: 8, color: 'var(--ink)', fontWeight: 600 }}><span style={{ width: 26, height: 26, borderRadius: 7, display: 'grid', placeItems: 'center', color: '#fff', background: color }}>{b.period === 'day' ? <CalendarDays style={{ width: 14, height: 14 }} /> : b.period === 'month' ? <CalendarRange style={{ width: 14, height: 14 }} /> : <Inf style={{ width: 14, height: 14 }} />}</span>{titleCase(b.name)}</span><span>{b.window.replace(/^(day|month):/, '')}</span></h3>
                  <div className="kv"><b>{b.costUSD ? money(b.costUSD.used) : compact(m.used)} <small>of {b.costUSD ? money(b.costUSD.limit, 0) : compact(m.limit)}</small></b><span className="faint">{Math.round(pct)}%</span></div>
                  <div className="meter" style={{ height: 8 }}><i style={{ width: `${pct}%`, background: color }} /></div>
                  <div style={{ display: 'flex', justifyContent: 'space-between', marginTop: 6 }} className="faint">
                    <span>{b.costUSD && b.tokens ? `${compact(b.tokens.used)} of ${compact(b.tokens.limit)} tokens` : 'Calls stop at the cap'}</span>
                    {m.reserved > 0 && <span>{b.costUSD ? money(m.reserved) : compact(m.reserved)} in flight</span>}
                  </div>
                </div>
              )
            })}
          </div>
        </div>

        <div className="grid2" style={{ gridTemplateColumns: '1.15fr 1fr' }}>
          <div className="card">
            <h3>Limits<span>Resource quota</span></h3>
            {d.quotas.length === 0 && <p className="muted" style={{ marginTop: 8 }}>No <span className="mono">ResourceQuota</span> in this workspace.</p>}
            <table className="t"><tbody>
              {d.quotas.map(q => {
                const [label, icon, hint] = quotaLabel[q.resource] ?? [titleCase(q.resource.replace('count/', '')) + (q.resource.startsWith('count/') ? '' : ''), <Boxes />, q.resource.startsWith('count/') ? 'objects' : undefined]
                const pct = Math.min(100, (100 * q.used) / Math.max(q.hard, 1e-9))
                return (
                  <tr key={q.resource}>
                    <td style={{ width: 34 }}><span style={{ width: 26, height: 26, borderRadius: 7, background: 'var(--fill)', display: 'grid', placeItems: 'center', color: 'var(--ink-2)' }}>{icon}</span></td>
                    <td><div style={{ fontWeight: 500 }}>{label}</div>{hint && <div className="faint" style={{ fontSize: 12 }}>{hint}</div>}</td>
                    <td style={{ width: 140, verticalAlign: 'middle' }}><div className="meter" style={{ margin: 0 }}><i style={{ width: `${pct}%`, background: pct > 80 ? 'var(--orange)' : 'var(--green)' }} /></div></td>
                    <td style={{ width: 80, textAlign: 'right', fontWeight: 600, fontVariantNumeric: 'tabular-nums' }}>{+q.used.toFixed(2)} <span className="faint" style={{ fontWeight: 500 }}>/ {q.hard}</span></td>
                  </tr>
                )
              })}
            </tbody></table>
          </div>
          <div className="card">
            <h3>Where the money goes<span>This month</span></h3>
            {d.modelSpendThisMonth.length === 0 ? <p className="muted" style={{ marginTop: 8 }}>No model calls yet this month.</p> : (
              <>
                <div className="stack">{d.modelSpendThisMonth.map((c, i) => <i key={c.claim} style={{ flex: c.costUSD ?? c.inputTokens + c.outputTokens, background: PALETTE[i % PALETTE.length] }} />)}</div>
                <table className="t"><tbody>
                  {d.modelSpendThisMonth.map((c, i) => (
                    <tr key={c.claim}>
                      <td style={{ width: 18, verticalAlign: 'middle' }}><span style={{ display: 'block', width: 10, height: 10, borderRadius: 3, background: PALETTE[i % PALETTE.length] }} /></td>
                      <td><div style={{ fontWeight: 500 }}>{titleCase(c.claim)}</div><div className="faint" style={{ fontSize: 12 }}>{c.calls} calls · {compact(c.inputTokens + c.outputTokens)} tokens</div></td>
                      <td className="faint" style={{ textAlign: 'right' }}>{monthSpend > 0 && c.costUSD != null ? `${Math.round((100 * c.costUSD) / monthSpend)}%` : ''}</td>
                      <td style={{ textAlign: 'right', fontWeight: 600, width: 80 }}>{money(c.costUSD)}</td>
                    </tr>
                  ))}
                </tbody></table>
              </>
            )}
          </div>
        </div>
      </div>
    </section>
  )
}

function Forecast({ spent, limit }: { spent: number; limit?: number }) {
  const now = new Date()
  const days = new Date(now.getUTCFullYear(), now.getUTCMonth() + 1, 0).getDate()
  const forecast = (spent / Math.max(now.getUTCDate() - 1 + now.getUTCHours() / 24, 0.5)) * days
  if (!spent) return <div className="faint" style={{ fontSize: 12.5, marginTop: 2 }}>No spend yet</div>
  const over = limit != null && forecast > limit
  return <div style={{ color: over ? 'var(--red)' : 'var(--green-ink)', fontWeight: 500, fontSize: 12.5, marginTop: 2 }}>{over ? 'Over pace' : 'On track'} · forecast {money(forecast, 0)} by month end</div>
}

/** Cumulative spend this month with the month's straight-line forecast and the budget line. */
function SpendChart({ days, limit }: { days: Usage['spendByDay']; limit?: number }) {
  const now = new Date()
  const n = new Date(now.getUTCFullYear(), now.getUTCMonth() + 1, 0).getDate()
  const today = now.getUTCDate()
  const per: number[] = Array(n + 1).fill(0)
  for (const d of days) per[+d.day.slice(8, 10)] += d.costUSD ?? 0
  const cum: number[] = []
  per.reduce((s, v, i) => (cum[i] = s + v), 0)
  const spent = cum[today] ?? 0
  const forecast = (spent / Math.max(today - 1 + now.getUTCHours() / 24, 0.5)) * n
  const top = Math.max(limit ?? 0, forecast, spent, 1) * 1.08
  const W = 700, H = 180
  const x = (day: number) => ((day - 1) / (n - 1)) * W
  const y = (v: number) => H - (v / top) * (H - 6)
  const pts = Array.from({ length: today }, (_, i) => `${x(i + 1).toFixed(1)} ${y(cum[i + 1]).toFixed(1)}`)
  return (
    <>
      <svg width="100%" height={H} viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" style={{ marginTop: 12 }}>
        <defs><linearGradient id="sp" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stopColor="#0A84FF" stopOpacity=".22" /><stop offset="1" stopColor="#0A84FF" stopOpacity="0" /></linearGradient></defs>
        {[0.25, 0.5, 0.75].map(f => <path key={f} d={`M0 ${H * f} H${W}`} stroke="#F2F2F7" />)}
        {limit != null && <path d={`M0 ${y(limit)} H${W}`} stroke="#FF453A" strokeDasharray="4 4" />}
        {pts.length > 0 && <path d={`M${pts.join(' L')} L${x(today)} ${H} L0 ${H} Z`} fill="url(#sp)" />}
        {pts.length > 0 && <path d={`M${pts.join(' L')}`} fill="none" stroke="#0A84FF" strokeWidth={2} vectorEffect="non-scaling-stroke" />}
        {spent > 0 && <path d={`M${x(today)} ${y(spent)} L${W} ${y(forecast)}`} stroke="#C7C7CC" strokeWidth={2} strokeDasharray="5 5" vectorEffect="non-scaling-stroke" />}
      </svg>
      <div style={{ display: 'flex', justifyContent: 'space-between', color: 'var(--ink-3)', fontSize: 11.5, marginTop: 4 }}>
        {[1, 8, 15, 22, n].map(dd => <span key={dd}>{now.toLocaleString([], { month: 'short' })} {dd}{dd === today ? ' · today' : ''}</span>)}
      </div>
    </>
  )
}
