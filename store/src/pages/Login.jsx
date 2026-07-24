import { useState } from 'react'
import { Link, useNavigate, useLocation } from 'react-router-dom'
import { useAuth } from '../auth.jsx'

export default function Login() {
  const { login } = useAuth()
  const navigate = useNavigate()
  const location = useLocation()
  const from = location.state?.from || '/'

  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [totp, setTotp] = useState('')
  const [needTotp, setNeedTotp] = useState(false)
  const [error, setError] = useState(null)
  const [busy, setBusy] = useState(false)

  async function onSubmit(e) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await login(username, password, totp)
      navigate(from, { replace: true })
    } catch (err) {
      if (err.data?.totpRequired) setNeedTotp(true)
      setError(err.message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="auth-page">
      <form className="auth-card" onSubmit={onSubmit}>
        <h1>Sign in</h1>
        <p className="muted">Sign in to build your library and install games in the launcher.</p>

        {error && <div className="notice error">{error}</div>}

        <label>
          Username or email
          <input value={username} onChange={(e) => setUsername(e.target.value)} autoFocus required />
        </label>
        <label>
          Password
          <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} required />
        </label>
        {needTotp && (
          <label>
            Two-factor code
            <input value={totp} onChange={(e) => setTotp(e.target.value)} inputMode="numeric" placeholder="123456" />
          </label>
        )}

        <button className="btn-primary" type="submit" disabled={busy}>
          {busy ? 'Signing in…' : 'Sign in'}
        </button>

        <div className="auth-alt">
          No account? <Link to="/register">Create one</Link>
        </div>
      </form>
    </div>
  )
}
