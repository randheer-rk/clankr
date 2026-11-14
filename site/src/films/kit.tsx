import type { CSSProperties, ReactNode } from 'react'
import { AbsoluteFill, Easing, interpolate, useVideoConfig } from 'remotion'

export const W = 1600, H = 900, FPS = 30
export const ease = Easing.bezier(.2, .7, .2, 1)
export const clamp = { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' } as const
export const t = (frame: number, from: number, to: number) => interpolate(frame, [from, to], [0, 1], { ...clamp, easing: ease })
export const C = { bg: '#1A1C2C', card: '#242740', text: '#FFF1E8', yellow: '#FFD23F', muted: '#A3A7C2', line: '#383C5C', blue: '#29ADFF', red: '#FF004D' }
export const mono = "'JetBrains Mono', ui-monospace, monospace"
export const panel: CSSProperties = { border: `2px solid ${C.line}`, background: C.card, borderRadius: 0 }

export function Stage({ title, subtitle, phase, phases, children }: { title: string; subtitle: string; phase: number; phases: string[]; children: ReactNode }) {
  const { width } = useVideoConfig()
  const compact = width < 1000, pad = compact ? 48 : 72
  return <AbsoluteFill style={{ background: C.bg, color: C.text, fontFamily: "'Rubik', system-ui, sans-serif", lineHeight: 1.35, overflow: 'hidden' }}>
    <div style={{ position: 'absolute', left: pad, right: pad, top: compact ? 42 : 48 }}>
      <div style={{ fontFamily: mono, fontSize: compact ? 20 : 18, color: C.yellow, letterSpacing: 2 }}>CLANKR / IN PRACTICE <span style={{ float: 'right', letterSpacing: 1 }}>ILLUSTRATIVE DEMO</span></div>
      <h3 style={{ margin: '22px 0 12px', fontFamily: "'Jersey 15', sans-serif", fontSize: compact ? 76 : 88, lineHeight: .95, fontWeight: 400, letterSpacing: 1, textTransform: 'uppercase' }}>{title}</h3>
      <p style={{ margin: 0, fontSize: compact ? 27 : 28, color: C.muted, lineHeight: 1.4 }}>{subtitle}</p>
    </div>
    {children}
    <div style={{ position: 'absolute', left: pad, right: pad, bottom: 42, display: 'flex', gap: compact ? 16 : 30 }}>
      {phases.map((name, i) => <div key={name} style={{ flex: 1, borderTop: `3px solid ${i <= phase ? C.yellow : C.line}`, paddingTop: 18, color: i === phase ? C.text : C.muted, fontFamily: mono, fontSize: compact ? 21 : 23 }}><span style={{ opacity: .6, marginRight: 12 }}>0{i + 1}</span>{name}</div>)}
    </div>
  </AbsoluteFill>
}

export function Rail({ from, to, progress, blocked = false }: { from: [number, number]; to: [number, number]; progress?: number; blocked?: boolean }) {
  return <svg style={{ position: 'absolute', inset: 0, width: '100%', height: '100%', overflow: 'visible' }} aria-hidden="true">
    <path d={`M${from.join(',')} L${to.join(',')}`} fill="none" stroke={blocked ? C.red : C.line} strokeWidth="3" strokeDasharray={blocked ? '7 7' : undefined} />
    {progress !== undefined && <circle cx={from[0] + (to[0] - from[0]) * progress} cy={from[1] + (to[1] - from[1]) * progress} r="7" fill={blocked ? C.red : C.yellow} />}
  </svg>
}

export function EventBar({ event, detail, success = false }: { event: string; detail: string; success?: boolean }) {
  const { width } = useVideoConfig(), compact = width < 1000
  return <div style={{ position: 'absolute', left: compact ? 48 : 72, right: compact ? 48 : 72, bottom: compact ? 134 : 132, height: compact ? 80 : 88, background: '#0D0E16', color: C.text, border: `2px solid ${C.line}`, display: 'flex', alignItems: 'center', gap: 20, padding: '0 26px', fontFamily: mono }}>
    <span style={{ color: success ? C.blue : C.yellow, fontSize: compact ? 26 : 30 }}>{success ? '✓' : '›'}</span>
    <span style={{ fontSize: compact ? 26 : 29 }}>{event}</span>
    <span style={{ marginLeft: 'auto', fontSize: compact ? 21 : 23, color: C.muted }}>{detail}</span>
  </div>
}
