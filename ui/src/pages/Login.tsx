import { useEffect, useState } from 'react'
import { Bot, KeyRound, LogIn } from 'lucide-react'
import { authConfig, finishLogin, startLogin, type OidcConfig } from '../oidc'

/** Sign in with the organisation's identity provider (when configured), or with a token:
 *  a ServiceAccount token (`clank token create`), an ID token, or the owner token. */
export function Login({ onToken }: { onToken: (t: string) => void }) {
  const [token, setToken] = useState('')
  const [error, setError] = useState<string>()
  const [busy, setBusy] = useState(false)
  const [oidc, setOidc] = useState<OidcConfig | null>(null)
  const [showToken, setShowToken] = useState(false)

  useEffect(() => {
    authConfig().then(setOidc)
    finishLogin().then(t => t && onToken(t), e => setError(String(e.message ?? e)))
  }, [onToken])

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
        {oidc && (
          <button type="button" className="b pri lg" style={{ marginTop: 22, width: '100%', justifyContent: 'center' }} onClick={() => startLogin(oidc).catch(e => setError(String(e.message ?? e)))}>
            <LogIn />Sign in with SSO
          </button>
        )}
        {oidc && !showToken ? (
          <button type="button" className="faint" style={{ marginTop: 14, display: 'flex', gap: 6, alignItems: 'center' }} onClick={() => setShowToken(true)}><KeyRound style={{ width: 14, height: 14 }} />Use an access token instead</button>
        ) : (
          <>
            <label htmlFor="t">Access token</label>
            <input id="t" type="password" value={token} onChange={e => setToken(e.target.value)} placeholder="clk_…" autoFocus={!oidc} />
            <p className="muted" style={{ marginTop: 8, fontSize: 12 }}>Create one with <span className="mono">clank token create &lt;account&gt;</span>, or use the ID token from <span className="mono">clank login</span>.</p>
            <button className={`b ${oidc ? 'sec' : 'pri'} lg`} style={{ marginTop: 18, width: '100%', justifyContent: 'center' }} disabled={!token.trim() || busy}>{busy ? 'Checking…' : 'Continue'}</button>
          </>
        )}
        {error && <div className="err">{error}</div>}
      </form>
    </div>
  )
}
