import { useEffect, useRef, useState, type ReactNode } from "react"
import { AnimatePresence, motion, useScroll, useSpring, useTransform, type MotionValue } from "motion/react"
import { Film } from "./Film"
import "./styles.css"

const GITHUB = "https://github.com/randheer/clankr"
const soft = [0.16, 1, 0.3, 1] as const

function Reveal({ children, delay = 0, className }: { children: ReactNode; delay?: number; className?: string }) {
  return (
    <motion.div className={className} initial={{ opacity: 0, y: 34, filter: "blur(10px)" }} whileInView={{ opacity: 1, y: 0, filter: "blur(0px)" }}
      viewport={{ once: true, margin: "-12% 0px" }} transition={{ duration: 1.1, ease: soft, delay }}>
      {children}
    </motion.div>
  )
}

function Mark({ size = 26 }: { size?: number }) {
  return <img src="./clankr-mark.svg" width={size} height={size * 112 / 104} alt="" />
}

/* ── the opening ─────────────────────────────────────────── */

function Hero() {
  const ref = useRef<HTMLElement>(null)
  const { scrollYProgress } = useScroll({ target: ref, offset: ["start start", "end start"] })
  const lift = useTransform(scrollYProgress, [0, 1], [0, -120])
  const tilt = useTransform(scrollYProgress, [0, 0.6], [0, 1])
  const scale = useTransform(tilt, [0, 1], [1, 0.94])
  return (
    <header ref={ref} className="hero section">
      <motion.div style={{ y: lift }} className="hero-text">
        <Reveal><span className="eyebrow glass"><i />Free, open source, runs on your own computers</span></Reveal>
        <Reveal delay={0.08}><h1>A safe place for your helpers <em className="s">to get work done.</em></h1></Reveal>
        <Reveal delay={0.16}>
          <p className="lede">Clankr is where the programs that work for you live: the ones that answer email, keep the books, write the newsletter.
            Each one gets its own room, a logbook it can't skip, and a front desk that holds your keys. They do the busy work. You stay the one who says yes.</p>
        </Reveal>
        <Reveal delay={0.24} className="row">
          <a className="btn" href="#workshop">See how it works</a>
          <a className="btn light" href="#start">Set it up</a>
        </Reveal>
      </motion.div>
      <Reveal delay={0.3}>
        <motion.div className="film-frame glass" style={{ scale }}>
          <Film id="room" />
        </motion.div>
        <div className="film-cap"><b>Its own room</b><span>A helper can try any door it likes. Only the ones you opened will move.</span></div>
      </Reveal>
    </header>
  )
}

/* ── the whole idea, lit word by word as you read ───────── */

const BREATH = "Think of Clankr as a well-run workshop. Every helper gets its own bench behind its own door. Everything it does goes into a logbook. It borrows tools from the front desk instead of carrying your keys around. And anything that leaves the building, a payment, an email, a post, waits for your yes."

function Word({ w, i, n, p }: { w: string; i: number; n: number; p: MotionValue<number> }) {
  const o = useTransform(p, [i / n, (i + 1.5) / n], [0.16, 1])
  return <motion.span style={{ opacity: o }}>{w} </motion.span>
}

function OneBreath() {
  const ref = useRef<HTMLParagraphElement>(null)
  const { scrollYProgress } = useScroll({ target: ref, offset: ["start 85%", "end 45%"] })
  const words = BREATH.split(" ")
  return (
    <section className="section breath">
      <Reveal><p className="mono">The idea, in one breath</p></Reveal>
      <p ref={ref} className="breath-text">{words.map((w, i) => <Word key={i} w={w} i={i} n={words.length} p={scrollYProgress} />)}</p>
    </section>
  )
}

/* ── the workshop, room by room ──────────────────────────── */

