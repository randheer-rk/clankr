import { useCurrentFrame, useVideoConfig } from 'remotion'
import { C, EventBar, mono, panel, Rail, Stage, t } from './kit'

export const LOGBOOK_FRAMES = 390

export function Logbook() {
  const f = useCurrentFrame(), { width } = useVideoConfig(), compact = width < 1000
  const phase = f < 120 ? 0 : f < 225 ? 1 : 2
  const nodes = compact ? [[48, 252, 704, 132], [48, 443, 704, 132], [48, 634, 704, 132]] : [[72, 292, 390, 272], [605, 292, 390, 272], [1138, 292, 390, 272]]
  const route = compact ? [[400, 384, 400, 443], [400, 575, 400, 634]] : [[462, 428, 605, 428], [995, 428, 1138, 428]]
  const data = [
    ['FIRST COMPUTER', 'Agent / writer', phase === 0 ? 'eu-1 · running' : 'eu-1 · offline', phase === 0 ? '●' : '×'],
    ['SAVED PROGRESS', 'Step 14', 'draft saved', '▤'],
    ['SECOND COMPUTER', 'Agent / writer', phase === 2 ? 'eu-2 · resumed' : 'eu-2 · ready', phase === 2 ? '✓' : '○'],
  ]
  return <Stage title="The work survives the machine." subtitle="Another computer picks up the same job, right where it stopped." phase={phase} phases={['Run', 'Recover', 'Continue']}>
    {route.map(([x, y, xx, yy], i) => <Rail key={i} from={[x, y]} to={[xx, yy]} progress={i === 0 && phase === 0 ? t(f, 35, 90) : i === 1 && phase === 2 ? t(f, 225, 265) : undefined} />)}
    {nodes.map(([x, y, w, h], i) => <div key={i} style={{ ...panel, position: 'absolute', left: x, top: y, width: w, height: h, padding: compact ? '20px 28px' : 32, borderColor: i === 1 ? C.yellow : i === 0 && phase > 0 ? C.red : i === 2 && phase === 2 ? C.blue : C.line, opacity: i === 2 && phase < 2 ? .55 : 1 }}>
      <div style={{ color: C.muted, fontFamily: mono, fontSize: compact ? 20 : 18, letterSpacing: 1 }}>{data[i][0]}</div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 16, marginTop: compact ? 9 : 29, fontSize: compact ? 35 : 43, fontWeight: 550, letterSpacing: -1 }}><span style={{ color: i === 0 && phase > 0 ? C.red : i === 2 && phase === 2 ? C.blue : C.yellow, fontSize: 34 }}>{data[i][3]}</span>{data[i][1]}</div>
      <div style={{ fontFamily: mono, color: C.muted, fontSize: compact ? 23 : 22, marginTop: compact ? 7 : 24 }}>{data[i][2]}</div>
    </div>)}
    <EventBar event={phase === 0 ? 'Step recorded' : phase === 1 ? 'Computer crashed' : 'Work resumed'} detail={phase === 0 ? 'step 14' : phase === 1 ? 'eu-1' : 'eu-2'} success={phase === 2} />
  </Stage>
}
