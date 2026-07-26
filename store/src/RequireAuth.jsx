import { Navigate, useLocation } from 'react-router-dom'
import { useAuth } from './auth.jsx'

export default function RequireAuth({ children }) {
  const { user, loading } = useAuth()
  const location = useLocation()

  if (loading) {
    return <div className="notice">Checking your session…</div>
  }

  if (!user) {
    return <Navigate to="/login" replace state={{ from: location.pathname + location.search }} />
  }

  return children
}
