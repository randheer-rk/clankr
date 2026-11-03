import { useEffect, useState } from "react"
import { AnimatePresence, animate, motion, useMotionValue, useTransform } from "motion/react"

const BUDGET = 10_000
const SPENT = 3_812

function Num({ value }: { value: number }) {
  const mv = useMotionValue(value)
  const text = useTransform(mv, v => Math.round(v).toLocaleString("en-US"))
  useEffect(() => {
    const c = animate(mv, value, { type: "spring", stiffness: 200, damping: 26 })
    return () => c.stop()
  }, [value, mv])
  return <motion.span>{text}</motion.span>
}

/** Drag how many tokens a call asks for: the gateway reserves them, or refuses. */
export function Broker() {
  const [ask, setAsk] = useState(1_200)
  const ok = SPENT + ask <= BUDGET
  const pct = (n: number) => `${(n / BUDGET) * 100}%`

  return (
    <div className="broker">
      <div className="card call">
        <p className="cap">agent</p>
        <pre>{`await ctx.models.generate(
  "default", messages,
  { maxOutputTokens: `}<b>{ask.toLocaleString("en-US")}</b>{` })`}</pre>
        <input type="range" min={100} max={9000} step={50} value={ask} onChange={e => setAsk(+e.target.value)} aria-label="Tokens the call asks for" />
      </div>

      <motion.div className="wire" aria-hidden="true">
        <motion.i animate={{ x: ["-10%", "110%"] }} transition={{ duration: 1.4, repeat: Infinity, ease: "linear" }} style={{ background: ok ? "var(--ember)" : "var(--bad)" }} />
      </motion.div>

      <div className="card gate">
        <div className="gate-top">
          <p className="cap">gateway</p>
          <AnimatePresence mode="popLayout">
            <motion.span key={ok ? "ok" : "no"} className={`verdict ${ok ? "yes" : "no"}`} initial={{ scale: 0.6, opacity: 0 }} animate={{ scale: 1, opacity: 1 }} exit={{ scale: 0.6, opacity: 0 }} transition={{ type: "spring", stiffness: 500, damping: 26 }}>
              {ok ? "reserved" : "refused: over budget"}
            </motion.span>
          </AnimatePresence>
        </div>
        <dl>
          <dt>claim</dt><dd>default → your model</dd>
          <dt>key</dt><dd><span className="shimmer">sk-ant-••••••••</span> <span className="muted">stays here</span></dd>
          <dt>budget</dt>
          <dd>
            <div className="meter">
              <span className="spent" style={{ width: pct(SPENT) }} />
              <motion.span className={`ask ${ok ? "" : "over"}`} animate={{ width: pct(Math.min(ask, BUDGET - SPENT + 400)) }} transition={{ type: "spring", stiffness: 260, damping: 30 }} style={{ left: pct(SPENT) }} />
              <span className="cap-line" />
            </div>
            <span className="mono small"><Num value={SPENT + (ok ? ask : 0)} /> / 10,000 tokens</span>
          </dd>
        </dl>
      </div>
    </div>
  )
}
