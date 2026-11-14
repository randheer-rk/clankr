import { useCurrentFrame, useVideoConfig } from 'remotion'
import { C, EventBar, mono, panel, Rail, Stage, t } from './kit'

export const ROOM_FRAMES = 390

export function Room() {
  const f = useCurrentFrame(), { width } = useVideoConfig(), compact = width < 1000
  const phase = f < 120 ? 0 : f < 240 ? 1 : 2
  const boundary = compact ? { left: 48, top: 245, width: 704, height: 225 } : { left: 72, top: 264, width: 570, height: 344 }
  const gateway = compact ? { left: 48, top: 549, width: 320, height: 204 } : { left: 800, top: 302, width: 300, height: 268 }
  const provider = compact ? { left: 432, top: 549, width: 320, height: 204 } : { left: 1216, top: 302, width: 312, height: 268 }
  const outgoing: [[number, number], [number, number]] = compact ? [[208, 470], [208, 549]] : [[642, 436], [800, 436]]
  const allowed: [[number, number], [number, number]] = compact ? [[368, 651], [432, 651]] : [[1100, 436], [1216, 436]]
  return <Stage title="Every agent gets a boundary." subtitle="Each agent works in a sealed box. Whatever it asks for is checked first." phase={phase} phases={['Isolate', 'Check', 'Broker']}>
    <Rail from={outgoing[0]} to={outgoing[1]} progress={phase === 1 ? t(f, 120, 153) : phase === 2 ? t(f, 240, 265) : undefined} blocked={phase === 1} />
    <Rail from={allowed[0]} to={allowed[1]} progress={phase === 2 ? t(f, 270, 305) : undefined} />
    <div style={{ ...boundary, position: 'absolute', border: `2px dashed ${C.yellow}`, borderRadius: 20, padding: compact ? '22px 28px' : 32 }}>
      <div style={{ fontFamily: mono, color: C.yellow, fontSize: compact ? 21 : 18, letterSpacing: 1 }}>SEALED BOX</div>
      <div style={{ ...panel, marginTop: compact ? 15 : 28, padding: compact ? '15px 22px' : '26px 28px', borderRadius: 12 }}><div style={{ fontSize: compact ? 36 : 44, fontWeight: 550, letterSpacing: -1 }}>Agent / writer</div><div style={{ fontFamily: mono, fontSize: compact ? 24 : 23, marginTop: 12, color: C.muted }}>working alone</div></div>
      {!compact && <div style={{ marginTop: 24, fontSize: 23, color: C.muted }}>nothing in or out unless allowed</div>}
    </div>
    <div style={{ ...panel, ...gateway, position: 'absolute', padding: compact ? 24 : 28, borderColor: phase === 1 ? C.red : C.yellow }}><div style={{ fontFamily: mono, fontSize: compact ? 20 : 18, color: C.yellow }}>THE GATE</div><div style={{ fontSize: compact ? 34 : 38, lineHeight: 1.2, marginTop: 20, fontWeight: 550 }}>{phase === 1 ? '× Denied' : phase === 2 ? '✓ Granted' : 'Check request'}</div><div style={{ fontSize: compact ? 24 : 23, marginTop: 18, color: C.muted }}>{phase === 1 ? 'Unknown website' : 'Limited permissions'}</div></div>
    <div style={{ ...panel, ...provider, position: 'absolute', padding: compact ? 24 : 28, opacity: phase === 2 ? 1 : .5, borderColor: phase === 2 ? C.blue : C.line }}><div style={{ fontFamily: mono, fontSize: compact ? 20 : 18, color: C.muted }}>AI MODEL</div><div style={{ fontSize: compact ? 34 : 38, marginTop: 20, fontWeight: 550 }}>Model call</div><div style={{ fontSize: compact ? 24 : 23, color: C.muted, marginTop: 18 }}>Keys stay at the gate</div></div>
    <EventBar event={phase === 0 ? 'Sandbox active' : phase === 1 ? 'Request blocked' : 'AI model call allowed'} detail={compact ? '' : phase === 2 ? 'claim: default' : 'policy enforced'} success={phase === 2} />
  </Stage>
}
