import { lazy, Suspense, useEffect, useState, type CSSProperties, type ReactNode } from "react"
import type { FilmId } from "./films"
import { Lockbot } from "./Lockbot"
import "./styles.css"

const Film = lazy(() => import("./Film"))

const GITHUB = "https://github.com/randheer-rk/clankr"
const ADR = `${GITHUB}/tree/main/docs/adr`

const HEART = "M1 0h2v1h1v-1h2v1h1v2h-1v1h-1v1h-1v1h-1v-1h-1v-1h-1v-1h-1v-2h1z"
const Px = ({ d, box, className }: { d: string; box: string; className?: string }) =>
  <svg className={className} viewBox={box} aria-hidden="true" shapeRendering="crispEdges"><path d={d} fillRule="evenodd" /></svg>
const Play = () => <Px box="0 0 7 8" d="M0 0h2v1h2v1h2v1h1v2h-1v1h-2v1h-2v1h-2z" />

function Agent({ x, walk, ghost }: { x: number; walk?: boolean; ghost?: boolean }) {
  return (
    <div className={`agent ${walk ? "walk" : ""} ${ghost ? "ghost" : ""}`} style={{ left: `${x}%` }} aria-hidden="true">
      <svg viewBox="0 0 12 14" shapeRendering="crispEdges">
        <rect className="body" width="12" height="10" fill="#FFF1E8" />
        <rect className="body" x="1.5" y="10" width="3" height="4" fill="#FFF1E8" />
        <rect className="body" x="7.5" y="10" width="3" height="4" fill="#FFF1E8" />
        <rect x="2.5" y="3.5" width="2" height="2.5" fill="#0D0E16" />
        <rect x="7.5" y="3.5" width="2" height="2.5" fill="#0D0E16" />
      </svg>
    </div>
  )
}

type Tone = "bad" | "good" | "wait"
const RUN: { t: string; text: string; x: number; tone?: Tone; say?: string }[] = [
  { t: "00:00", text: "An agent starts writing on computer 1", x: 5 },
  { t: "00:09", text: "First draft saved", x: 21 },
  { t: "00:31", text: "Computer 1 crashes", x: 21, tone: "bad", say: "The draft is safe." },
  { t: "00:31", text: "The agent moves to computer 2", x: 39 },
  { t: "00:33", text: "It picks up the saved draft", x: 54, tone: "good", say: "No starting over." },
  { t: "00:34", text: "Ready to publish. It asks you first.", x: 68, tone: "wait", say: "It will wait as long as you need." },
  { t: "", text: "You said yes", x: 68, tone: "good" },
  { t: "", text: "Published", x: 108, tone: "good", say: "Clankr remembers who approved it." },
]
const PARKED = 5
const clock = (s: number) => `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`

function Hero() {
  const [step, setStep] = useState(() => (matchMedia("(prefers-reduced-motion: reduce)").matches ? PARKED : 0))
  const [wait, setWait] = useState(0)
  useEffect(() => {
    if (step === PARKED) {
      const id = setInterval(() => setWait(w => w + 1), 1000)
      return () => clearInterval(id)
    }
    if (step === RUN.length - 1) return
    const id = setTimeout(() => setStep(step + 1), step === 2 ? 1300 : 1700)
    return () => clearTimeout(id)
  }, [step])
  const ev = RUN[step]
  const done = step === RUN.length - 1
  return (
    <header className="hero">
      <div className="wrap">
        <div className="copy">
          <div className="head"><h1>AI agents you can <em>leave running.</em></h1><Lockbot size={190} /></div>
          <div className="sub">
            <p className="lede">Clankr looks after your AI agents so they can do real work, every day. It gives each one a safe place to work and the tools it is allowed to use. If a computer crashes, the work carries on. If an agent wants to do something risky, it asks you first.</p>
            <div className="row">
              <a className="btn" href="#start"><Play />Get started</a>
              <a className="btn o" href={GITHUB}>See it on GitHub</a>
            </div>
          </div>
        </div>
        <div className="hud">
          <span className="run">Job<b>weekly blog post</b></span>
          <span className="ev">Steps saved<b>{String(step + 2).padStart(3, "0")}</b></span>
          <span>Computers<Px className={`heart ${step >= 2 ? "lost" : ""}`} box="0 0 7 6" d={HEART} /><Px className="heart" box="0 0 7 6" d={HEART} /></span>
        </div>
        <div className="field">
          <div className={`node n1 ${step >= 2 ? "lost" : ""}`}><b>Computer 1</b><span>{step >= 2 ? "crashed" : "working"}</span></div>
          <div className={`node n2 ${step >= 3 ? "live" : ""}`}><b>Computer 2</b><span>{step >= 3 ? "working" : "spare"}</span></div>
          {step >= 3 && <div className="flag"><span>Saved here</span></div>}
          {step >= 2 && <Agent x={21} ghost />}
          {step !== 2 && <Agent key={step < 2 ? "one" : "two"} x={ev.x} walk={step !== PARKED && !done} />}
          <div className={`gate ${step > PARKED ? "open" : ""}`}><i /><b>Asks first</b></div>
          {step === PARKED && (
            <div className="ask" role="group" aria-label="The agent is asking for approval">
              The agent wants to publish. OK?
              <span className="wait">Waiting<b>{clock(wait)}</b><i /></span>
              <button type="button" onClick={() => setStep(PARKED + 1)}>Yes, publish</button>
            </div>
          )}
          {done && (
            <div className="ask done" role="group" aria-label="Job finished">
              Published. Job done.
              <button type="button" onClick={() => { setWait(0); setStep(0) }}>Watch again</button>
            </div>
          )}
        </div>
      </div>
      <div className="cap" aria-live="polite">
        <div className="wrap">
          <span>{ev.t || clock(34 + wait)}</span>
          <b className={ev.tone}>{ev.text}</b>
          {ev.say && <em>{ev.say}</em>}
        </div>
      </div>
    </header>
  )
}

