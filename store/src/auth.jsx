// Storefront auth + library state. One provider holds the signed-in user and the
// set of owned game ids, so any page can render "In Library ✓" and the nav can
// show Sign in / Sign out without prop-drilling. The session itself lives in an
// HttpOnly cookie; this context only mirrors "who am I" + "what do I own".
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react'
import * as api from './api.js'

const AuthContext = createContext(null)

export function AuthProvider({ children }) {
  const [user, setUser] = useState(null)
  const [ownedIds, setOwnedIds] = useState(() => new Set())
  const [loading, setLoading] = useState(true)

  const refreshLibrary = useCallback(async () => {
    try {
      const data = await api.fetchLibrary()
      setOwnedIds(new Set((data.games || []).map((g) => g.id)))
    } catch {
      setOwnedIds(new Set())
    }
  }, [])

  // On first load, resolve the cookie session and (if present) the library.
  useEffect(() => {
    let live = true
    ;(async () => {
      try {
        const u = await api.me()
        if (!live) return
        setUser(u)
        if (u) await refreshLibrary()
      } finally {
        if (live) setLoading(false)
      }
    })()
    return () => { live = false }
  }, [refreshLibrary])

  const login = useCallback(async (username, password, totp) => {
    const u = await api.login(username, password, totp)
    setUser(u)
    await refreshLibrary()
    return u
  }, [refreshLibrary])

  const logout = useCallback(async () => {
    try { await api.logout() } catch { /* ignore */ }
    setUser(null)
    setOwnedIds(new Set())
  }, [])

  const addToLibrary = useCallback(async (id) => {
    await api.addToLibrary(id)
    setOwnedIds((prev) => new Set(prev).add(id))
  }, [])

  const removeFromLibrary = useCallback(async (id) => {
    await api.removeFromLibrary(id)
    setOwnedIds((prev) => {
      const next = new Set(prev)
      next.delete(id)
      return next
    })
  }, [])

  const value = useMemo(() => ({
    user,
    loading,
    ownedIds,
    isOwned: (id) => ownedIds.has(id),
    login,
    logout,
    addToLibrary,
    removeFromLibrary,
    refreshLibrary,
  }), [user, loading, ownedIds, login, logout, addToLibrary, removeFromLibrary, refreshLibrary])

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>
}

export function useAuth() {
  const ctx = useContext(AuthContext)
  if (!ctx) throw new Error('useAuth must be used within AuthProvider')
  return ctx
}