type Line = [string, string, string?]
const CHAPTERS: { id: string; title: string; hook: string; body: string; under: string; card: { head: string; lines: Line[]; foot: string } }[] = [
  {
    id: "room", title: "Its own room", hook: "A helper can be clever and still make mistakes. So it never works in your living room.",
    body: "Each one runs in a sealed space with its own files, its own memory and its own share of the computer. If it gets something wrong, the mess stays on its side of the door. You choose how thick the walls are, from a light partition to a whole separate machine.",
    under: "Linux namespaces and cgroups · gVisor · Firecracker microVMs",
    card: { head: "Room 3 · the writer", lines: [["Can see", "its own folder"], ["Can reach", "the front desk"], ["Memory", "512 MB, no more"], ["Your files", "not in here", "dim"]], foot: "Walls: microVM" },
  },
  {
    id: "logbook", title: "A logbook that never forgets", hook: "Every step is written down before the next one starts.",
    body: "If the power goes out, or a machine restarts at three in the morning, the work doesn't vanish. It carries on from the last line in the book. Nothing gets paid twice, nothing gets sent twice. And when someone asks why it did that, you can read the answer.",
    under: "Durable event log · resume after a crash · replay any run",
    card: { head: "Logbook · Tuesday", lines: [["09:35", "Drafted the newsletter"], ["09:41", "Server restarted", "bad"], ["09:41", "Carried on from line 4", "good"], ["09:44", "Waiting for your OK", "hot"]], foot: "1,204 lines today" },
  },
  {
    id: "desk", title: "Keys at the front desk", hook: "Helpers never hold your passwords.",
    body: "When one needs to send an email or look something up, it asks the front desk. The desk checks the errand is allowed, does it with your key, and hands back only the result. The key never enters the room, so it can't be copied, leaked or left behind.",
    under: "A gateway on every machine · secrets sealed at rest · Vault KMS",
    card: { head: "Front desk", lines: [["Asked for", "the shop's email"], ["By", "the writer"], ["Allowed for", "newsletters only"], ["Key handed over", "never", "good"]], foot: "Errand done · 09:46" },
  },
  {
    id: "ask", title: "Ask before you act", hook: "Some decisions are yours, and stay yours.",
    body: "You pick which actions need a yes: spending money, emailing customers, deleting anything at all. When a helper reaches one, it stops, tells you plainly what it wants to do and why, and waits, for a minute or a week. Your answer goes in the logbook with your name on it.",
    under: "Approval steps · rules per tool · every answer recorded",
    card: { head: "Waiting for you", lines: [["The writer wants to", "send the newsletter"], ["To", "2,340 people"], ["Checked by", "the editor"], ["Your answer", "Yes · 09:46", "good"]], foot: "Signed: you" },
  },
  {
    id: "notes", title: "Notes, not shouting", hook: "Helpers work together the way a good team does.",
    body: "The writer can ask the editor for a second look. The bookkeeper can pin a note saying stock is low, and anyone who cares can read it. Every note passes the front desk, so you can see who spoke to whom, and nobody can reach a helper they weren't introduced to.",
    under: "Requests and replies · shared notice boards · routed by the node",
    card: { head: "Notes", lines: [["writer → editor", "Could you read this?"], ["editor → writer", "One typo, fixed."], ["bookkeeper → board", "Blue mug: 4 left", "hot"], ["stranger → writer", "turned away", "bad"]], foot: "All notes logged" },
  },
  {
    id: "copy", title: "A copy kept somewhere safe", hook: "The record doesn't live in only one place.",
    body: "A copy of the logbook goes, line by line, to storage you choose, somewhere the workshop itself can't touch. Each line is sealed to the one before it. If anything is ever changed after the fact, the seal breaks and you'll know.",
    under: "Off-site audit export · each entry chained to the last · audit-check",
    card: { head: "The kept copy", lines: [["Lives in", "your own bucket"], ["Lines", "14,212"], ["Last checked", "this morning"], ["Changed since", "nothing", "good"]], foot: "Seal intact" },
  },
]

function Card({ c, n }: { c: (typeof CHAPTERS)[number]; n: number }) {
  return (
    <motion.div key={c.id} className="card glass" initial={{ opacity: 0, y: 24, filter: "blur(12px)", rotateX: 8 }} animate={{ opacity: 1, y: 0, filter: "blur(0px)", rotateX: 0 }}
      exit={{ opacity: 0, y: -24, filter: "blur(12px)", rotateX: -8 }} transition={{ duration: 0.7, ease: soft }}>
      <div className="card-head"><span>{c.card.head}</span><span className="mono">0{n + 1} / 06</span></div>
      {c.card.lines.map(([k, v, tone], i) => (
        <motion.div key={i} className={`card-line ${tone ?? ""}`} initial={{ opacity: 0, x: -10 }} animate={{ opacity: 1, x: 0 }} transition={{ delay: 0.18 + i * 0.09, duration: 0.6, ease: soft }}>
          <span>{k}</span><b>{v}</b>
        </motion.div>
      ))}
      <div className="card-foot mono">{c.card.foot}</div>
    </motion.div>
  )
}