const POWER: { name: string; icon: ReactNode; title: string; body: string; code: ReactNode }[] = [
  { name: "1UP", icon: <Px box="0 0 7 6" d={HEART} />, title: "A crash doesn't lose the work.",
    body: "Clankr saves each step as an agent finishes it. If its computer crashes, the agent picks up on another one, right where it stopped. No starting over.",
    code: <><span className="r">Computer 1 crashed</span>{"\n"}<span className="b">Agent carried on from computer 2</span></> },
  { name: "Key", icon: <Px box="0 0 8 7" d="M0 2h1v-1h3v1h1v1h3v2h-1v-1h-1v1h-2v1h-3v-1h-1zM2 3h1v1h-1z" />, title: "Your passwords stay with you.",
    body: "Agents never see your keys or logins. Clankr does the sensitive part for them, and every team has a spending cap, so a confused agent can't run up a bill.",
    code: <>Blog team, this month{"\n"}<span className="y">▮▮▮▮▮</span><span className="d">▯▯</span> 71% of budget used</> },
  { name: "Hold", icon: <Px box="0 0 7 6" d="M1 0h2v6h-2zM4 0h2v6h-2z" />, title: "Nothing risky without a yes.",
    body: "You choose what needs a human: publishing, emailing customers, spending money. The agent waits until a person says yes, and Clankr remembers who did.",
    code: <>Publish the weekly post?{"\n"}<span className="b">Yes</span> <span className="d">· approved by you</span></> },
]

function Problem() {
  return (
    <section className="sec">
      <div className="wrap">
        <div className="head">
          <h2>One agent is easy. <em style={{ color: "var(--blue)" }}>A team is hard work.</em></h2>
          <p className="lede">An agent that works once on your laptop is a demo. Agents you rely on every day need a place to run, someone to pick them up when they fall, and someone to say no. That is what Clankr is for.</p>
        </div>
        {POWER.map((p, i) => (
          <div key={p.name} className={`power c${i + 1}`}>
            <div className="item px">{p.icon}{p.name}</div>
            <div><h3>{p.title}</h3><p>{p.body}</p></div>
            <pre>{p.code}</pre>
          </div>
        ))}
      </div>
    </section>
  )
}

const DEMOS: { id: FilmId; label: string; detail: string; c: string }[] = [
  { id: "logbook", label: "A computer crashes", detail: "Halfway through a job, the computer goes down. Another one finishes it.", c: "var(--red)" },
  { id: "room", label: "An agent oversteps", detail: "An agent reaches for something it was never given. It gets a no.", c: "var(--blue)" },
  { id: "ask", label: "A person decides", detail: "A risky step waits for a human. Their answer is remembered.", c: "var(--yellow)" },
]

function Clips() {
  const [selected, setSelected] = useState(0)
  const demo = DEMOS[selected]
  return (
    <section id="demos" className="sec demos">
      <div className="wrap">
        <div className="head">
          <h2>See it <em>in action.</em></h2>
          <p className="lede">Three short clips showing how Clankr keeps work safe: when a computer crashes, when an agent tries something it shouldn't, and when a decision needs you.</p>
        </div>
        <div className="cabinet">
          <div role="tabpanel" id="demo-panel" aria-labelledby={`demo-tab-${demo.id}`} tabIndex={0}>
            <Suspense fallback={<div className="film" style={{ aspectRatio: "16 / 9" }} />}><Film id={demo.id} /></Suspense>
          </div>
          <div className="deck">
            <div role="tablist" aria-label="Clips" style={{ display: "contents" }}>
              {DEMOS.map((item, i) => <button key={item.id} type="button" role="tab" id={`demo-tab-${item.id}`} aria-selected={selected === i} aria-controls="demo-panel" tabIndex={selected === i ? 0 : -1} style={{ "--c": item.c } as CSSProperties} onClick={() => setSelected(i)} onKeyDown={event => {
                if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) return
                event.preventDefault()
                const back = event.key === "ArrowLeft" || event.key === "ArrowUp"
                const next = event.key === "Home" ? 0 : event.key === "End" ? DEMOS.length - 1 : (i + (back ? -1 : 1) + DEMOS.length) % DEMOS.length
                setSelected(next)
                event.currentTarget.parentElement?.querySelectorAll("button")[next]?.focus()
              }}>{item.label}</button>)}
            </div>
            <p>{demo.detail}</p>
          </div>
        </div>
      </div>
    </section>
  )
}

