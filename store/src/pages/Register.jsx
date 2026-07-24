import { useState } from 'react'
import { Link } from 'react-router-dom'
import { register } from '../api.js'

export default function Register() {
  const [username, setUsername] = useState('')
  const [email, setEmail] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState(null)
  const [busy, setBusy] = useState(false)
  const [done, setDone] = useState(false)

  async function onSubmit(e) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await register(username, email, password)
      setDone(true)
    } catch (err) {
      setError(err.message)
    } finally {
      setBusy(false)
    }
  }

  if (done) {
    return (
      <div className="auth-page">
        <div className="auth-card">
          <h1>Request submitted</h1>
          <p>
            Thanks, <strong>{username}</strong>. An administrator must approve your
            account before you can sign in. You’ll be able to log in once it’s approved.
          </p>
          <div className="auth-alt">
            <Link to="/">← Back to store</Link>
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="auth-page">
      <form className="auth-card" onSubmit={onSubmit}>
        <h1>Create an account</h1>
        <p className="muted">
          Accounts are free. New sign-ups are reviewed by an administrator before
          they’re activated.
        </p>

        {error && <div className="notice error">{error}</div>}

        <label>
          Username
          <input value={username} onChange={(e) => setUsername(e.target.value)} autoFocus required
                 placeholder="3–32 chars, letters/numbers/_-." />
        </label>
        <label>
          Email
          <input type="email" value={email} onChange={(e) => setEmail(e.target.value)} required />
        </label>
        <label>
          Password
          <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} required
                 placeholder="at least 8 characters" />
        </label>

        <button className="btn-primary" type="submit" disabled={busy}>
          {busy ? 'Submitting…' : 'Request account'}
        </button>

        <div className="auth-alt">
          Already have an account? <Link to="/login">Sign in</Link>
        </div>
      </form>
    </div>
  )
}