function Workshop() {
  const [on, setOn] = useState(0)
  const refs = useRef<(HTMLElement | null)[]>([])
  useEffect(() => {
    const io = new IntersectionObserver(es => es.forEach(e => e.isIntersecting && setOn(+(e.target as HTMLElement).dataset.n!)), { rootMargin: "-45% 0px -45% 0px" })
    refs.current.forEach(r => r && io.observe(r))
    return () => io.disconnect()
  }, [])
  return (
    <section id="workshop" className="section">
      <Reveal className="intro">
        <p className="mono">How it works</p>
        <h2>Six rooms of <em className="s">the workshop.</em></h2>
        <p className="lede">Nothing here is magic. It's the same common sense you'd use if you hired someone new: give them a desk, keep a record, hold on to the keys, and ask them to check with you first.</p>
      </Reveal>
      <div className="chapters">
        <div className="pin">
          <div className="steps">{CHAPTERS.map((c, i) => <span key={c.id} className={i === on ? "on" : ""}>{c.title}</span>)}</div>
          <div className="stage"><AnimatePresence mode="wait"><Card key={on} c={CHAPTERS[on]} n={on} /></AnimatePresence></div>
        </div>
        <div className="list">
          {CHAPTERS.map((c, i) => (
            <article key={c.id} ref={el => { refs.current[i] = el }} data-n={i} className={`chapter ${i === on ? "on" : ""}`}>
              <span className="num">0{i + 1}</span>
              <h3>{c.title}</h3>
              <p className="hook">{c.hook}</p>
              <p>{c.body}</p>
              <p className="mono under">Under the hood: {c.under}</p>
              <div className="inline-card"><Card c={c} n={i} /></div>
            </article>
          ))}
        </div>
      </div>
    </section>
  )
}

/* ── the cinema: two short films on a dark stage ─────────── */

function Cinema() {
  return (
    <section className="cinema">
      <div className="section">
        <Reveal className="intro center">
          <p className="mono">Watch it happen</p>
          <h2>Lights out. <em className="s">Nothing lost.</em></h2>
          <p className="lede">The writer is halfway through the spring newsletter when the server restarts. Here's what that looks like from the inside.</p>
        </Reveal>
        <Reveal><div className="film-frame dark-glass"><Film id="logbook" /></div>
          <div className="film-cap"><b>A logbook that never forgets</b><span>It carried on from line six. You'd never have known.</span></div></Reveal>
        <Reveal className="intro center gap">
          <h2>It asks. <em className="s">You answer.</em></h2>
          <p className="lede">The draft is ready, the editor has read it, and it's going to 2,340 people. That last part is your call.</p>
        </Reveal>
        <Reveal><div className="film-frame dark-glass"><Film id="ask" /></div>
          <div className="film-cap"><b>Ask before you act</b><span>Hold to allow. Your yes goes in the book, with the time.</span></div></Reveal>
      </div>
    </section>
  )
}

/* ── a day in the life ───────────────────────────────────── */

const DAY: [string, string, string][] = [
  ["07:00", "You're still asleep.", "The bookkeeper reads last night's orders and matches every payment to a parcel. Two don't match. It leaves you a note instead of guessing."],
  ["08:15", "A customer writes in about a cracked mug.", "The helpdesk finds the order, drafts a kind reply and a replacement, and puts both in front of you. You change one word and say yes."],
  ["09:41", "The server restarts for an update.", "The writer was halfway through the newsletter. It picks up from the last line in its logbook. Nobody notices, which is the point."],
  ["09:46", "Your phone buzzes on the bus.", "“Send the spring newsletter to 2,340 people?” You read it, hold to allow, and look out of the window again."],
  ["13:30", "The blue mug is down to four.", "The bookkeeper pins a note. The writer reads it and quietly takes the mug out of Friday's email."],
  ["18:00", "You close up.", "Every step of the day is in the logbook, and a sealed copy is already somewhere safe. None of the helpers ever saw your bank password."],
]

function Day() {
  const ref = useRef<HTMLDivElement>(null)
  const { scrollYProgress } = useScroll({ target: ref, offset: ["start 70%", "end 60%"] })
  const fill = useSpring(scrollYProgress, { stiffness: 80, damping: 24 })
  return (
    <section id="day" className="section split">
      <Reveal className="intro side">
        <p className="mono">A day with Clankr</p>
        <h2>Tuesday at a small <em className="s">pottery shop.</em></h2>
        <p className="lede">You make the mugs. Three helpers handle the rest: a bookkeeper, a helpdesk and a writer. This is what their day looks like, and how little of it needs you.</p>
      </Reveal>
      <div className="day" ref={ref}>
        <div className="rail"><motion.i style={{ scaleY: fill }} /></div>
        {DAY.map(([time, head, body], i) => (
          <Reveal key={time} delay={0.04 * (i % 2)} className="moment">
            <span className="time mono">{time}</span>
            <div className="glass moment-card"><b>{head}</b><p>{body}</p></div>
          </Reveal>
        ))}
      </div>
    </section>
  )
}