const LOG: [string, string][] = [
  ["00:00", "A new job: write the weekly blog post"],
  ["00:00", "A writer agent starts on computer 1"],
  ["00:00", "It asks to use the AI. Budget checked."],
  ["00:09", "Draft written. It cost about one cent."],
  ["00:09", "Draft passed to a reviewer agent"],
  ["00:31", "Computer 1 crashes"],
  ["00:31", "The writer moves to computer 2"],
  ["00:33", "It carries on from the saved draft"],
  ["00:34", "Ready to publish. Waiting for a person."],
  ["08:12", "A person says yes"],
  ["08:13", "Published. Every step is remembered."],
]

function Record() {
  return (
    <section id="how" className="sec">
      <div className="wrap">
        <div className="head">
          <h2>Every job <em style={{ color: "var(--red)" }}>leaves a record.</em></h2>
        </div>
        <div className="replay">
          <div className="log">
            <div className="hd"><span>Job record</span><span>Weekly blog post</span></div>
            {LOG.map(([t, k], i) => (
              <div key={i} className={i === 3 ? "r on" : "r"}><span>{t}</span><b>{k}</b></div>
            ))}
          </div>
          <div className="story">
            <div>
              <h3>Nothing is forgotten.</h3>
              <p>Open any job later and read it from start to finish: what was done, when, and who said yes. Nobody has to guess.</p>
            </div>
            <div>
              <h3>You see what it cost.</h3>
              <p>Clankr checks the budget before an agent uses the AI, and keeps count afterwards. You can see what every job spent, down to the cent.</p>
            </div>
          </div>
        </div>
      </div>
    </section>
  )
}

function Why() {
  return (
    <section className="sec print say">
      <div className="wrap">
        <h2>Your computers.<br />Your rules.<br /><em>One program.</em></h2>
        <div className="cols">
          <p>Clankr is a single program. Start it on one computer and it brings everything with it: a page where you watch your agents, an inbox for the things they ask you, and the record of what they did.</p>
          <p>Need more room? Add another computer with one command. Laptops, office servers and rented ones all work together. Clankr is free and open source, and it runs where you put it.</p>
          <a className="inkbtn" href={ADR}>Read how it's built</a>
        </div>
      </div>
    </section>
  )
}

const QS: [string, string, string][] = [
  ["Get it", "Download Clankr and build it. You need a Linux computer with Rust.", "cd kennel && cargo build\ncd ../clankr && cargo build"],
  ["Switch it on", "It sets itself up the first time. There is nothing to fill in.", "./target/debug/clankrd --dev\n# https://127.0.0.1:7600"],
  ["Add a computer", "Start with the one you are on. Add more whenever you like.", "clankr-node --server https://127.0.0.1:7600 \\\n  --join-token \"$(cat ~/.config/clankr/join-token)\""],
  ["Give an agent a job", "The sample agent uses a pretend AI, so you need no account anywhere.", "clank apply -f hello.yaml\nclank run start hello -n demo"],
]

function Start() {
  return (
    <section id="start" className="sec print start">
      <div className="wrap">
        <div className="head">
          <h2>Get started</h2>
          <p className="lede">Four steps. By the end you will have watched your first agent finish a job.</p>
        </div>
        <ol>
          {QS.map(([t, p, code], i) => (
            <li key={t}><span aria-hidden="true">{i + 1}</span><div><h3>{t}</h3><p>{p}</p></div><pre>{code}</pre></li>
          ))}
        </ol>
      </div>
    </section>
  )
}

function Closing() {
  return (
    <footer>
      <div className="wrap">
        <div className="continue">
          <div>
            <h2>Put your agents <em>to work.</em></h2>
            <p className="lede">Set Clankr up on one computer and give your first agent a job. It is free, and it runs on machines you already have.</p>
            <div className="row">
              <a className="btn" href="#start"><Play />Get started</a>
              <a className="btn o" href={GITHUB}>See it on GitHub</a>
            </div>
          </div>
          <Lockbot size={210} />
        </div>
        <div className="credits">
          <span>Clankr 0.1.0 · free and open source</span>
          <nav><a href={GITHUB}>GitHub</a><a href={`${GITHUB}#readme`}>Guide</a><a href={ADR}>How it's built</a></nav>
        </div>
      </div>
    </footer>
  )
}

export function App() {
  return (
    <>
      <nav className="nav">
        <div className="wrap">
          <a className="wm" href="#"><Lockbot size={30} />Clankr</a>
          <a className="x" href="#how">How it works</a>
          <a className="x" href="#demos">Clips</a>
          <a className="x" href="#start">Get started</a>
          <a className="gh" href={GITHUB}>GitHub</a>
        </div>
      </nav>
      <main>
        <Hero />
        <Problem />
        <Clips />
        <Record />
        <Why />
        <Start />
      </main>
      <Closing />
    </>
  )
}
