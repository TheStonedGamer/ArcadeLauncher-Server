// Thin client for the storefront endpoints (store_api.rs / store_auth.rs /
// store_lib.rs). Same-origin in production (nginx proxies /api to the k3s
// NodePort); the Vite dev proxy handles it during `npm run dev`.
//
// Public browse endpoints need no auth. The auth + library endpoints ride a
// session cookie (AL_STORE_SESSION), so every authed call sends
// `credentials: 'include'` — the cookie is HttpOnly and never touches JS.

async function getJSON(path) {
  const res = await fetch(path, { headers: { Accept: 'application/json' }, credentials: 'include' })
  if (!res.ok) throw new Error(`${path} → ${res.status}`)
  return res.json()
}

// POST/DELETE helper. `form` (an object) is sent as x-www-form-urlencoded to
// match the server's axum `Form<…>` extractors; omit it for bodyless calls.
// Throws an Error whose message is the server's `error` field when present.
async function send(path, method, form) {
  const opts = { method, credentials: 'include', headers: { Accept: 'application/json' } }
  if (form) {
    opts.headers['Content-Type'] = 'application/x-www-form-urlencoded'
    opts.body = new URLSearchParams(form).toString()
  }
  const res = await fetch(path, opts)
  let data = null
  try { data = await res.json() } catch { /* empty body */ }
  if (!res.ok) {
    const err = new Error((data && data.error) || `${path} → ${res.status}`)
    err.status = res.status
    err.data = data
    throw err
  }
  return data
}

// --- Public storefront ---
export const fetchSummary = () => getJSON('/api/store/summary')
export const fetchGames = () => getJSON('/api/store/games')
export const fetchGame = (id) => getJSON(`/api/store/games/${encodeURIComponent(id)}`)

// --- Auth (session cookie) ---
export const login = (username, password, totp) =>
  send('/api/store/auth/login', 'POST', { username, password, totp: totp || '' })
export const logout = () => send('/api/store/auth/logout', 'POST')
export const register = (username, email, password) =>
  send('/api/auth/register', 'POST', { username, email, password })
// Resolve the current session, or null when signed out (401 is expected, not an error).
export async function me() {
  const res = await fetch('/api/store/auth/me', {
    headers: { Accept: 'application/json' },
    credentials: 'include',
  })
  if (res.status === 401) return null
  if (!res.ok) throw new Error(`me → ${res.status}`)
  return res.json()
}

// --- Library ("ownership") ---
export const fetchLibrary = () => getJSON('/api/store/library')
export const addToLibrary = (id) => send(`/api/store/library/${encodeURIComponent(id)}`, 'POST')
export const removeFromLibrary = (id) => send(`/api/store/library/${encodeURIComponent(id)}`, 'DELETE')

// --- Downloads (self-hosted launcher installers) ---
export const fetchDownloads = () => getJSON('/api/downloads/latest')

// Format helpers shared across pages.
export function fmtDate(unixSecs) {
  if (!unixSecs || unixSecs <= 0) return 'Unreleased'
  const d = new Date(unixSecs * 1000)
  return d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' })
}

export function fmtRating(igdb) {
  // IGDB rating is 0..100; show as a /100 score or a dash when absent.
  if (!igdb || igdb <= 0) return null
  return Math.round(igdb)
}

export function fmtHours(hours) {
  if (!hours) return '0h'
  if (hours < 1) return '<1h'
  return `${hours.toLocaleString()}h`
}

export function fmtBytes(bytes) {
  if (!bytes || bytes <= 0) return ''
  const units = ['B', 'KB', 'MB', 'GB']
  let n = bytes
  let u = 0
  while (n >= 1024 && u < units.length - 1) { n /= 1024; u++ }
  return `${n.toFixed(n < 10 && u > 0 ? 1 : 0)} ${units[u]}`
}
