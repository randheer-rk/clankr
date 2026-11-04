import { AbsoluteFill, interpolate, useCurrentFrame } from "remotion"
import { clamp, Lower, Stage, t, Words } from "./kit"

export const LOGBOOK_FRAMES = 400

// a morning in a small shop, as the helper wrote it down
const ROWS: { at: number; time: string; text: string; tone?: "bad" | "good" | "hot" }[] = [
  { at: 40, time: "09:30", text: "Read this morning's orders" },
  { at: 62, time: "09:31", text: "Wrote a thank-you note to Ana" },
  { at: 84, time: "09:33", text: "Checked stock for the blue mug" },
  { at: 106, time: "09:35", text: "Drafted the spring newsletter" },
  { at: 128, time: "09:36", text: "Asked the editor for a second look" },
  { at: 204, time: "09:41", text: "The server restarted", tone: "bad" },
  { at: 236, time: "09:41", text: "Picked up right where it stopped", tone: "good" },
  { at: 262, time: "09:42", text: "Editor: “Reads well. One typo, fixed.”" },
  { at: 288, time: "09:44", text: "Waiting for your OK to send", tone: "hot" },
]
const ROW_H = 84

function Row({ r, f }: { r: (typeof ROWS)[number]; f: number }) {
  const k = t(f, r.at, r.at + 20)
  const write = interpolate(f, [r.at + 4, r.at + 30], [0, 100], clamp)
  const glow = Math.max(0, 1 - Math.abs(f - (r.at + 16)) / 24)
  const color = r.tone === "bad" ? "#ff8a7a" : r.tone === "good" ? "#8fe0ad" : r.tone === "hot" ? "#ffb37a" : "#f5f3ef"
  return (
    <div style={{ height: ROW_H, display: "flex", alignItems: "center", gap: 34, padding: "0 46px", opacity: k, borderTop: "1px solid rgba(255,255,255,.08)",
      background: `linear-gradient(90deg, rgba(255,255,255,${0.05 * glow}), transparent 70%)` }}>
      <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 22, color: "rgba(245,243,239,.45)", width: 90 }}>{r.time}</span>
      <span style={{ width: 12, height: 12, borderRadius: 6, background: color, boxShadow: `0 0 ${10 + glow * 24}px ${color}` }} />
      <span style={{ fontSize: 33, fontWeight: 500, letterSpacing: "-0.01em", color, clipPath: `inset(0 ${100 - write}% 0 0)` }}>{r.text}</span>
    </div>
  )
}

export function Logbook() {
  const f = useCurrentFrame()
  const fadeIn = t(f, 0, 26), fadeOut = 1 - t(f, 376, 400)
  // the power cut: a flicker, near darkness, then the lights come back
  const cut = f >= 176 && f < 214
  const flick = f < 176 ? 1 : f < 186 ? (f % 3 === 0 ? 0.25 : 0.85) : f < 206 ? 0.08 : interpolate(f, [206, 222], [0.08, 1], clamp)
  // camera: the sheet slides up as it fills, the view drifts and pushes in
  // how many lines are written, as a smooth number, so the sheet glides rather than jumps
  const written = ROWS.reduce((n, r) => n + t(f, r.at - 6, r.at + 18), 0)
  const lift = interpolate(f, [0, 400], [0, 1], clamp)
  const pull = t(f, 316, 360)

  return (
    <Stage glow="#1d2633" x={46} y={40}>
      <AbsoluteFill style={{ opacity: fadeIn * fadeOut, filter: `brightness(${flick})` }}>
        <AbsoluteFill style={{ perspective: 1500 }}>
          <div style={{
            position: "absolute", left: "50%", top: 120, width: 1100, marginLeft: -550, transformOrigin: "50% 0%",
            transform: `translateY(${-lift * 40 - pull * 40}px) rotateX(${interpolate(f, [0, 400], [40, 30], clamp)}deg) rotateZ(${interpolate(f, [0, 400], [-6, -2], clamp)}deg) scale(${interpolate(f, [0, 316], [0.92, 1], clamp) * (1 - pull * 0.12)})`,
          }}>
            <div style={{ borderRadius: 34, overflow: "hidden", border: "1.5px solid rgba(255,255,255,.18)", background: "linear-gradient(170deg, rgba(255,255,255,.09), rgba(255,255,255,.02) 60%)", boxShadow: "0 60px 120px -40px rgba(0,0,0,.8), inset 0 1px 0 rgba(255,255,255,.2)" }}>
              <div style={{ padding: "30px 46px", display: "flex", justifyContent: "space-between", fontFamily: "'JetBrains Mono', monospace", fontSize: 20, color: "rgba(245,243,239,.5)", letterSpacing: ".06em" }}>
                <span>LOGBOOK · SPRING NEWSLETTER</span>
                <span style={{ color: cut ? "#ff8a7a" : "#8fe0ad" }}>● {cut ? "offline" : "recording"}</span>
              </div>
              <div style={{ height: ROW_H * 6, overflow: "hidden", position: "relative" }}>
                <div style={{ transform: `translateY(${-Math.max(0, written - 6) * ROW_H}px)`, transition: "none" }}>
                  {ROWS.map(r => <Row key={r.at} r={r} f={f} />)}
                </div>
                <div style={{ position: "absolute", left: 0, right: 0, top: 0, height: 70, background: "linear-gradient(rgba(10,12,16,.6), transparent)" }} />
              </div>
            </div>
          </div>
        </AbsoluteFill>
        <div style={{ position: "absolute", left: 0, right: 0, top: 690 }}>
          <Words text="It writes everything down." at={52} size={58} out={160} />
        </div>
        <div style={{ position: "absolute", left: 0, right: 0, top: 690 }}>
          <Words text="Even when the lights go out." at={222} size={58} out={306} />
        </div>
        <div style={{ position: "absolute", left: 0, right: 0, top: 660 }}>
          <Words text="A logbook that never forgets." at={322} size={86} />
        </div>
        <Lower text="If anything stops, it starts again from the last line. Nothing is done twice." at={340} style={{ top: 776 }} />
      </AbsoluteFill>
    </Stage>
  )
}
