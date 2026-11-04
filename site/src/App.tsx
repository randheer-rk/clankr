import { useEffect, useRef, useState, type ReactNode } from "react"
import { motion, useMotionValueEvent, useScroll, useSpring, useTransform } from "motion/react"
import { Film } from "./Film"
import "./styles.css"

const GITHUB = "https://github.com/randheer/clankr"
const ease = [0.16, 1, 0.3, 1] as const

/** Rises into place the first time it scrolls into view. */
function Up({ children, delay = 0, className, y = 40 }: { children: ReactNode; delay?: number; className?: string; y?: number }) {
  return (
    <motion.div className={className} initial={{ opacity: 0, y }} whileInView={{ opacity: 1, y: 0 }} viewport={{ once: true, margin: "-10% 0px" }} transition={{ duration: 0.9, ease, delay }}>
      {children}
    </motion.div>
  )
}

/** The Lockbot, as the animated SVG in docs/assets: its eyes look around and blink on their own. */
function Lockbot({ size = 26 }: { size?: number }) {
  return <img src="./clankr-mark-animated.svg" width={size} height={Math.round(size * 112 / 104)} alt="" draggable={false} />
}

/* ── hero ─────────────────────────────────────────────────── */

const H1 = ["Agents", "that", "work", "*shifts,*", "not", "demos."]

function Hero() {
  const ref = useRef<HTMLElement>(null)
  const { scrollYProgress } = useScroll({ target: ref, offset: ["start start", "end start"] })
  const botY = useTransform(scrollYProgress, [0, 1], [0, 140])
  const botR = useTransform(scrollYProgress, [0, 1], [0, -6])
  const textY = useTransform(scrollYProgress, [0, 1], [0, 60])
  const fade = useTransform(scrollYProgress, [0, 0.7], [1, 0])
  return (
    <header ref={ref} className="hero">
      <div className="wrap">
        <motion.div style={{ y: textY, opacity: fade }}>
          <h1>
            {H1.map((w, i) => {
              const em = w.startsWith("*")
              const text = w.replaceAll("*", "")
              return (
                <span key={i}><span className="w">
                  <motion.span initial={{ y: "110%" }} animate={{ y: 0 }} transition={{ duration: 0.9, ease, delay: 0.1 + i * 0.07 }}>
                    {em ? <em>{text}</em> : text}
                  </motion.span>
                </span>{" "}</span>
              )
            })}
          </h1>
          <motion.p className="lede" initial={{ opacity: 0, y: 20 }} animate={{ opacity: 1, y: 0 }} transition={{ duration: 0.9, ease, delay: 0.6 }}>
            Clankr runs your AI agents like a plant runs machines: each one <b>sandboxed on a node</b>, every step <b>logged before it happens</b>, every key <b>kept at the gate</b>. When a machine fails, the job carries on. When a tool needs sign-off, it waits for you.
          </motion.p>
          <motion.div className="row" initial={{ opacity: 0, y: 20 }} animate={{ opacity: 1, y: 0 }} transition={{ duration: 0.9, ease, delay: 0.75 }}>
            <a className="btn" href="#start">Build it in an afternoon</a>
            <a className="btn o" href="#how">How it runs</a>
          </motion.div>
        </motion.div>
        <motion.div className="bot" style={{ y: botY, rotate: botR }} initial={{ opacity: 0, scale: 0.9 }} animate={{ opacity: 1, scale: 1 }} transition={{ duration: 1.2, ease, delay: 0.3 }}>
          <Lockbot size={360} />
          <div className="plate-tag">Model <b>Lockbot</b><br />Rust · Postgres · one binary<br />Ser. no. <b>0.1.0</b></div>
        </motion.div>
      </div>
    </header>
  )
}

/* ── strip ────────────────────────────────────────────────── */

const BELT = ["Durable workflows", "Sandboxed runtimes", "Brokered credentials", "Human approvals", "Audit, off-site", "Fleet scheduling", "Durable messaging", "Python · TypeScript · Go"]

function Strip() {
  return (
    <div className="strip" aria-hidden>
      <div>{[...BELT, ...BELT].map((b, i) => <span key={i}><i>■</i>{b}</span>)}</div>
    </div>
  )
}

/* ── plates ───────────────────────────────────────────────── */

