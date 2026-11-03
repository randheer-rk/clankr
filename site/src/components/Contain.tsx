import { useEffect, useRef, useState } from "react"
import { AnimatePresence, motion } from "motion/react"

const CLASSES = [
  { id: "container", walls: 1, color: "var(--ink)", what: "namespaces, seccomp, read-only root" },
  { id: "gvisor", walls: 2, color: "var(--slate)", what: "a user-space kernel in between" },
  { id: "microvm", walls: 3, color: "var(--moss)", what: "its own kernel, in Firecracker" },
] as const

/** Pick a runtime class, then try to drag the agent out of its box. */
export function Contain() {
  const [cls, setCls] = useState<(typeof CLASSES)[number]>(CLASSES[1])
  const box = useRef<HTMLDivElement>(null)
  const [tries, setTries] = useState(0)
  const [pings, setPings] = useState<{ id: number; angle: number }[]>([])

  // every so often the agent tries the network and hits the wall
  useEffect(() => {
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return
    const t = setInterval(() => setPings(p => [...p.slice(-3), { id: Date.now(), angle: Math.random() * Math.PI * 2 }]), 1700)
    return () => clearInterval(t)
  }, [])

  return (
    <div className="contain">
      <div className="seg" role="group" aria-label="Runtime class">
        {CLASSES.map(c => (
          <button key={c.id} aria-pressed={c.id === cls.id} onClick={() => setCls(c)}>
            {c.id === cls.id && <motion.span layoutId="seg-pill" className="pill" style={{ inset: 0 }} transition={{ type: "spring", stiffness: 420, damping: 34 }} />}
            <span style={{ position: "relative" }}>{c.id}</span>
          </button>
        ))}
      </div>

      <div className="room" ref={box}>
        {Array.from({ length: 3 }).map((_, i) => (
          <motion.div key={i} className="wall" initial={false}
            animate={{ opacity: i < cls.walls ? 1 : 0, scale: i < cls.walls ? 1 : 0.92, borderColor: cls.color }}
            transition={{ type: "spring", stiffness: 260, damping: 24, delay: i * 0.05 }}
            style={{ inset: i * 16, borderStyle: i === 2 ? "dashed" : "solid", borderRadius: 34 - i * 8 }} />
        ))}
        <AnimatePresence>
          {pings.map(p => (
            <motion.span key={p.id} className="ping" initial={{ x: 0, y: 0, opacity: 1 }}
              animate={{ x: [0, Math.cos(p.angle) * 96, Math.cos(p.angle) * 84], y: [0, Math.sin(p.angle) * 70, Math.sin(p.angle) * 60], opacity: [1, 1, 0] }}
              exit={{ opacity: 0 }} transition={{ duration: 1.1, times: [0, 0.7, 1], ease: "easeOut" }} />
          ))}
        </AnimatePresence>
        <motion.button className="agent" aria-label="The agent: drag it" drag dragConstraints={box} dragElastic={0.18}
          dragSnapToOrigin whileDrag={{ scale: 1.15 }} onDragEnd={() => setTries(t => t + 1)}
          dragTransition={{ bounceStiffness: 500, bounceDamping: 18 }} />
      </div>

      <div className="caption">
        <AnimatePresence mode="wait">
          <motion.p key={cls.id} initial={{ opacity: 0, y: 6 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0, y: -6 }} transition={{ duration: 0.2 }}>
            <b className="mono" style={{ color: cls.color }}>class: {cls.id}</b> · {cls.what} · <span className="mono">network: none</span>
          </motion.p>
        </AnimatePresence>
        <p className="tries mono">{tries === 0 ? "drag the agent; try to get it out" : `escape attempts: ${tries} · escapes: 0`}</p>
      </div>
    </div>
  )
}
