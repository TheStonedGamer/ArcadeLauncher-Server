import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { fetchGame, fmtDate, fmtRating, fmtHours } from '../api.js'
import LibraryButton from '../components/LibraryButton.jsx'

function Stat({ label, value }) {
  return (
    <div className="stat">
      <div className="stat-value">{value}</div>
      <div className="stat-label">{label}</div>
    </div>
  )
}

export default function GameDetail() {
  const { id } = useParams()
  const [game, setGame] = useState(null)
  const [error, setError] = useState(null)
  const [shot, setShot] = useState(null)

  useEffect(() => {
    setGame(null)
    setError(null)
    fetchGame(id)
      .then((g) => {
        setGame(g)
        setShot(g.screenshots && g.screenshots.length ? g.screenshots[0] : null)
      })
      .catch((e) => setError(e.message))
  }, [id])

  if (error) return <div className="notice error">Couldn’t load this game: {error}</div>
  if (!game) return <div className="notice">Loading…</div>

  const rating = fmtRating(game.igdbRating)
  const s = game.stats || {}

  return (
    <div className="detail">
      <Link to="/" className="back-link">← Back to store</Link>

      <div className="detail-head">
        <h1>{game.title}</h1>
        <div className="detail-sub">
          {game.platform}
          {game.developer ? ` · ${game.developer}` : ''}
          {` · ${fmtDate(game.releaseDate)}`}
        </div>
      </div>

      <div className="detail-body">
        <div className="detail-media">
          <div className="detail-shot">
            {shot ? (
              <img src={shot} alt={`${game.title} screenshot`} />
            ) : game.coverArtUrl ? (
              <img src={game.coverArtUrl} alt={game.title} />
            ) : (
              <div className="card-art-fallback">{game.title}</div>
            )}
          </div>
          {game.screenshots && game.screenshots.length > 1 && (
            <div className="thumbs">
              {game.screenshots.map((url) => (
                <button
                  key={url}
                  className={`thumb ${url === shot ? 'active' : ''}`}
                  onClick={() => setShot(url)}
                >
                  <img src={url} alt="" loading="lazy" />
                </button>
              ))}
            </div>
          )}
        </div>

        <aside className="detail-side">
          {game.coverArtUrl && (
            <img className="detail-cover" src={game.coverArtUrl} alt={game.title} />
          )}
          <div className="detail-actions">
            <LibraryButton id={game.id} className="lib-btn-lg" />
            <div className="muted free-note">Free · installs from the Arcade Launcher</div>
          </div>
          <div className="side-meta">
            {rating != null && <div className="side-score">Critic score <b>{rating}</b>/100</div>}
            {game.developer && <div><span className="k">Developer</span> {game.developer}</div>}
            {game.publisher && <div><span className="k">Publisher</span> {game.publisher}</div>}
            {game.franchise && <div><span className="k">Franchise</span> {game.franchise}</div>}
            <div><span className="k">Release</span> {fmtDate(game.releaseDate)}</div>
            {game.genres && game.genres.length > 0 && (
              <div className="tags">
                {game.genres.map((t) => (
                  <span className="tag" key={t}>{t}</span>
                ))}
              </div>
            )}
          </div>
        </aside>
      </div>

      {game.summary && <p className="detail-summary">{game.summary}</p>}

      <section className="community">
        <h2>Community stats</h2>
        <div className="stats-row">
          <Stat label="Players" value={(s.playerCount || 0).toLocaleString()} />
          <Stat label="Total playtime" value={fmtHours(s.totalPlaytimeHours)} />
          <Stat label="Sessions" value={(s.playCount || 0).toLocaleString()} />
          <Stat
            label="Player rating"
            value={s.avgUserRating ? `${s.avgUserRating.toFixed(1)}/5` : '—'}
          />
          <Stat label="Reviews" value={(s.reviewCount || 0).toLocaleString()} />
        </div>
      </section>
    </div>
  )
}
