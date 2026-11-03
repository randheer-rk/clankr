import { AbsoluteFill, Easing, interpolate, spring, useCurrentFrame, useVideoConfig } from "remotion"

export const MSG_FRAMES = 320
const ease = Easing.bezier(0.22, 1, 0.36, 1)
const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const

const at = (frame: number, from: number, to: number, a: [number, number], b: [number, number]) => {
  const k = interpolate(frame, [from, to], [0, 1], { ...clamp, easing: ease })
  return { x: a[0] + (b[0] - a[0]) * k, y: a[1] + (b[1] - a[1]) * k, on: frame >= from && frame <= to }
}

function Pill({ x, y, w, children, tone = "card", glow = 0 }: { x: number; y: number; w: number; children: React.ReactNode; tone?: "card" | "ink"; glow?: number }) {
  return (
    <div style={{
      position: "absolute", left: x - w / 2, top: y - 26, width: w, height: 52, borderRadius: 999,
      background: tone === "ink" ? "var(--ink)" : "var(--card)", color: tone === "ink" ? "var(--paper)" : "var(--ink)",
      border: `1.5px solid ${glow > 0.05 ? "var(--ember)" : "var(--line)"}`, boxShadow: `var(--shadow), 0 0 0 ${glow * 8}px color-mix(in srgb, var(--ember) 18%, transparent)`,
      display: "flex", alignItems: "center", justifyContent: "center", gap: 8, fontSize: 15, fontWeight: 600, whiteSpace: "nowrap",
    }}>{children}</div>
  )
}

const Dot = ({ p, color = "var(--ember)" }: { p: { x: number; y: number; on: boolean }; color?: string }) =>
  p.on ? <div style={{ position: "absolute", left: p.x - 8, top: p.y - 8, width: 16, height: 16, borderRadius: 8, background: color, boxShadow: `0 0 0 6px color-mix(in srgb, ${color} 20%, transparent)` }} /> : null

