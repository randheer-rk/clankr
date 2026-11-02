// Browser sign-in with the cluster's OIDC provider: authorization code + PKCE (public client,
// no secret in the page). The ID token becomes the bearer token clankrd verifies.

export interface OidcConfig { issuer: string; clientId: string; scopes: string }

const PENDING = 'clankr.oidc.pending'

const b64url = (bytes: ArrayBuffer | Uint8Array) =>
  btoa(String.fromCharCode(...new Uint8Array(bytes))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')

const random = (n = 32) => b64url(crypto.getRandomValues(new Uint8Array(n)))

async function discover(issuer: string) {
  const r = await fetch(`${issuer.replace(/\/$/, '')}/.well-known/openid-configuration`)
  if (!r.ok) throw new Error(`OIDC discovery failed (${r.status})`)
  return r.json() as Promise<{ authorization_endpoint: string; token_endpoint: string }>
}

export async function authConfig(): Promise<OidcConfig | null> {
  const r = await fetch('/v1/auth/config').catch(() => null)
  if (!r?.ok) return null
  return (await r.json()).oidc ?? null
}

const redirectUri = () => `${location.origin}${location.pathname}`

/** Sends the browser to the identity provider. */
export async function startLogin(cfg: OidcConfig) {
  const d = await discover(cfg.issuer)
  const verifier = random(48)
  const challenge = b64url(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier)))
  const state = random(16)
  sessionStorage.setItem(PENDING, JSON.stringify({ verifier, state, tokenEndpoint: d.token_endpoint, clientId: cfg.clientId }))
  const q = new URLSearchParams({ response_type: 'code', client_id: cfg.clientId, redirect_uri: redirectUri(), scope: cfg.scopes, state, code_challenge: challenge, code_challenge_method: 'S256' })
  location.assign(`${d.authorization_endpoint}?${q}`)
}

/** On return from the provider: exchanges the code for an ID token. Null when this isn't a callback. */
export async function finishLogin(): Promise<string | null> {
  const p = new URLSearchParams(location.search)
  const code = p.get('code')
  if (!code) return null
  const pending = JSON.parse(sessionStorage.getItem(PENDING) ?? 'null')
  sessionStorage.removeItem(PENDING)
  history.replaceState(null, '', redirectUri() + location.hash)
  if (!pending || p.get('state') !== pending.state) throw new Error('Sign-in response did not match this browser session; try again.')
  const r = await fetch(pending.tokenEndpoint, {
    method: 'POST',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({ grant_type: 'authorization_code', code, redirect_uri: redirectUri(), client_id: pending.clientId, code_verifier: pending.verifier }),
  })
  const t = await r.json().catch(() => ({}))
  if (!r.ok || !t.id_token) throw new Error(t.error_description ?? t.error ?? 'The identity provider did not return an ID token.')
  return t.id_token as string
}
