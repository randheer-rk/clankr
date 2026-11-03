import { useEffect, useRef, useState } from "react"
import { motion, useSpring } from "motion/react"

/** The monoline padlock superellipse body, as in docs/assets (n = 4.4). */
function squircle(cx: number, cy: number, rx: number, ry: number, n = 4.4, steps = 96) {
  const p: string[] = []
  for (let i = 0; i < steps; i++) {
    const t = (2 * Math.PI * i) / steps, c = Math.cos(t), s = Math.sin(t)
    p.push(`${(cx + rx * Math.sign(c) * Math.abs(c) ** (2 / n)).toFixed(2)} ${(cy + ry * Math.sign(s) * Math.abs(s) ** (2 / n)).toFixed(2)}`)
  }
  return "M" + p.join(" L") + " Z"
}
const BODY = squircle(60, 80, 44, 30)

/**
 * The Clankr mark, alive: its eyes follow the pointer and it blinks now and then.
 * `watch` turns the tracking off (a static mark).
 */
export function Lockbot({ size = 32, watch = true, stroke = 8 }: { size?: number; watch?: boolean; stroke?: number }) {
  const ref = useRef<SVGSVGElement>(null)
  const x = useSpring(0, { stiffness: 260, damping: 22 })
  const y = useSpring(0, { stiffness: 260, damping: 22 })
  const [blink, setBlink] = useState(false)

  useEffect(() => {
    if (!watch) return
    const move = (e: PointerEvent) => {
      const r = ref.current?.getBoundingClientRect()
      if (!r) return
      const dx = e.clientX - (r.left + r.width / 2), dy = e.clientY - (r.top + r.height * 0.66)
      const d = Math.hypot(dx, dy) || 1
      const k = Math.min(1, d / 260)
      x.set((dx / d) * 6 * k)
      y.set((dy / d) * 4 * k)
    }
    addEventListener("pointermove", move)
    return () => removeEventListener("pointermove", move)
  }, [watch, x, y])

  useEffect(() => {
    if (!watch || matchMedia("(prefers-reduced-motion: reduce)").matches) return
    let t: ReturnType<typeof setTimeout>
    const next = () => {
      t = setTimeout(() => {
        setBlink(true)
        setTimeout(() => setBlink(false), 130)
        next()
      }, 2600 + Math.random() * 3800)
    }
    next()
    return () => clearTimeout(t)
  }, [watch])

  return (
    <svg ref={ref} viewBox="8 6 104 112" width={size} height={size * 112 / 104} aria-hidden="true" style={{ overflow: "visible" }}>
      <path d="M37 52 V37 a23 23 0 0 1 46 0 V52" fill="none" stroke="currentColor" strokeWidth={stroke} strokeLinecap="round" />
      <path d={BODY} fill="none" stroke="currentColor" strokeWidth={stroke} />
      <motion.g style={{ x, y }}>
        {[44, 68].map(ex => (
          <motion.rect key={ex} x={ex} y={71} width={8} height={19} rx={4} fill="var(--ember)"
            animate={{ scaleY: blink ? 0.12 : 1 }} transition={{ duration: 0.08 }} style={{ originY: "50%", transformBox: "fill-box" }} />
        ))}
      </motion.g>
    </svg>
  )
}
