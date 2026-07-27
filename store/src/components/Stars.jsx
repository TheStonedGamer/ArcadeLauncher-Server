// Star rating, read-only or interactive.
//
// Read-only mode renders a fixed 5-star row where the filled portion is clipped
// to the fraction, so 4.67 looks like 4.67 rather than being rounded to 5.
// Interactive mode renders real <button>s so the control is keyboard-usable.

export default function Stars({ value = 0, onChange, size = 16, label }) {
  const clamped = Math.max(0, Math.min(5, value || 0))

  if (!onChange) {
    return (
      <span
        className="stars"
        style={{ fontSize: `${size}px` }}
        role="img"
        aria-label={label || `${clamped.toFixed(1)} out of 5`}
      >
        <span className="stars-empty">★★★★★</span>
        <span className="stars-fill" style={{ width: `${(clamped / 5) * 100}%` }}>★★★★★</span>
      </span>
    )
  }

  return (
    <span className="stars stars-input" style={{ fontSize: `${size}px` }}>
      {[1, 2, 3, 4, 5].map((n) => (
        <button
          key={n}
          type="button"
          className={`star-btn${n <= clamped ? ' on' : ''}`}
          onClick={() => onChange(n)}
          aria-label={`${n} star${n > 1 ? 's' : ''}`}
          aria-pressed={n === clamped}
        >
          ★
        </button>
      ))}
    </span>
  )
}
