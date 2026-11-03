// The background: a field of small sandboxes, each holding an agent, with messages passing
// between them. It stays in the margins so the text column is never drawn over.
(() => {
  const canvas = document.getElementById("field")
  const ctx = canvas.getContext("2d")
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches
  const css = getComputedStyle(document.documentElement)
  const hues = ["--ember", "--moss", "--slate", "--ochre", "--plum"].map(v => css.getPropertyValue(v).trim())
  const ink = css.getPropertyValue("--ink").trim()
  let boxes = [], msgs = [], w = 0, h = 0, dpr = 1, spanY = 0

  function layout() {
    dpr = Math.min(devicePixelRatio || 1, 2)
    w = innerWidth
    h = innerHeight
    canvas.width = w * dpr
    canvas.height = h * dpr
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
    // the column the text lives in stays clear
    const column = Math.min(900, w - 32)
    const clearL = (w - column) / 2, clearR = clearL + column
    spanY = h * 3
    const n = Math.round((w * spanY) / 19000)
    boxes = []
    for (let i = 0; i < n * 3 && boxes.length < n; i++) {
      const x = Math.random() * w
      if (w > 760 && x > clearL - 24 && x < clearR + 24) continue
      if (w <= 760 && Math.random() > 0.25) continue
      const s = 10 + Math.random() * 16
      boxes.push({ x, y: Math.random() * spanY, s, hue: hues[(Math.random() * hues.length) | 0], depth: 0.25 + Math.random() * 0.55, rot: (Math.random() - 0.5) * 0.5, flash: 0, dash: Math.random() < 0.35 })
    }
  }

  function at(b) {
    // slow parallax, wrapping vertically
    const y = (((b.y - scrollY * b.depth * 0.35) % spanY) + spanY) % spanY - h * 0.15
    return [b.x, y]
  }

  function draw(t) {
    ctx.clearRect(0, 0, w, h)
    // phones have no margins to keep it in: it's the hero's alone, and fades as you scroll on
    const fade = w > 760 ? 1 : Math.max(0, 1 - scrollY / (h * 0.7))
    if (fade === 0) return
    for (const b of boxes) {
      const [x, y] = at(b)
      if (y < -40 || y > h + 40) continue
      ctx.save()
      ctx.translate(x, y)
      ctx.rotate(b.rot)
      ctx.globalAlpha = (0.28 + b.depth * 0.5) * fade
      ctx.strokeStyle = b.hue
      ctx.lineWidth = 1.4
      ctx.setLineDash(b.dash ? [3, 3] : [])
      round(-b.s / 2, -b.s / 2, b.s, b.s, b.s * 0.28)
      ctx.stroke()
      const pulse = still ? 0 : Math.sin(t / 900 + b.x) * 0.4
      ctx.globalAlpha = Math.min(1, 0.45 + b.depth * 0.5 + b.flash) * fade
      ctx.fillStyle = b.flash > 0.05 ? hues[0] : b.hue
      ctx.beginPath()
      ctx.arc(0, 0, b.s * 0.16 + pulse + b.flash * 3, 0, Math.PI * 2)
      ctx.fill()
      ctx.restore()
      b.flash *= 0.93
    }
    // messages in flight
    for (const m of msgs) {
      const k = Math.min(1, (t - m.t0) / m.dur)
      const [ax, ay] = at(m.a), [bx, by] = at(m.b)
      const x = ax + (bx - ax) * k, y = ay + (by - ay) * k
      ctx.globalAlpha = 0.18 * fade
      ctx.strokeStyle = ink
      ctx.setLineDash([2, 5])
      ctx.beginPath()
      ctx.moveTo(ax, ay)
      ctx.lineTo(x, y)
      ctx.stroke()
      ctx.setLineDash([])
      ctx.globalAlpha = 0.9 * fade
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

  function round(x, y, w, h, r) {
    ctx.beginPath()
    ctx.moveTo(x + r, y)
    ctx.arcTo(x + w, y, x + w, y + h, r)
    ctx.arcTo(x + w, y + h, x, y + h, r)
    ctx.arcTo(x, y + h, x, y, r)
    ctx.arcTo(x, y, x + w, y, r)
    ctx.closePath()
  }

  function send(t) {
    const onScreen = boxes.filter(b => {
      const y = at(b)[1]
      return y > 0 && y < h
    })
    if (onScreen.length < 2 || msgs.length > 5) return
    const a = onScreen[(Math.random() * onScreen.length) | 0]
    // nearby, so the lines read as conversations, not noise
    const near = onScreen.filter(b => b !== a && Math.hypot(b.x - a.x, at(b)[1] - at(a)[1]) < 260)
    const b = near[(Math.random() * near.length) | 0]
    if (b) msgs.push({ a, b, t0: t, dur: 900 + Math.random() * 900 })
  }

  layout()
  addEventListener("resize", () => {
    layout()
    if (still) draw(0)
  })
  if (still) {
    draw(0)
    addEventListener("scroll", () => draw(0), { passive: true })
    return
  }
  let last = 0
  const frame = t => {
    if (t - last > 520) {
      send(t)
      last = t
    }
    draw(t)
    requestAnimationFrame(frame)
  }
  requestAnimationFrame(frame)
})()

// Sections fade in as they arrive; the event log plays out row by row.
;(() => {
  const io = new IntersectionObserver(entries => {
    for (const e of entries) {
      if (!e.isIntersecting) continue
      e.target.classList.add("in")
      e.target.querySelectorAll(".log").forEach(log => {
        log.querySelectorAll("li").forEach((li, i) => (li.style.transitionDelay = `${i * 140}ms`))
        log.classList.add("on")
      })
      io.unobserve(e.target)
    }
  }, { threshold: 0.18 })
  document.querySelectorAll("main section:not(.hero)").forEach(s => {
    s.classList.add("reveal")
    io.observe(s)
  })
})()

// Copy the quick-start commands without the prompts.
document.querySelectorAll("[data-copy]").forEach(btn => {
  btn.addEventListener("click", async () => {
    const text = document.getElementById(btn.dataset.copy).innerText.replace(/^\$ /gm, "")
    try {
      await navigator.clipboard.writeText(text)
      btn.textContent = "copied"
    } catch {
      btn.textContent = "select and copy"
    }
    setTimeout(() => (btn.textContent = "copy"), 1600)
  })
})
