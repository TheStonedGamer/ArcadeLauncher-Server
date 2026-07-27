// "Featured & Recommended" hero for the storefront home page — the web
// counterpart of the desktop client's StoreFeatured. A wide banner with a right
// info rail (title, score, blurb, library CTA), rotating through the picks the
// server recommends from the signed-in user's tracked playtime.
//
// The picks come from /api/store/featured rather than being computed here: the
// browser has no local playtime, and the ranking is shared with the desktop so
// both surfaces agree.

import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { fmtRating } from '../api.js'
import LibraryButton from './LibraryButton.jsx'

/** Dwell time per pick. Long enough to read the blurb, short enough to notice. */
const ROTATE_MS = 9000

export default function Featured({ picks, personalized }) {
  const [index, setIndex] = useState(0)

  // Advance on a timer; restart only when the shortlist's *contents* change, so
  // an unrelated re-render can't keep resetting the rotation to the first pick.
  const key = picks.map((p) => p.id).join(',')
  useEffect(() => {
    setIndex(0)
    const count = key ? key.split(',').length : 0
    if (count < 2) return undefined
    const id = setInterval(() => setIndex((i) => (i + 1) % count), ROTATE_MS)
    return () => clearInterval(id)
  }, [key])

  const game = picks[index] || picks[0]
  if (!game) return null

  // Prefer the server's wide 1080p key art; the cover is portrait and much
  // smaller, so it is only a fallback for games IGDB has no artwork for.
  const art = game.heroArtUrl || game.coverArtUrl
  const score = fmtRating(game.igdbRating)
  const href = `/game/${encodeURIComponent(game.id)}`

  return (
    <section className="featured">
      <div className="featured-head">
        <h2>Featured &amp; Recommended</h2>
        {personalized && (
          <span className="featured-why" title="Picked from the games you play most">
            Based on your playtime
          </span>
        )}
        {picks.length > 1 && (
          <div className="featured-dots" role="tablist" aria-label="Recommendations">
            {picks.map((p, i) => (
              <button
                key={p.id}
                role="tab"
                aria-selected={i === index}
                aria-label={`Recommendation ${i + 1} of ${picks.length}`}
                className={`featured-dot${i === index ? ' active' : ''}`}
                onClick={() => setIndex(i)}
              />
            ))}
          </div>
        )}
      </div>
      <div className="featured-body">
        <Link to={href} className="featured-art" title={game.title}>
          {art ? (
            <img src={art} alt={game.title} />
          ) : (
            <span className="featured-logo">{game.title}</span>
          )}
        </Link>
        <div className="featured-rail">
          <Link to={href} className="featured-title">{game.title}</Link>
          <div className="featured-sub">
            {game.platform}
            {score != null ? ` · Critic score ${score}/100` : ''}
          </div>
          {game.summary && <p className="featured-summary">{game.summary}</p>}
          <div className="featured-cta">
            <LibraryButton id={game.id} className="featured-lib-btn" />
            <span className="featured-price">Free</span>
          </div>
        </div>
      </div>
    </section>
  )
}
