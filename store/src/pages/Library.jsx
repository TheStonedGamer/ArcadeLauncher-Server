import { useEffect, useState } from 'react'
import { Link, Navigate } from 'react-router-dom'
import { fetchLibrary, fmtRating } from '../api.js'
import { useAuth } from '../auth.jsx'
import LibraryButton from '../components/LibraryButton.jsx'

function LibCard({ g }) {
  const rating = fmtRating(g.igdbRating)
  return (
    <div className="card">
      <Link to={`/game/${encodeURIComponent(g.id)}`} className="card-art-link">
        <div className="card-art">
          {g.coverArtUrl ? (
            <img src={g.coverArtUrl} alt={g.title} loading="lazy" />
          ) : (
            <div className="card-art-fallback">{g.title}</div>
          )}
          {rating != null && <span className="card-score">{rating}</span>}
        </div>
      </Link>
      <div className="card-body">
        <div className="card-title" title={g.title}>{g.title}</div>
        <div className="card-meta">{g.platform}</div>
        <LibraryButton id={g.id} className="card-lib-btn" />
      </div>
    </div>
  )
}

export default function Library() {
  const { user, loading: authLoading, ownedIds } = useAuth()
  const [games, setGames] = useState(null)
  const [error, setError] = useState(null)

  // Re-fetch when the owned set changes (add/remove elsewhere keeps this fresh).
  useEffect(() => {
    if (!user) return
    fetchLibrary()
      .then((d) => setGames(d.games || []))
      .catch((e) => setError(e.message))
  }, [user, ownedIds])

  if (authLoading) return <div className="notice">Loading…</div>
  if (!user) return <Navigate to="/login" state={{ from: '/library' }} replace />

  return (
    <div className="home">
      <section className="hero">
        <h1>Your library</h1>
        <p className="hero-stats">
          Games here are yours to install in the launcher. Everything is free — browse the{' '}
          <Link to="/">store</Link> to add more.
        </p>
      </section>

      {error && <div className="notice error">Couldn’t load your library: {error}</div>}
      {!games && !error && <div className="notice">Loading your library…</div>}
      {games && games.length === 0 && (
        <div className="empty-state">
          <p>Your library is empty.</p>
          <Link to="/" className="btn-primary">Browse the store</Link>
        </div>
      )}

      <div className="grid">
        {games && games.map((g) => <LibCard key={g.id} g={g} />)}
      </div>
    </div>
  )
}
