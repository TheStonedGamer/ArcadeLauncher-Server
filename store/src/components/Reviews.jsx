// Player reviews for a game page: the aggregate score with a star histogram,
// the signed-in user's own review (write / edit / delete), then everyone else's.
//
// Rows are shared with the launcher — the same `game_reviews` row backs a review
// written here and one written in the desktop client — so editing is an upsert
// rather than a second post, and there is exactly one review per person.

import { useEffect, useState } from 'react'
import { deleteReview, fetchReviews, putReview } from '../api.js'
import { useAuth } from '../auth.jsx'
import Stars from './Stars.jsx'

const BODY_MAX = 4000

function fmtWhen(unixSecs) {
  if (!unixSecs) return ''
  return new Date(unixSecs * 1000).toLocaleDateString(undefined, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
  })
}

function ReviewRow({ r }) {
  return (
    <li className={`review${r.mine ? ' mine' : ''}`}>
      <div className="review-head">
        <span className="review-who">{r.username || 'Someone'}</span>
        <Stars value={r.rating} size={14} />
        <span className="review-when">{fmtWhen(r.updatedAt)}</span>
        {r.mine && <span className="review-badge">Your review</span>}
      </div>
      {r.body && <p className="review-body">{r.body}</p>}
    </li>
  )
}

export default function Reviews({ gameId }) {
  const { user } = useAuth()
  const [data, setData] = useState(null)
  const [error, setError] = useState(null)
  const [editing, setEditing] = useState(false)
  const [rating, setRating] = useState(0)
  const [body, setBody] = useState('')
  const [busy, setBusy] = useState(false)

  function load() {
    return fetchReviews(gameId)
      .then((d) => {
        setData(d)
        const own = (d.reviews || []).find((r) => r.mine)
        // Seed the form from the existing review so "Edit" starts from what you
        // wrote, not from blank.
        setRating(own ? own.rating : 0)
        setBody(own ? own.body : '')
        setEditing(false)
      })
      .catch((e) => setError(e.message))
  }

  useEffect(() => {
    setData(null)
    setError(null)
    load()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [gameId])

  async function save() {
    if (rating < 1) return
    setBusy(true)
    setError(null)
    try {
      await putReview(gameId, rating, body)
      await load()
    } catch (e) {
      setError(e.message)
    } finally {
      setBusy(false)
    }
  }

  async function remove() {
    setBusy(true)
    setError(null)
    try {
      await deleteReview(gameId)
      await load()
    } catch (e) {
      setError(e.message)
    } finally {
      setBusy(false)
    }
  }

  if (error) return <div className="notice error">Couldn’t load reviews: {error}</div>
  if (!data) return <div className="notice">Loading reviews…</div>

  const reviews = data.reviews || []
  const mine = reviews.find((r) => r.mine)
  const others = reviews.filter((r) => !r.mine)
  const hist = data.histogram || [0, 0, 0, 0, 0]

  return (
    <section className="reviews">
      <h2>Player reviews</h2>

      <div className="reviews-summary">
        <div className="reviews-score">
          <div className="reviews-avg">{data.count ? data.average.toFixed(1) : '—'}</div>
          <Stars value={data.average} size={18} />
          <div className="muted">
            {data.count} {data.count === 1 ? 'review' : 'reviews'}
          </div>
        </div>
        <div className="reviews-hist">
          {[5, 4, 3, 2, 1].map((star) => {
            const n = hist[star - 1] || 0
            const pct = data.count ? (n / data.count) * 100 : 0
            return (
              <div className="hist-row" key={star}>
                <span className="hist-star">{star}★</span>
                <span className="hist-bar"><span style={{ width: `${pct}%` }} /></span>
                <span className="hist-n">{n}</span>
              </div>
            )
          })}
        </div>
      </div>

      {/* Signed-out visitors can read reviews but not write one; the sign-in
          prompt sits where the form would be rather than hiding the section. */}
      {!user ? (
        <p className="muted">Sign in to write a review.</p>
      ) : mine && !editing ? (
        <div className="review-mine-actions">
          <button className="lib-btn review-edit" onClick={() => setEditing(true)}>
            Edit your review
          </button>
          <button className="lib-btn review-del" onClick={remove} disabled={busy}>
            Delete
          </button>
        </div>
      ) : (
        <div className="review-form">
          <div className="review-form-row">
            <span className="k">Your rating</span>
            <Stars value={rating} onChange={setRating} size={22} />
          </div>
          <textarea
            className="review-text"
            rows={4}
            maxLength={BODY_MAX}
            placeholder="What did you think? (optional)"
            value={body}
            onChange={(e) => setBody(e.target.value)}
          />
          <div className="review-form-row">
            <button className="lib-btn review-save" onClick={save} disabled={busy || rating < 1}>
              {busy ? 'Saving…' : mine ? 'Update review' : 'Post review'}
            </button>
            {mine && (
              <button className="lib-btn review-cancel" onClick={() => setEditing(false)}>
                Cancel
              </button>
            )}
            <span className="muted">
              {rating < 1 ? 'Pick a star rating first' : `${body.length}/${BODY_MAX}`}
            </span>
          </div>
        </div>
      )}

      <ul className="review-list">
        {mine && !editing && <ReviewRow r={mine} />}
        {others.map((r) => (
          <ReviewRow key={r.userId} r={r} />
        ))}
        {reviews.length === 0 && (
          <li className="muted">No reviews yet — be the first.</li>
        )}
      </ul>
    </section>
  )
}
