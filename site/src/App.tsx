import { motion } from "motion/react"
import { Approve } from "./components/Approve"
import { Broker } from "./components/Broker"
import { Contain } from "./components/Contain"
import { Field } from "./components/Field"
import { Lockbot } from "./components/Lockbot"
import { Prove } from "./components/Prove"
import { CrashScene, TalkScene } from "./components/Scenes"
import { Terminal } from "./components/Terminal"

const DOCS = "../../README.md"

function Step({ n, label, title, sub, hint, children, id }: { n: string; label: string; title: React.ReactNode; sub: React.ReactNode; hint?: string; children: React.ReactNode; id?: string }) {
  return (
    <motion.section className="section" id={id} initial={{ opacity: 0, y: 24 }} whileInView={{ opacity: 1, y: 0 }} viewport={{ once: true, amount: 0.2 }} transition={{ duration: 0.7, ease: [0.22, 1, 0.36, 1] }}>
      <div className="head">
        <p className="label"><i />{n} · {label}</p>
        <h2>{title}</h2>
        <p className="sub">{sub}</p>
      </div>
      <div className="stage">
        <div className="stage-bar"><span>{label}</span>{hint && <span className="hint"><b>try it</b> {hint}</span>}</div>
        {children}
      </div>
    </motion.section>
  )
}

export function App() {
  return (
    <>
      <header className="top">
        <a className="brand" href="#top" aria-label="Clankr home"><Lockbot size={30} />clankr</a>
        <nav>
          <a href="#how">How it works</a>
          <a href="#start">Quick start</a>
          <a href={DOCS}>Docs</a>
          <a className="gh" href={DOCS}>Read the docs →</a>
        </nav>
      </header>

      <main id="top">
        <section className="hero">
          <Field />
          <motion.div className="hero-in" initial="h" animate="v" variants={{ v: { transition: { staggerChildren: 0.09 } } }}>
            <motion.div className="hero-bot" variants={{ h: { opacity: 0, y: 12, scale: 0.9 }, v: { opacity: 1, y: 0, scale: 1 } }}><Lockbot size={86} stroke={7} /></motion.div>
            <motion.p className="label" variants={{ h: { opacity: 0, y: 8 }, v: { opacity: 1, y: 0 } }}><i />open source · written in Rust · Apache-2.0</motion.p>
            <motion.h1 variants={{ h: { opacity: 0, y: 16 }, v: { opacity: 1, y: 0, transition: { duration: 0.8, ease: [0.22, 1, 0.36, 1] } } }}>
              Your AI agents, locked in.<span className="hot">Every move on the record.</span>
            </motion.h1>
            <motion.p className="lede" variants={{ h: { opacity: 0, y: 10 }, v: { opacity: 1, y: 0 } }}>
              Clankr runs AI agents in sandboxes, records every step they take, and stands between them and every model, tool and message they touch. One binary on Postgres. No Kubernetes, no Temporal.
            </motion.p>
            <motion.div className="ctas" variants={{ h: { opacity: 0, y: 10 }, v: { opacity: 1, y: 0 } }}>
              <a className="btn" href="#start">Run it locally <span aria-hidden="true">→</span></a>
              <a className="btn ghost" href="#how">See how it works</a>
            </motion.div>
          </motion.div>
        </section>

        <Step id="how" n="01" label="contain" hint="drag the agent"
          title={<>Every agent gets a lock.</>}
          sub="A rootless container, a gVisor sandbox, or a Firecracker microVM, chosen per agent. Read-only, unprivileged, and with no network until you grant a host by name.">
          <Contain />
        </Step>

        <Step n="02" label="record"
          title={<>Crash anywhere.<br />Resume exactly there.</>}
          sub="Each step's result, the event that records it and the next task commit in one Postgres transaction. When a node dies, its lease runs out, the task moves to another node, and nothing that finished runs twice.">
          <CrashScene />
        </Step>

        <Step n="03" label="broker" hint="drag the slider"
          title={<>They ask by name.<br />The gateway holds the keys.</>}
          sub="An agent asks for a model claim, never an API key. The gateway checks it's allowed, reserves tokens against the budget, makes the call with the real key, and records who spent what.">
          <Broker />
        </Step>

        <Step n="04" label="ask a person" hint="press and hold"
          title={<>Some calls wait for a yes.</>}
          sub="Publishing, paying, deleting: mark a step or a tool as approval-required, and it waits for someone in the right group. Who said yes, and when, goes on the record.">
          <Approve />
        </Step>

        <Step n="05" label="talk"
          title={<>Agents call each other by name.</>}
          sub="A service is a stable name that every instance of an agent serves. Requests time out cleanly, late replies never come back to haunt anyone, and topics fan out into durable mailboxes and triggers.">
          <TalkScene />
        </Step>

        <Step n="06" label="prove it" hint="click a row to rewrite it"
          title={<>An audit log your DBA<br />can't quietly edit.</>}
          sub="Every privileged action and every denial goes into a hash-chained log, shipped off-site as it's written. Rewrite a row in the database, and the copy says so.">
          <Prove />
        </Step>

        <motion.section className="section close" id="start" initial={{ opacity: 0, y: 24 }} whileInView={{ opacity: 1, y: 0 }} viewport={{ once: true, amount: 0.3 }} transition={{ duration: 0.7 }}>
          <div className="head">
            <p className="label"><i />quick start</p>
            <h2>Three commands to a running cluster.</h2>
            <p className="sub">Then <code>clank apply -f hello.yaml</code>, and watch it in the console at <code>https://127.0.0.1:7600</code>. Add kennel for sandboxed runtimes; the README walks through all of it.</p>
          </div>
          <Terminal />
          <div className="ctas center">
            <a className="btn" href={`${DOCS}#quick-start`}>Read the quick start →</a>
            <a className="btn ghost" href="../../docs/adr">Design notes</a>
          </div>
        </motion.section>
      </main>

      <footer>
        <span className="brand small"><Lockbot size={20} watch={false} />clankr · Apache-2.0</span>
        <nav>
          <a href={DOCS}>README</a>
          <a href="../../docs/adr">Design notes</a>
          <a href="../../docs/runbooks/backup-restore.md">Runbook</a>
          <a href={`${DOCS}#security`}>Security model</a>
          <a href={`${DOCS}#not-built-yet`}>Not built yet</a>
        </nav>
      </footer>
    </>
  )
}
