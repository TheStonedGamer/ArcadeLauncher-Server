import { useEffect, useMemo, useState } from 'react'
import { Link, useNavigate, useLocation } from 'react-router-dom'
import { QRCodeSVG } from 'qrcode.react'
import { useAuth } from '../auth.jsx'
import * as api from '../api.js'

export default function Login() {
  const { login, acceptQrLogin } = useAuth()
  const navigate = useNavigate()
  const location = useLocation()
  const from = location.state?.from || '/'

  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [totp, setTotp] = useState('')
  const [needTotp, setNeedTotp] = useState(false)
  const [error, setError] = useState(null)
  const [busy, setBusy] = useState(false)
  const [qr, setQr] = useState(null)
  const [qrError, setQrError] = useState(null)
  const [qrBusy, setQrBusy] = useState(false)

  const qrValue = useMemo(() => {
    if (!qr) return ''
    const params = new URLSearchParams({
      server: window.location.origin,
      id: qr.challengeId,
      secret: qr.scanSecret,
    })
    return `arcadelauncher://signin?${params.toString()}`
  }, [qr])

  useEffect(() => {
    if (!qr) return undefined
    let live = true
    const timer = window.setInterval(async () => {
      try {
        const result = await api.pollQrSignin(qr.challengeId, qr.pollToken)
        if (!live || result.status !== 'complete') return
        window.clearInterval(timer)
        await acceptQrLogin(result)
        navigate(from, { replace: true })
      } catch (err) {
        if (!live || err.status === 202) return
        window.clearInterval(timer)
        setQr(null)
        setQrError(err.message)
      }
    }, 1500)
    return () => {
      live = false
      window.clearInterval(timer)
    }
  }, [qr, acceptQrLogin, navigate, from])

  async function beginQrSignin() {
    setQrBusy(true)
    setQrError(null)
    try {
      const device = `Web browser on ${navigator.platform || 'this device'}`
      setQr(await api.startQrSignin('store', device))
    } catch (err) {
      setQrError(err.message)
    } finally {
      setQrBusy(false)
    }
  }

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
          {busy ? 'Signing in...' : 'Sign in'}
        </button>

        <div className="auth-divider"><span>or</span></div>
        {qr ? (
          <div className="qr-signin">
            <div className="qr-signin-code">
              <QRCodeSVG value={qrValue} size={208} level="M" marginSize={2} />
            </div>
            <strong>Scan with Arcade Launcher</strong>
            <p className="muted">Open the mobile app's QR Login tab and approve this browser.</p>
            <button className="linkbtn" type="button" onClick={() => setQr(null)}>Cancel QR sign-in</button>
          </div>
        ) : (
          <button className="btn-secondary" type="button" onClick={beginQrSignin} disabled={qrBusy}>
            {qrBusy ? 'Creating QR code...' : 'Sign in with QR code'}
          </button>
        )}
        {qrError && <div className="notice error">{qrError}</div>}

        <div className="auth-alt">
          No account? <Link to="/register">Create one</Link>
        </div>
      </form>
    </div>
  )
}
