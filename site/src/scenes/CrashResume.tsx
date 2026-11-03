import { AbsoluteFill, interpolate, spring, useCurrentFrame, useVideoConfig, Easing } from "remotion"

export const CRASH_FRAMES = 330

const EVENTS: { at: number; seq: string; kind: string; detail: string; tone?: "bad" | "good" }[] = [
  { at: 8, seq: "1", kind: "RunStarted", detail: "workflow: draft" },
  { at: 22, seq: "2", kind: "TaskScheduled", detail: "write → agent/writer" },
  { at: 78, seq: "3", kind: "TaskCompleted", detail: "write · 412 tokens" },
  { at: 92, seq: "4", kind: "TaskScheduled", detail: "review → agent/reviewer" },
  { at: 150, seq: "·", kind: "node-a lost", detail: "its lease lapses", tone: "bad" },
  { at: 214, seq: "·", kind: "redelivered", detail: "review → node-b", tone: "good" },
  { at: 268, seq: "5", kind: "TaskCompleted", detail: "review · approved" },
  { at: 290, seq: "6", kind: "RunCompleted", detail: "nothing ran twice" },
]

const ease = Easing.bezier(0.22, 1, 0.36, 1)

/** A token travelling between two points over [from, to] frames. */
function travel(frame: number, from: number, to: number, a: [number, number], b: [number, number]) {
  const k = interpolate(frame, [from, to], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp", easing: ease })
  return { x: a[0] + (b[0] - a[0]) * k, y: a[1] + (b[1] - a[1]) * k, on: frame >= from && frame <= to + 4 }
}

export function CrashResume({ tall = false }: { tall?: boolean }) {
  const frame = useCurrentFrame()
  const { fps } = useVideoConfig()
  const CP: [number, number] = [tall ? 235 : 250, 106], A: [number, number] = [tall ? 125 : 120, 270], B: [number, number] = [tall ? 345 : 380, 270]

  // the token: write → node-a, back; review → node-a (dies there); later → node-b, back
  const legs = [
    travel(frame, 24, 52, CP, A), travel(frame, 66, 80, A, CP),
    travel(frame, 94, 120, CP, A), travel(frame, 200, 230, CP, B), travel(frame, 252, 270, B, CP),
  ]
  const tok = legs.find(l => l.on)
  const dead = frame >= 140
  const shake = dead && frame < 156 ? Math.sin(frame * 2.4) * 5 : 0
  // the lease: a ring that drains from the crash until it lapses
  const lease = interpolate(frame, [140, 200], [1, 0], { extrapolateLeft: "clamp", extrapolateRight: "clamp" })
  const pop = (at: number) => spring({ frame: frame - at, fps, config: { damping: 18, stiffness: 180 } })

  const node = (label: string, [x, y]: [number, number], state: "idle" | "busy" | "dead" | "done") => (
    <div style={{
      position: "absolute", left: x - (tall ? 100 : 90), top: y - 44, width: tall ? 200 : 180, height: 88, borderRadius: 22,
      background: state === "dead" ? "color-mix(in srgb, var(--bad) 10%, var(--card))" : "var(--card)",
      border: `1.5px solid ${state === "dead" ? "var(--bad)" : state === "busy" ? "var(--ember)" : "var(--line)"}`,
      boxShadow: "var(--shadow)", display: "flex", flexDirection: "column", justifyContent: "center", padding: "0 18px",
      transform: `translateX(${label === "node-a" ? shake : 0}px)`, opacity: state === "dead" ? 0.75 : 1,
    }}>
      <span style={{ fontFamily: "var(--mono)", fontSize: 13, color: "var(--muted)" }}>{label}</span>
      <span style={{ fontSize: 17, fontWeight: 600, color: state === "dead" ? "var(--bad)" : "var(--ink)" }}>
        {state === "dead" ? "lost" : state === "busy" ? "running a task" : state === "done" ? "done" : "ready"}
      </span>
    </div>
  )

  const aState = dead ? "dead" : (frame >= 52 && frame < 66) || frame >= 120 ? "busy" : "idle"
  const bState = frame >= 230 && frame < 252 ? "busy" : frame >= 252 ? "done" : "idle"

  return (
    <AbsoluteFill style={{ fontFamily: "var(--sans)", color: "var(--ink)" }}>
      {/* control plane */}
      <div style={{ position: "absolute", left: CP[0] - 110, top: 34, width: 220, height: 72, borderRadius: 20, background: "var(--ink)", color: "var(--paper)", display: "grid", placeItems: "center", boxShadow: "var(--shadow)" }}>
        <div style={{ textAlign: "center", lineHeight: 1.2 }}>
          <div style={{ fontWeight: 650, fontSize: 18 }}>clankrd</div>
          <div style={{ fontFamily: "var(--mono)", fontSize: 12, opacity: 0.65 }}>events in Postgres</div>
        </div>
      </div>
      {/* wires */}
      <svg width={520} height={340} style={{ position: "absolute", left: 0, top: 0 }}>
        {[A, B].map((p, i) => <line key={i} x1={CP[0]} y1={CP[1]} x2={p[0]} y2={p[1] - 44} stroke="var(--line)" strokeWidth={2} strokeDasharray="5 6" />)}
      </svg>
      {node("node-a", A, aState)}
      {node("node-b", B, bState)}
      {/* the lease, draining after the crash: when it lapses, the task can go elsewhere */}
      {dead && frame < 206 && (
        <div style={{ position: "absolute", left: A[0] + 34, top: A[1] - 18, display: "flex", alignItems: "center", gap: 6, fontFamily: "var(--mono)", fontSize: 11, color: "var(--bad)" }}>
          <svg width={26} height={26}><circle cx={13} cy={13} r={10} fill="none" stroke="color-mix(in srgb, var(--bad) 25%, transparent)" strokeWidth={3} />
            <circle cx={13} cy={13} r={10} fill="none" stroke="var(--bad)" strokeWidth={3} strokeLinecap="round" strokeDasharray={`${2 * Math.PI * 10 * lease} 999`} transform="rotate(-90 13 13)" /></svg>
          lease
        </div>
      )}
      {tok && <div style={{ position: "absolute", left: tok.x - 9, top: tok.y - 9, width: 18, height: 18, borderRadius: 9, background: "var(--ember)", boxShadow: "0 0 0 6px color-mix(in srgb, var(--ember) 22%, transparent)" }} />}

      {/* the event log */}
      <div style={{ position: "absolute", left: tall ? 25 : 540, top: tall ? 340 : 16, width: tall ? 420 : 380, padding: "10px 0", borderRadius: 20, background: "var(--card)", border: "1px solid var(--line)", boxShadow: "var(--shadow)", fontFamily: "var(--mono)", fontSize: 13.5 }}>
        <div style={{ padding: "4px 18px 8px", fontSize: 11, letterSpacing: ".12em", textTransform: "uppercase", color: "var(--muted)" }}>run 7ef6 · history</div>
        {EVENTS.map(e => {
          const s = pop(e.at)
          if (frame < e.at) return null
          const color = e.tone === "bad" ? "var(--bad)" : e.tone === "good" ? "var(--moss)" : "var(--ink)"
          return (
            <div key={e.kind + e.at} style={{ display: "grid", gridTemplateColumns: "22px 136px 1fr", gap: 8, padding: "7px 18px", borderTop: "1px dashed var(--line)", opacity: s, transform: `translateY(${(1 - s) * 8}px)` }}>
              <span style={{ color: "var(--muted)" }}>{e.seq}</span>
              <span style={{ fontWeight: 700, color }}>{e.kind}</span>
              <span style={{ color: "var(--muted)", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{e.detail}</span>
            </div>
          )
        })}
      </div>
    </AbsoluteFill>
  )
}