/* ── plain answers ───────────────────────────────────────── */

const ASKED: [string, string][] = [
  ["Is Clankr a chatbot?", "No. It's the place your helpers live and work, not a helper itself. You bring your own, written in Python, TypeScript or Go, and use whichever model provider you like, or none."],
  ["Where does it run?", "On your own Linux machines: a laptop, a box in the back room, or a whole rack. Your data stays where you put it, and it doesn't phone home."],
  ["Do I need to be technical?", "To set it up, for now, yes. Someone comfortable with a terminal can have it running in an afternoon. After that, the day to day is reading the logbook and answering the odd question."],
  ["What does it cost?", "Nothing. It's open source and free to run. Your only bills are the computers and any model provider you choose."],
]

function Answers() {
  return (
    <section className="section split">
      <Reveal className="intro side"><p className="mono">Plain answers</p><h2>Things people <em className="s">usually ask.</em></h2></Reveal>
      <div className="qa">
        {ASKED.map(([q, a], i) => (
          <Reveal key={q} delay={i * 0.05}>
            <details className="glass" open={i === 0}><summary>{q}<i /></summary><p>{a}</p></details>
          </Reveal>
        ))}
      </div>
    </section>
  )
}

/* ── setting it up ───────────────────────────────────────── */

const STEPS: [string, string, string][] = [
  ["Build it", "You'll need Linux and Rust. Keep the clankr and kennel folders side by side.", "cd kennel && cargo build\ncd ../clankr && cargo build"],
  ["Open the workshop", "It makes its own database and keys the first time, then opens on your machine.", "./target/debug/clankrd --dev\n# → https://127.0.0.1:7600"],
  ["Add a bench", "A bench is any computer your helpers can work on. Start with the one you're on.", "./target/debug/clankr-node --server https://127.0.0.1:7600 \\\n  --join-token \"$(cat ~/.config/clankr/join-token)\""],
  ["Bring in your first helper", "Describe it in a short file, hand it over, and give it something to do.", "clank apply -f hello.yaml\nclank run start hello -n demo"],
]

function Start() {
  return (
    <section id="start" className="section">
      <Reveal className="intro">
        <p className="mono">Set it up</p>
        <h2>Running by <em className="s">this afternoon.</em></h2>
        <p className="lede">Four steps. The full guide, with every option, lives in the README.</p>
      </Reveal>
      <div className="steps-grid">
        {STEPS.map(([title, body, code], i) => (
          <Reveal key={title} delay={i * 0.06} className="step glass">
            <span className="num">{i + 1}</span>
            <h3>{title}</h3>
            <p>{body}</p>
            <pre><code>{code}</code></pre>
          </Reveal>
        ))}
      </div>
      <Reveal className="closing glass">
        <Mark size={54} />
        <h2>Let them do the busy work. <em className="s">Keep the keys.</em></h2>
        <div className="row"><a className="btn" href={GITHUB}>Get Clankr on GitHub</a><a className="btn light" href={`${GITHUB}#readme`}>Read the guide</a></div>
      </Reveal>
    </section>
  )
}

export function App() {
  const [dark, setDark] = useState(false)
  useEffect(() => {
    const io = new IntersectionObserver(([e]) => setDark(e.isIntersecting), { rootMargin: "0px 0px -92% 0px" })
    io.observe(document.querySelector(".cinema")!)
    return () => io.disconnect()
  }, [])
  return (
    <>
      <div className="field" aria-hidden><i /><i /><i /><i /></div>
      <div className="grain" aria-hidden />
      <nav className={`nav glass ${dark ? "dark" : ""}`}>
        <a className="brand" href="#"><Mark />Clankr</a>
        <a className="l" href="#workshop">How it works</a>
        <a className="l" href="#day">A day</a>
        <a className="l" href="#start">Set it up</a>
        <a className="btn sm" href={GITHUB}>GitHub</a>
      </nav>
      <main>
        <Hero />
        <OneBreath />
        <Workshop />
        <Cinema />
        <Day />
        <Answers />
        <Start />
      </main>
      <footer>
        <span>Clankr · open source · made for people who'd rather not hand over their keys</span>
        <nav><a href={GITHUB}>GitHub</a><a href={`${GITHUB}#readme`}>Guide</a><a href={`${GITHUB}/tree/main/docs`}>Docs</a></nav>
      </footer>
    </>
  )
}
