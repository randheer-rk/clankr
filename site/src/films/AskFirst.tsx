import { useCurrentFrame, useVideoConfig } from 'remotion'
import { C, EventBar, mono, panel, Stage, t } from './kit'

export const ASK_FRAMES = 390

export function AskFirst() {
  const f = useCurrentFrame(), { width } = useVideoConfig(), compact = width < 1000
  const phase = f < 150 ? 0 : f < 255 ? 1 : 2
  const approved = phase > 0
  return <Stage title="Some actions need a person." subtitle="The agent waits. The decision is kept on record." phase={phase} phases={['Request', 'Approve', 'Record']}>
    <div style={{ ...panel, position: 'absolute', left: compact ? 48 : 72, top: compact ? 245 : 250, width: compact ? 704 : 880, height: compact ? 425 : 420, padding: 30 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', fontFamily: mono, fontSize: compact ? 23 : 22, color: C.yellow }}><span>Publish post</span><span style={{ fontSize: compact ? 20 : 18, color: approved ? C.blue : C.muted }}>{approved ? '✓ APPROVED' : 'AWAITING REVIEW'}</span></div>
      <div style={{ fontSize: compact ? 42 : 48, lineHeight: 1.15, fontWeight: 550, letterSpacing: -1.5, marginTop: 20 }}>Publish the weekly update</div>
      <div style={{ fontSize: compact ? 25 : 26, lineHeight: 1.5, color: approved ? C.blue : C.muted, marginTop: 18 }}>{approved ? '“Reviewed and ready to publish.”' : 'Approver group: editors · Waiting for a decision'}</div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 25, marginTop: 24 }}><div style={{ padding: compact ? '17px 28px' : '15px 30px', background: approved ? C.blue : C.text, color: C.card, borderRadius: 8, fontSize: compact ? 27 : 26, transform: `scale(${1 - .025 * Math.sin(t(f, 142, 158) * Math.PI)})` }}>{approved ? '✓ Approved' : 'Approve'}</div><span style={{ color: C.muted, fontSize: compact ? 24 : 23 }}>{approved ? 'by editor' : 'Reject'}</span></div>
    </div>
    <div style={{ position: 'absolute', left: compact ? 48 : 1055, right: 72, top: compact ? 705 : 290, display: 'flex', flexDirection: compact ? 'row' : 'column', gap: compact ? 24 : 42 }}>
      {['Agent asks', 'Human approved', 'Audit recorded'].map((label, i) => <div key={label} style={{ display: 'flex', alignItems: 'center', gap: compact ? 12 : 20, opacity: i <= phase ? 1 : .4 }}><span style={{ width: compact ? 36 : 46, height: compact ? 36 : 46, borderRadius: '50%', display: 'grid', placeItems: 'center', flex: 'none', border: `2px solid ${i <= phase ? C.blue : C.line}`, background: i <= phase ? C.blue : 'transparent', color: i <= phase ? C.card : C.muted, fontSize: compact ? 23 : 25 }}>{i <= phase ? '✓' : i + 1}</span><span style={{ fontSize: compact ? 23 : 29, fontWeight: 500 }}>{compact ? ['Requested', 'Approved', 'Recorded'][i] : label}</span></div>)}
    </div>
    <EventBar event={phase === 0 ? 'Approval requested' : phase === 1 ? 'Approval given' : 'Workflow continued'} detail={compact ? '' : phase === 0 ? 'waiting for editors' : phase === 1 ? 'actor: editor' : 'decision recorded'} success={approved} />
  </Stage>
}
