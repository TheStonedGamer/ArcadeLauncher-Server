import { useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { fetchGames, fetchSummary, fmtRating } from '../api.js'
import LibraryButton from '../components/LibraryButton.jsx'

function GameCard({ g }) {
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
        <Link to={`/game/${encodeURIComponent(g.id)}`} className="card-title-link">
          <div className="card-title" title={g.title}>{g.title}</div>
          <div className="card-meta">{g.platform}</div>
        </Link>
        <LibraryButton id={g.id} className="card-lib-btn" />
      </div>
    </div>
  )
}

export default function Home() {
  const [games, setGames] = useState(null)
  const [summary, setSummary] = useState(null)
  const [error, setError] = useState(null)
  const [query, setQuery] = useState('')
  const [platform, setPlatform] = useState('all')

  useEffect(() => {
    fetchSummary().then(setSummary).catch(() => {})
    fetchGames()
      .then((d) => setGames(d.games || []))
      .catch((e) => setError(e.message))
  }, [])

  const platforms = useMemo(() => {
    if (!games) return []
    return [...new Set(games.map((g) => g.platform).filter(Boolean))].sort()
  }, [games])

  const filtered = useMemo(() => {
    if (!games) return []
    const q = query.trim().toLowerCase()
    return games.filter((g) => {
      if (platform !== 'all' && g.platform !== platform) return false
      if (q && !g.title.toLowerCase().includes(q)) return false
      return true
    })
  }, [games, query, platform])

  return (
    <div className="home">
      <section className="hero">
        <h1>The Arcade Library</h1>
        {summary && (
          <p className="hero-stats">
            <strong>{summary.totalGames.toLocaleString()}</strong> games across{' '}
            <strong>{summary.totalPlatforms}</strong> platforms ·{' '}
            <strong>{summary.totalPlaytimeHours.toLocaleString()}</strong> hours played
          </p>
        )}
      </section>

      <div className="toolbar">
        <input
          className="search"
          type="search"
          placeholder="Search games…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <select className="filter" value={platform} onChange={(e) => setPlatform(e.target.value)}>
          <option value="all">All platforms</option>
          {platforms.map((p) => (
            <option key={p} value={p}>{p}</option>
          ))}
        </select>
        <span className="result-count">{filtered.length} results</span>
      </div>

      {error && <div className="notice error">Couldn’t load the catalog: {error}</div>}
      {!games && !error && <div className="notice">Loading catalog…</div>}

      <div className="grid">
        {filtered.map((g) => (
          <GameCard key={g.id} g={g} />
        ))}
      </div>
    </div>
  )
}