const PLATES: [string, string, ReactNode][] = [
  ["It picks up where it fell.", "Every workflow step is an event in Postgres, committed together with the work it schedules. A node dies, the run moves, and resumes at the last line. No model is ever called twice.",
    <>#15 <span className="y">node.lost</span>        eu-1{"\n"}#16 <span className="g">step.resumed</span>     draft ← <b>#14</b></>],
  ["Keys stay at the gate.", "Agents name a claim; the gateway on their node holds the real key, reserves budget before the call and settles after. A runaway loop hits the budget, not your card.",
    <>ModelClaim default → anthropic{"\n"}budget: blog/monthly  <b>▮▮▮▮▮</b>▯▯ 71%</>],
  ["Some levers need a hand.", "Mark a tool as approval-gated and the run parks in an inbox until a person says yes. The decision, and who made it, goes in the hash-chained audit log.",
    <><span className="d">$</span> clank approve a-91c2 --reason "ship it"{"\n"}audit #14,212 <span className="g">✓ sealed</span></>],
]

function Plates() {
  return (
    <section className="sec">
      <div className="wrap">
        <div className="head">
          <Up><span className="label">What it's for</span><h2>Many agents. <em>For real.</em></h2></Up>
          <Up delay={0.1}><p className="lede">Getting one agent working in a notebook is easy. Running many of them for real raises problems no single agent should have to solve.</p></Up>
        </div>
        <div className="plates">
          {PLATES.map(([t, p, code], i) => (
            <Up key={t} delay={i * 0.1} className="plate">
              <span className="label">Plate 0{i + 1}</span><h3>{t}</h3><p>{p}</p><pre>{code}</pre>
            </Up>
          ))}
        </div>
      </div>
    </section>
  )
}

/* ── films ────────────────────────────────────────────────── */

function Films() {
  return (
    <section className="sec" style={{ paddingTop: 0 }}>
      <div className="wrap">
        <div className="head">
          <Up><span className="label">Shift report</span><h2>Watch a node <em>die.</em></h2></Up>
          <Up delay={0.1}><p className="lede">The writer is halfway through a draft when its node's lease expires. Thirty seconds later it's on another node, at the same line.</p></Up>
        </div>
        <Up className="film-box"><Film id="logbook" /><div className="film-cap"><b>Durable</b><span>resumes from event #14 · no replay</span></div></Up>
        <div className="films">
          <Up className="film-box"><Film id="room" /><div className="film-cap"><b>Sandboxed</b><span>container · gVisor · microVM</span></div></Up>
          <Up delay={0.1} className="film-box"><Film id="ask" /><div className="film-cap"><b>Gated</b><span>approval in the audit log</span></div></Up>
        </div>
      </div>
    </section>
  )
}

/* ── the replay ───────────────────────────────────────────── */

const LOG: [string, string, string][] = [
  ["00:00.012", "run.started", "blog/publish-weekly-8d1"],
  ["00:00.090", "instance.placed", "writer → eu-1 · microVM"],
  ["00:00.340", "model.call.reserved", "4,000 tok · blog/monthly"],
  ["00:09.077", "model.call.settled", "3,412 tok · $0.0112"],
  ["00:09.081", "message.sent", "→ clankr://agents/blog/reviewer"],
  ["00:31.502", "node.lost", "eu-1 · lease expired · epoch 41"],
  ["00:31.640", "instance.rescheduled", "writer → eu-2 · epoch 42"],
  ["00:33.910", "step.resumed", "draft ← #14 · no replay"],
  ["00:34.004", "approval.requested", "publish.post · inbox"],
  ["08:12.901", "approval.granted", "ops@ · \"LGTM\""],
  ["08:13.120", "audit.appended", "#14,212 · chained · shipped"],
]

const STEPS: { at: number; label: string; title: string; body: ReactNode }[] = [
  { at: 1, label: "Describe", title: "You apply it. Clankr places it.", body: <>Agents, models, tools and workflows are resources: like Kubernetes objects, without Kubernetes. <code>clank apply</code>, and the scheduler puts each agent on a node, in a rootless container, gVisor, or a Firecracker microVM.</> },
  { at: 3, label: "Broker", title: "Keys never enter the sandbox.", body: <>The agent names a claim. The gateway on its node holds the real credential, reserves tokens from the budget before the call, and settles the true cost after. Who spent what is in the log.</> },
  { at: 7, label: "Survive", title: "The machine died. The run didn't.", body: <>A step commits in the same transaction as the work it schedules. When a lease expires, the instance is rescheduled with a new epoch and resumes from the last committed event. Stale writes from the old node are refused.</> },
  { at: 9, label: "Gate", title: "Some tools wait for a person.", body: <>Gate the tool, and the run parks in an inbox for a minute or a week. The answer, and who gave it, is appended here and to a hash-chained audit log you can ship off-site and verify with <code>clank audit-check</code>.</> },
  { at: 10, label: "Read back", title: "All of it, in order, forever.", body: <>Inspect any run as it stood after any event. Fork a new run from there. Nothing is inferred; it's in the log.</> },
]

function Replay() {
  const [on, setOn] = useState(0)
  const refs = useRef<(HTMLDivElement | null)[]>([])
  useEffect(() => {
    const io = new IntersectionObserver(es => es.forEach(e => e.isIntersecting && setOn(+(e.target as HTMLElement).dataset.n!)), { rootMargin: "-45% 0px -45% 0px" })
    refs.current.forEach(r => r && io.observe(r))
    return () => io.disconnect()
  }, [])
  const cursor = STEPS[on].at
  return (
    <section id="how" className="sec">
      <div className="wrap">
        <div className="head">
          <Up><span className="label">How it runs</span><h2>One run, <em>replayed.</em></h2></Up>
          <Up delay={0.1}><p className="lede">Every run is a log of events. This is a real one's shape, from the first placement to the sealed audit line.</p></Up>
        </div>
        <div className="replay">
          <div className="pin">
            <Up className="log">
              <div className="hd"><b>clank run events</b><span>publish-weekly-8d1</span></div>
              {LOG.map(([t, k, n], i) => (
                <div key={i} className={`r ${i === cursor ? "on" : i < cursor ? "seen" : ""}`}><span>{t}</span><span><span className="k">{k}</span>  <span>{n}</span></span></div>
              ))}
            </Up>
          </div>
          <div className="steps">
            {STEPS.map((s, i) => (
              <div key={s.label} ref={el => { refs.current[i] = el }} data-n={i} className="step">
                <Up><span className="label">0{i + 1} · {s.label}</span></Up>
                <Up delay={0.05}><h3>{s.title}</h3></Up>
                <Up delay={0.1}><p>{s.body}</p></Up>
              </div>
            ))}
          </div>
        </div>
      </div>
    </section>
  )
}

/* ── manifesto ────────────────────────────────────────────── */

function Say() {
  const ref = useRef<HTMLDivElement>(null)
  const { scrollYProgress } = useScroll({ target: ref, offset: ["start 90%", "start 40%"] })
  const x = useSpring(useTransform(scrollYProgress, [0, 1], [-60, 0]), { stiffness: 80, damping: 20 })
  return (
    <section className="sec" style={{ paddingTop: 0 }}>
      <div className="wrap say" ref={ref} style={{ paddingTop: "var(--sec)" }}>
        <motion.h2 style={{ x }}>No Kubernetes.<br />No Temporal.<br /><em>One binary.</em></motion.h2>
        <Up delay={0.15}>
          <p><b>clankrd</b> is the whole control plane: API, scheduler, workflow engine, gateway, inbox and web UI, on a Postgres you already know how to back up. Nodes dial in over mTLS, so they can sit behind NAT. Postgres is the source of truth and the message bus: dispatch is <code>SKIP LOCKED</code>, wake-ups are <code>NOTIFY</code>, leadership is a lease row.</p>
          <p><b>clankr-node</b> is a small daemon on each machine that runs agents under <b>kennel</b>. Write agents in Python, TypeScript or Go with an SDK that hands you work and messages. Describe them in YAML. <code>clank apply</code>. That's the job.</p>
          <div className="row"><a className="btn o" href={`${GITHUB}/tree/main/docs/adr`}>Read the design notes</a></div>
        </Up>
      </div>
    </section>
  )
}

/* ── quick start ──────────────────────────────────────────── */

const QS: [string, string, string][] = [
  ["Build", "Linux and Rust. Keep clankr and kennel side by side.", "cd kennel && cargo build\ncd ../clankr && cargo build"],
  ["Start the plane", "Makes its own Postgres, CA and tokens on first run.", "./target/debug/clankrd --dev\n# https://127.0.0.1:7600"],
  ["Join a node", "Any Linux box. Start with the one you're on.", "clankr-node --server https://127.0.0.1:7600 \\\n  --join-token \"$(cat ~/.config/clankr/join-token)\""],
  ["Run something", "A mock provider, so no API key is needed.", "clank apply -f hello.yaml\nclank run start hello -n demo"],
]

function Start() {
  return (
    <section id="start" className="sec" style={{ paddingTop: 0 }}>
      <div className="wrap">
        <div className="head">
          <Up><span className="label">Quick start</span><h2>Running by <em>this afternoon.</em></h2></Up>
          <Up delay={0.1}><p className="lede">Four steps on a laptop. The README has every option, including gVisor, microVMs and multi-node clusters.</p></Up>
        </div>
        <div className="qs">
          {QS.map(([t, p, code], i) => (
            <Up key={t} delay={i * 0.08} className="plate"><span className="n">{i + 1}</span><h3>{t}</h3><p>{p}</p><pre>{code}</pre></Up>
          ))}
        </div>
      </div>
    </section>
  )
}

export function App() {
  const [scrolled, setScrolled] = useState(false)
  const { scrollY } = useScroll()
  useMotionValueEvent(scrollY, "change", v => setScrolled(v > 40))
  return (
    <>
      <nav className={`nav ${scrolled ? "on" : ""}`}>
        <div className="wrap">
          <a className="wm" href="#"><Lockbot />Clankr</a>
          <a className="x" href="#how">How it runs</a>
          <a className="x" href="#start">Quick start</a>
          <a className="x" href={`${GITHUB}/tree/main/docs/adr`}>Design notes</a>
          <a className="gh" href={GITHUB}>GitHub ↗</a>
        </div>
      </nav>
      <main>
        <Hero />
        <Strip />
        <Plates />
        <Films />
        <Replay />
        <Say />
        <Start />
      </main>
      <footer>
        <div className="wrap">
          <span>Clankr · Apache-2.0 · Rust · one binary on Postgres</span>
          <nav><a href={GITHUB}>GitHub</a><a href={`${GITHUB}#readme`}>README</a><a href={`${GITHUB}/tree/main/docs/adr`}>ADRs</a></nav>
        </div>
      </footer>
    </>
  )
}
