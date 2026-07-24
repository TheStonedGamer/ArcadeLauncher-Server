import { useEffect, useMemo, useState } from 'react'
import { fetchDownloads, fmtBytes } from '../api.js'

// Best-effort OS guess for highlighting the primary button.
function detectOS() {
  const p = (navigator.userAgent + ' ' + (navigator.platform || '')).toLowerCase()
  if (p.includes('win')) return 'windows'
  if (p.includes('mac')) return 'macos'
  if (p.includes('linux') || p.includes('x11')) return 'linux'
  return 'windows'
}

const OS_LABEL = { windows: 'Windows', linux: 'Linux', macos: 'macOS' }

export default function Download() {
  const [info, setInfo] = useState(null)
  const [error, setError] = useState(null)
  const os = useMemo(detectOS, [])

  useEffect(() => {
    fetchDownloads().then(setInfo).catch((e) => setError(e.message))
  }, [])

  const files = info?.files || []
  const primary = files.find((f) => f.platform === os && f.primary) || files.find((f) => f.platform === os)

  return (
    <div className="download-page">
      <section className="hero">
        <h1>Download Arcade Launcher</h1>
        {info && <p className="hero-stats">Latest version <strong>{info.version}</strong></p>}
        <p className="muted">
          Install the launcher, sign in, and any game in your library is ready to install.
        </p>
      </section>

      {error && <div className="notice error">Couldn’t load downloads: {error}</div>}
      {!info && !error && <div className="notice">Loading downloads…</div>}

      {primary && (
        <div className="download-primary">
          <a className="btn-primary btn-lg" href={primary.url}>
            Download for {OS_LABEL[os] || os}
          </a>
          <div className="muted">
            {primary.label}{primary.size ? ` · ${fmtBytes(primary.size)}` : ''}
          </div>
        </div>
      )}

      {files.length > 0 && (
        <div className="download-all">
          <h2>All downloads</h2>
          <table className="dl-table">
            <thead>
              <tr><th>Platform</th><th>File</th><th>Size</th><th></th></tr>
            </thead>
            <tbody>
              {files.map((f) => (
                <tr key={f.url}>
                  <td>{OS_LABEL[f.platform] || f.platform}{f.arch ? ` (${f.arch})` : ''}</td>
                  <td>{f.label || f.filename}</td>
                  <td>{f.size ? fmtBytes(f.size) : '—'}</td>
                  <td><a className="dl-link" href={f.url}>Download</a></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {info && files.length === 0 && (
        <div className="empty-state"><p>No installers are published yet.</p></div>
      )}
    </div>
  )
}
