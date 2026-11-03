import { useRef, useState } from "react"
import { AnimatePresence, animate, motion, useMotionValue, useTransform } from "motion/react"

const HOLD = 1.1 // seconds

/** An approval waits for a person: press and hold to approve; it lands in the audit log. */
export function Approve() {
  const progress = useMotionValue(0)
  const dash = useTransform(progress, p => `${p * 100} 100`)
  const anim = useRef<ReturnType<typeof animate> | null>(null)
  const [done, setDone] = useState<null | "approved" | "rejected">(null)
  const [log, setLog] = useState<{ id: number; text: string; tone: string }[]>([
    { id: 1, text: "approval requested · publish-post · run 7ef6", tone: "muted" },
  ])

  const decide = (d: "approved" | "rejected") => {
    setDone(d)
    const t = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    setLog(l => [...l, { id: Date.now(), text: `${d === "approved" ? "approve" : "reject"} · publish-post · by you · ${t}`, tone: d === "approved" ? "good" : "bad" }])
  }
  const start = () => {
    if (done) return
    anim.current = animate(progress, 1, { duration: HOLD * (1 - progress.get()), ease: "linear", onComplete: () => decide("approved") })
  }
  const stop = () => {
    if (done) return
    anim.current?.stop()
    animate(progress, 0, { type: "spring", stiffness: 300, damping: 30 })
  }
  const reset = () => {
    setDone(null)
    progress.set(0)
    setLog(l => l.slice(0, 1))
  }

  return (
    <div className="approve">
      <motion.div className="card ask" layout>
        <div className="who">
          <i>e</i>
          <div><b>blog / publish-post</b><span className="mono">run 7ef6 · waiting 4m · group: editors</span></div>
          <AnimatePresence>{done && <motion.span className={`stamp ${done}`} initial={{ scale: 1.6, opacity: 0, rotate: -12 }} animate={{ scale: 1, opacity: 1, rotate: -6 }} transition={{ type: "spring", stiffness: 400, damping: 18 }}>{done}</motion.span>}</AnimatePresence>
        </div>
        <p>Publish “Durable agents, explained” to the company blog.</p>
        <div className="actions">
          {done ? (
            <button className="btn ghost" onClick={reset}>reset</button>
          ) : (
            <>
              <motion.button className="btn hold" onPointerDown={start} onPointerUp={stop} onPointerLeave={stop}
                onKeyDown={e => (e.key === " " || e.key === "Enter") && !e.repeat && start()} onKeyUp={e => (e.key === " " || e.key === "Enter") && stop()}
                whileTap={{ scale: 0.97 }} aria-label="Hold to approve">
                <svg viewBox="0 0 36 36" width="22" height="22" aria-hidden="true">
                  <circle cx="18" cy="18" r="15.9" fill="none" stroke="color-mix(in srgb, currentColor 30%, transparent)" strokeWidth="3" />
                  <motion.circle cx="18" cy="18" r="15.9" fill="none" stroke="currentColor" strokeWidth="3" pathLength={100} strokeDasharray={dash} transform="rotate(-90 18 18)" strokeLinecap="round" />
                </svg>
                hold to approve
              </motion.button>
              <button className="btn ghost" onClick={() => decide("rejected")}>reject</button>
            </>
          )}
        </div>
      </motion.div>
      <ol className="audit mono" aria-label="Audit log">
        <AnimatePresence initial={false}>
          {log.map(r => (
            <motion.li key={r.id} className={r.tone} initial={{ opacity: 0, y: -8, height: 0 }} animate={{ opacity: 1, y: 0, height: "auto" }} exit={{ opacity: 0 }} transition={{ type: "spring", stiffness: 380, damping: 32 }}>
              <span>audit</span>{r.text}
            </motion.li>
          ))}
        </AnimatePresence>
      </ol>
    </div>
  )
}
