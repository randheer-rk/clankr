import type { CSSProperties, ReactNode } from "react"
import { AbsoluteFill, Easing, interpolate, useCurrentFrame } from "remotion"

export const W = 1600
export const H = 900
export const FPS = 30
export const ease = Easing.bezier(0.16, 1, 0.3, 1)
export const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const

/** 0→1 between two frames, eased. */
export const t = (f: number, a: number, b: number, e = ease) => interpolate(f, [a, b], [0, 1], { ...clamp, easing: e })

const GRAIN = `url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='220' height='220'><filter id='n'><feTurbulence type='fractalNoise' baseFrequency='.85' numOctaves='2' stitchTiles='stitch'/><feColorMatrix values='0 0 0 0 1  0 0 0 0 1  0 0 0 0 1  0 0 0 .55 0'/></filter><rect width='100%' height='100%' filter='url(%23n)'/></svg>")`

/** A dark stage: a slow-breathing spotlight, a vignette and film grain. */
export function Stage({ children, glow = "#3a2a1e", x = 50, y = 42 }: { children: ReactNode; glow?: string; x?: number; y?: number }) {
  const f = useCurrentFrame()
  const breathe = 1 + Math.sin(f / 40) * 0.04
  return (
    <AbsoluteFill style={{ background: "#060608", overflow: "hidden", fontFamily: "'Instrument Sans', system-ui, sans-serif", color: "#f5f3ef" }}>
      <AbsoluteFill style={{ background: `radial-gradient(${46 * breathe}% ${58 * breathe}% at ${x}% ${y}%, ${glow}, transparent 70%)` }} />
      {children}
      <AbsoluteFill style={{ background: "radial-gradient(120% 90% at 50% 45%, transparent 55%, rgba(0,0,0,.7))", pointerEvents: "none" }} />
      <AbsoluteFill style={{ backgroundImage: GRAIN, opacity: 0.07, mixBlendMode: "overlay", pointerEvents: "none", transform: `translate(${(f * 37) % 9}px, ${(f * 53) % 7}px)` }} />
    </AbsoluteFill>
  )
}

/** Words that rise out of a blur, one after another, the way launch films set type. */
export function Words({ text, at, every = 3, size = 84, weight = 600, style, out }: { text: string; at: number; every?: number; size?: number; weight?: number; style?: CSSProperties; out?: number }) {
  const f = useCurrentFrame()
  const gone = out === undefined ? 0 : t(f, out, out + 18)
  return (
    <div style={{ fontSize: size, fontWeight: weight, letterSpacing: "-0.035em", lineHeight: 1.04, textAlign: "center", opacity: 1 - gone, filter: `blur(${gone * 12}px)`, ...style }}>
      {text.split(" ").map((w, i) => {
        const k = t(f, at + i * every, at + i * every + 22)
        return (
          <span key={i} style={{ display: "inline-block", whiteSpace: "pre", opacity: k, filter: `blur(${(1 - k) * 14}px)`, transform: `translateY(${(1 - k) * 0.32}em)` }}>
            {w}{i < text.split(" ").length - 1 ? " " : ""}
          </span>
        )
      })}
    </div>
  )
}

/** A soft band of light that sweeps across whatever it sits on. */
export function Sweep({ at, dur = 50, angle = 115, strength = 0.28 }: { at: number; dur?: number; angle?: number; strength?: number }) {
  const f = useCurrentFrame()
  const k = interpolate(f, [at, at + dur], [-60, 160], clamp)
  if (f < at || f > at + dur) return null
  return <div style={{ position: "absolute", inset: 0, pointerEvents: "none", background: `linear-gradient(${angle}deg, transparent ${k - 22}%, rgba(255,255,255,${strength}) ${k}%, transparent ${k + 22}%)`, mixBlendMode: "screen" }} />
}

/** The helper itself: a warm, breathing light. */
export function Orb({ size = 120, x = 0, y = 0, heat = 1 }: { size?: number; x?: number; y?: number; heat?: number }) {
  const f = useCurrentFrame()
  const s = 1 + Math.sin(f / 18) * 0.04
  return (
    <div style={{ position: "absolute", left: "50%", top: "50%", width: size, height: size, marginLeft: -size / 2, marginTop: -size / 2, transform: `translate(${x}px, ${y}px) scale(${s})`, borderRadius: "50%",
      background: "radial-gradient(circle at 38% 34%, #fff6ea 0%, #ffc08a 18%, #ff7a3d 46%, #c2410c 78%, #7a2408 100%)",
      boxShadow: `0 0 ${60 * heat}px ${18 * heat}px rgba(255,122,61,.45), 0 0 ${180 * heat}px ${60 * heat}px rgba(255,122,61,.18)` }} />
  )
}

/** A small caption, set like a launch film's lower third. */
export function Lower({ text, at, out, style }: { text: string; at: number; out?: number; style?: CSSProperties }) {
  const f = useCurrentFrame()
  const k = t(f, at, at + 24) * (out === undefined ? 1 : 1 - t(f, out, out + 18))
  return <div style={{ position: "absolute", left: 0, right: 0, textAlign: "center", fontSize: 26, color: "rgba(245,243,239,.62)", letterSpacing: "0.01em", opacity: k, transform: `translateY(${(1 - k) * 10}px)`, ...style }}>{text}</div>
}
