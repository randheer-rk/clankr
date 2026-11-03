import { useEffect, useRef } from "react"

type Box = { x: number; y: number; s: number; hue: string; depth: number; rot: number; flash: number; dash: boolean; ox: number; oy: number }
type Msg = { a: Box; b: Box; t0: number; dur: number; done?: boolean }

/**
 * The hero's backdrop: little sandboxes, each with an agent inside, passing messages.
 * Boxes near the pointer drift out of its way; the text column is kept clear.
 */
export function Field() {
  const ref = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    const canvas = ref.current!
    const ctx = canvas.getContext("2d")!
    const still = matchMedia("(prefers-reduced-motion: reduce)").matches
    const css = getComputedStyle(document.documentElement)
    const hues = ["--ember", "--moss", "--slate", "--ochre", "--muted"].map(v => css.getPropertyValue(v).trim())
    const ink = css.getPropertyValue("--ink").trim()
    let boxes: Box[] = [], msgs: Msg[] = [], w = 0, h = 0, raf = 0, last = 0
    const ptr = { x: -9999, y: -9999 }

    const layout = () => {
      const dpr = Math.min(devicePixelRatio || 1, 2)
      const r = canvas.getBoundingClientRect()
      w = r.width
      h = r.height
      canvas.width = w * dpr
      canvas.height = h * dpr
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
      const col = Math.min(820, w - 32), l = (w - col) / 2, rr = l + col
      const n = Math.round((w * h) / 9000)
      boxes = []
      for (let i = 0; i < n * 4 && boxes.length < n; i++) {
        const x = Math.random() * w, y = Math.random() * h
        // the text column stays clear, except near the very top and bottom
        if (w > 760 && x > l - 30 && x < rr + 30 && y > h * 0.08 && y < h * 0.9) continue
        if (w <= 760 && y > h * 0.12 && y < h * 0.86) continue
        boxes.push({ x, y, s: 10 + Math.random() * 16, hue: hues[(Math.random() * hues.length) | 0], depth: 0.3 + Math.random() * 0.6, rot: (Math.random() - 0.5) * 0.5, flash: 0, dash: Math.random() < 0.35, ox: 0, oy: 0 })
      }
    }

    const round = (x: number, y: number, s: number, r: number) => {
      ctx.beginPath()
      ctx.moveTo(x + r, y)
      ctx.arcTo(x + s, y, x + s, y + s, r)
      ctx.arcTo(x + s, y + s, x, y + s, r)
      ctx.arcTo(x, y + s, x, y, r)
      ctx.arcTo(x, y, x + s, y, r)
      ctx.closePath()
    }
    const pos = (b: Box) => [b.x + b.ox, b.y + b.oy]

    const draw = (t: number) => {
      ctx.clearRect(0, 0, w, h)
      for (const b of boxes) {
        // drift away from the pointer, spring back home
        const dx = b.x - ptr.x, dy = b.y - ptr.y, d = Math.hypot(dx, dy)
        const push = d < 140 ? (140 - d) / 140 : 0
        b.ox += ((dx / (d || 1)) * push * 34 - b.ox) * 0.12
        b.oy += ((dy / (d || 1)) * push * 34 - b.oy) * 0.12
        const [x, y] = pos(b)
        ctx.save()
        ctx.translate(x, y)
        ctx.rotate(b.rot + b.ox * 0.01)
        ctx.globalAlpha = 0.22 + b.depth * 0.45
        ctx.strokeStyle = b.hue
        ctx.lineWidth = 1.4
        ctx.setLineDash(b.dash ? [3, 3] : [])
        round(-b.s / 2, -b.s / 2, b.s, b.s * 0.3)
        ctx.stroke()
        ctx.globalAlpha = Math.min(1, 0.4 + b.depth * 0.5 + b.flash)
        ctx.fillStyle = b.flash > 0.05 ? hues[0] : b.hue
        ctx.beginPath()
        ctx.arc(0, 0, b.s * 0.16 + (still ? 0 : Math.sin(t / 900 + b.x) * 0.4) + b.flash * 3, 0, Math.PI * 2)
        ctx.fill()
        ctx.restore()
        b.flash *= 0.93
      }
      for (const m of msgs) {
        const k = Math.min(1, (t - m.t0) / m.dur)
        const [ax, ay] = pos(m.a), [bx, by] = pos(m.b)
        const x = ax + (bx - ax) * k, y = ay + (by - ay) * k
        ctx.globalAlpha = 0.16
        ctx.strokeStyle = ink
        ctx.setLineDash([2, 5])
        ctx.beginPath()
        ctx.moveTo(ax, ay)
        ctx.lineTo(x, y)
        ctx.stroke()
        ctx.setLineDash([])
        ctx.globalAlpha = 0.9
        ctx.fillStyle = hues[0]
        ctx.beginPath()
        ctx.arc(x, y, 2.6, 0, Math.PI * 2)
        ctx.fill()
        if (k >= 1) {
          m.b.flash = 1
          m.done = true
        }
      }
      msgs = msgs.filter(m => !m.done)
      ctx.globalAlpha = 1
    }

    const send = (t: number) => {
      if (boxes.length < 2 || msgs.length > 5) return
      const a = boxes[(Math.random() * boxes.length) | 0]
      const near = boxes.filter(b => b !== a && Math.hypot(b.x - a.x, b.y - a.y) < 240)
      const b = near[(Math.random() * near.length) | 0]
      if (b) msgs.push({ a, b, t0: t, dur: 900 + Math.random() * 900 })
    }

    const frame = (t: number) => {
      if (t - last > 480) {
        send(t)
        last = t
      }
      draw(t)
      raf = requestAnimationFrame(frame)
    }
    const move = (e: PointerEvent) => {
      const r = canvas.getBoundingClientRect()
      ptr.x = e.clientX - r.left
      ptr.y = e.clientY - r.top
    }
    layout()
    addEventListener("resize", layout)
    addEventListener("pointermove", move)
    if (still) draw(0)
    else raf = requestAnimationFrame(frame)
    return () => {
      cancelAnimationFrame(raf)
      removeEventListener("resize", layout)
      removeEventListener("pointermove", move)
    }
  }, [])

  return <canvas ref={ref} aria-hidden="true" style={{ position: "absolute", inset: 0, width: "100%", height: "100%", pointerEvents: "none" }} />
}