/** Request/reply to a service by name, then a topic fanning out into durable mailboxes. */
export function Messaging({ tall = false }: { tall?: boolean }) {
  const f = useCurrentFrame()
  const { fps } = useVideoConfig()
  const phase2 = f >= 170
  const fade = (from: number, to: number) => interpolate(f, [from, to], [0, 1], clamp)

  // phase 1: writer -> service -> instance 2 -> back
  const W: [number, number] = tall ? [235, 70] : [120, 170], S: [number, number] = tall ? [235, 210] : [430, 170]
  const I = (tall ? [[90, 360], [235, 450], [380, 360]] : [[760, 80], [760, 170], [760, 260]]) as [number, number][]
  const req = at(f, 18, 52, W, S), hop = at(f, 56, 78, S, I[1]), back = at(f, 104, 136, I[1], W)
  const glow = (from: number) => Math.max(0, 1 - Math.abs(f - from) / 14)
  const ms = Math.round(interpolate(f, [18, 136], [0, 41], clamp))

  // phase 2: shop publishes -> topic -> two mailboxes + a trigger
  const P: [number, number] = tall ? [235, 70] : [120, 170], T: [number, number] = tall ? [235, 210] : [400, 170]
  const outs = (tall ? [[118, 370], [352, 370], [235, 470]] : [[720, 70], [720, 170], [720, 270]]) as [number, number][]
  const pub = at(f, 186, 214, P, T)
  const fan = outs.map((o, i) => at(f, 222 + i * 6, 252 + i * 6, T, o))
  const landed = (i: number) => spring({ frame: f - (252 + i * 6), fps, config: { damping: 14 } })

  const o1 = 1 - fade(150, 168), o2 = fade(170, 186)
  return (
    <AbsoluteFill style={{ fontFamily: "var(--sans)", color: "var(--ink)" }}>
      <svg width={tall ? 470 : 940} height={tall ? 600 : 340} style={{ position: "absolute", inset: 0 }}>
        <g opacity={o1}>
          <line x1={W[0]} y1={W[1]} x2={S[0]} y2={S[1]} stroke="var(--line)" strokeWidth={2} strokeDasharray="5 6" />
          {I.map((p, i) => <line key={i} x1={S[0]} y1={S[1]} x2={p[0]} y2={p[1]} stroke="var(--line)" strokeWidth={2} strokeDasharray="5 6" />)}
        </g>
        <g opacity={o2}>
          <line x1={P[0]} y1={P[1]} x2={T[0]} y2={T[1]} stroke="var(--line)" strokeWidth={2} strokeDasharray="5 6" />
          {outs.map((p, i) => <line key={i} x1={T[0]} y1={T[1]} x2={p[0]} y2={p[1]} stroke="var(--line)" strokeWidth={2} strokeDasharray="5 6" />)}
        </g>
      </svg>

      {!phase2 && (
        <div style={{ opacity: o1 }}>
          <Pill x={W[0]} y={W[1]} w={170} glow={glow(136)}>agent/writer</Pill>
          <Pill x={S[0]} y={S[1]} w={tall ? 330 : 300} tone="ink" glow={glow(52)}><span style={{ fontFamily: "var(--mono)", fontSize: 13 }}>clankr://agents/blog/reviewer</span></Pill>
          {I.map((p, i) => <Pill key={i} x={p[0]} y={p[1]} w={tall ? 150 : 190} glow={i === 1 ? glow(84) : 0}><span style={{ fontFamily: "var(--mono)", fontSize: 13 }}>reviewer-{["9c2e", "2f1a", "7d04"][i]}</span></Pill>)}
          <Dot p={req} /><Dot p={hop} /><Dot p={back} color="var(--moss)" />
          <div style={{ position: "absolute", left: tall ? 0 : 40, right: tall ? 0 : undefined, textAlign: tall ? "center" : undefined, top: tall ? 140 : 236, fontFamily: "var(--mono)", fontSize: 14, color: f > 136 ? "var(--moss)" : "var(--muted)", opacity: fade(18, 30) }}>
            {f > 136 ? `reply { "ok": true } · ${ms} ms` : `waiting for a reply · ${ms} ms`}
          </div>
          <div style={{ position: "absolute", left: tall ? 0 : 600, right: tall ? 0 : undefined, textAlign: tall ? "center" : undefined, top: tall ? 540 : 306, fontFamily: "var(--mono)", fontSize: 12, color: "var(--muted)" }}>any instance can answer; the caller never knows which</div>
        </div>
      )}

      {phase2 && (
        <div style={{ opacity: o2 }}>
          <Pill x={P[0]} y={P[1]} w={150}>agent/shop</Pill>
          <Pill x={T[0]} y={T[1]} w={210} tone="ink" glow={glow(214)}><span style={{ fontFamily: "var(--mono)", fontSize: 14 }}>orders.created</span></Pill>
          {[["mailbox · billing", "var(--slate)"], ["mailbox · audit", "var(--slate)"], ["trigger · start run", "var(--moss)"]].map(([label, c], i) => {
            const s = landed(i)
            return (
              <div key={label} style={{ position: "absolute", left: outs[i][0] - (tall ? 110 : 105), top: outs[i][1] - 26, transform: `scale(${0.94 + 0.06 * s})` }}>
                <Pill x={tall ? 110 : 105} y={26} w={tall ? 220 : 210} glow={f >= 252 + i * 6 ? Math.max(0, 1 - (f - 252 - i * 6) / 20) : 0}>
                  <span style={{ width: 8, height: 8, borderRadius: 4, background: c }} />
                  <span style={{ fontFamily: "var(--mono)", fontSize: 13 }}>{label}</span>
                  {f >= 252 + i * 6 && <span style={{ fontFamily: "var(--mono)", fontSize: 12, color: "var(--muted)" }}>+1</span>}
                </Pill>
              </div>
            )
          })}
          <Dot p={pub} />
          {fan.map((p, i) => <Dot key={i} p={p} />)}
          <div style={{ position: "absolute", left: tall ? 0 : 40, right: tall ? 0 : undefined, textAlign: tall ? "center" : undefined, top: tall ? 140 : 236, fontFamily: "var(--mono)", fontSize: 14, color: "var(--muted)", opacity: fade(256, 270) }}>
            a durable copy each; one run started
          </div>
        </div>
      )}
    </AbsoluteFill>
  )
}
