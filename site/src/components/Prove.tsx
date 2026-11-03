import { useMemo, useState } from "react"
import { AnimatePresence, motion } from "motion/react"

const ROWS = ["create agent", "read secret", "approve post", "exec a shell", "delete claim", "rotate key"]

// a stand-in for sha256 that's short enough to show: FNV-1a over prev + body
function hash(prev: string, body: string) {
  let h = 0x811c9dc5
  for (const c of prev + "|" + body) h = Math.imul(h ^ c.charCodeAt(0), 0x01000193) >>> 0
  return h.toString(16).padStart(8, "0").slice(0, 6)
}

/** The audit chain: click a row to rewrite it, the way a database superuser could. */
export function Prove() {
  const [edited, setEdited] = useState<number | null>(null)
  const chain = useMemo(() => {
    // the exported copy was taken from the honest chain
    let prev = "000000"
    const exported = ROWS.map(r => (prev = hash(prev, r)))
    prev = "000000"
    return ROWS.map((r, i) => {
      const body = i === edited ? r.replace(/\w+$/, "nothing") : r
      const h = hash(prev, body)
      const row = { i, body, h, prev, matches: h === exported[i] }
      prev = h
      return row
    })
  }, [edited])
  const broken = chain.filter(r => !r.matches).map(r => r.i + 41)

  return (
    <div className="prove">
      <div className="chain" role="list">
        {chain.map((r, idx) => (
          <div key={r.i} className="link-wrap" role="listitem">
            <motion.button className={`block ${r.matches ? "" : "bad"} ${r.i === edited ? "edited" : ""}`} onClick={() => setEdited(e => (e === r.i ? null : r.i))}
              animate={r.i === edited ? { rotate: [-2, 2, -1.5, 0], y: [0, -3, 0] } : { rotate: 0 }} transition={{ duration: 0.4 }}
              aria-label={`Row ${r.i + 41}: ${r.body}. Click to ${r.i === edited ? "restore" : "tamper with"} it`}>
              <span className="seq mono">#{r.i + 41}</span>
              <span className="body">{r.body}</span>
              <motion.span className="h mono" key={r.h} initial={{ opacity: 0, y: 4 }} animate={{ opacity: 1, y: 0 }}>{r.h}…</motion.span>
            </motion.button>
            {idx < chain.length - 1 && (
              <motion.span className="joint" animate={{ scaleX: chain[idx + 1].matches ? 1 : 0.35, backgroundColor: chain[idx + 1].matches ? "var(--muted)" : "var(--bad)" }} transition={{ type: "spring", stiffness: 300, damping: 18 }} />
            )}
          </div>
        ))}
      </div>
      <motion.pre className="term" layout>
        <span className="p">$</span> clankrd audit-check /var/log/clankr/audit.jsonl{"\n"}
        46 rows in the export{"\n"}
        <AnimatePresence mode="wait">
          {broken.length === 0 ? (
            <motion.span key="ok" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}><span className="good">ok</span>    every exported row is in the database, unchanged</motion.span>
          ) : (
            <motion.span key="bad" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}><span className="bad">FAIL</span>  {broken.length} exported rows differ in the database: [{broken.join(", ")}]</motion.span>
          )}
        </AnimatePresence>
      </motion.pre>
    </div>
  )
}
