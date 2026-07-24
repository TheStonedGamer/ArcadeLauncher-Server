// Add-to-Library / In-Library toggle. Signed-out clicks route to /login (with a
// return path); signed-in clicks add or remove ownership. `stop` prevents the
// click from bubbling to an enclosing card <Link>.
import { useState } from 'react'
import { useNavigate, useLocation } from 'react-router-dom'
import { useAuth } from '../auth.jsx'

export default function LibraryButton({ id, className = '', stop = false }) {
  const { user, isOwned, addToLibrary, removeFromLibrary } = useAuth()
  const navigate = useNavigate()
  const location = useLocation()
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState(null)
  const owned = isOwned(id)

  async function onClick(e) {
    if (stop) { e.preventDefault(); e.stopPropagation() }
    if (!user) {
      navigate('/login', { state: { from: location.pathname } })
      return
    }
    setBusy(true)
    setErr(null)
    try {
      if (owned) await removeFromLibrary(id)
      else await addToLibrary(id)
    } catch (e2) {
      setErr(e2.message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <button
      className={`lib-btn ${owned ? 'owned' : ''} ${className}`}
      onClick={onClick}
      disabled={busy}
      title={err || (owned ? 'Remove from your library' : 'Add to your library — free')}
    >
      {busy ? '…' : owned ? 'In Library ✓' : '+ Add to Library'}
    </button>
  )
}
