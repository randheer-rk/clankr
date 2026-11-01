import { useState } from 'react'
import { Bot } from 'lucide-react'

/** Token sign-in: an API token (`clank token create`), an OIDC ID token (`clank login`), or the owner token. */
export function Login({ onToken }: { onToken: (t: string) => void }) {
  const [token, setToken] = useState('')
  const [error, setError] = useState<string>()
  const [busy, setBusy] = useState(false)
  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    setBusy(true); setError(undefined)
    const r = await fetch('/v1/whoami', { headers: { authorization: `Bearer ${token.trim()}` } }).catch(() => null)
    setBusy(false)
    if (r?.ok) onToken(token.trim())
    else setError(r ? 'That token was not accepted.' : 'Could not reach clankrd.')
  }
  return (
    <div className="login">
      <form className="panel" onSubmit={submit}>
        <div className="ws" style={{ padding: 0 }}><div className="logo"><Bot /></div><div><b>Clankr</b><span>Sign in to your control plane</span></div></div>
        <label htmlFor="t">Access token</label>
        <input id="t" type="password" value={token} onChange={e => setToken(e.target.value)} placeholder="clk_…" autoFocus />
        <p className="muted" style={{ marginTop: 8, fontSize: 12 }}>Create one with <span className="mono">clank token create &lt;account&gt;</span>, or use the ID token from <span className="mono">clank login</span>.</p>
        {error && <div className="err">{error}</div>}
        <button className="b pri lg" style={{ marginTop: 18, width: '100%', justifyContent: 'center' }} disabled={!token.trim() || busy}>{busy ? 'Checking…' : 'Continue'}</button>
      </form>
    </div>
  )
}
