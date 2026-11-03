import { useEffect, useRef, useState } from "react"
import { AnimatePresence, motion, useInView } from "motion/react"

const LINES = [
  "cargo build",
  "./target/debug/clankrd --dev",
  './target/debug/clankr-node --server https://127.0.0.1:7600 \\\n    --join-token "$(cat ~/.config/clankr/join-token)" \\\n    --ca-cert ~/.local/share/clankr/pki/ca.crt --runtimes process',
]
const FULL = LINES.map(l => `$ ${l}`).join("\n")

/** The quick start, typed out when it scrolls into view; copy takes it without the prompts. */
export function Terminal() {
  const ref = useRef<HTMLDivElement>(null)
  const seen = useInView(ref, { once: true, amount: 0.5 })
  const [n, setN] = useState(0)
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    if (!seen) return
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return setN(FULL.length)
    let i = 0
    const t = setInterval(() => {
      // a pause at each new prompt, like someone typing
      i += FULL[i] === "\n" ? 1 : 2
      setN(Math.min(i, FULL.length))
      if (i >= FULL.length) clearInterval(t)
    }, 22)
    return () => clearInterval(t)
  }, [seen])

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(LINES.join("\n"))
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      /* clipboard blocked: the text is selectable */
    }
  }
  const shown = FULL.slice(0, n)

  return (
    <div className="terminal" ref={ref}>
      <div className="dots" aria-hidden="true"><i /><i /><i /><span className="mono">~/clankr</span></div>
      <button className="copy" onClick={copy} aria-label="Copy the commands">
        <AnimatePresence mode="wait" initial={false}>
          <motion.span key={copied ? "y" : "n"} initial={{ y: 8, opacity: 0 }} animate={{ y: 0, opacity: 1 }} exit={{ y: -8, opacity: 0 }} transition={{ duration: 0.15 }}>
            {copied ? "copied ✓" : "copy"}
          </motion.span>
        </AnimatePresence>
      </button>
      <pre aria-label={FULL}>
        {shown.split("\n").map((line, i, all) => (
          <span key={i} className="ln">
            {line.startsWith("$ ") ? <><span className="p">$</span>{line.slice(1)}</> : line}
            {i === all.length - 1 && n < FULL.length && <span className="caret" />}
            {i < all.length - 1 && "\n"}
          </span>
        ))}
      </pre>
    </div>
  )
}
