import { Link, NavLink, Outlet } from 'react-router-dom'
import { useAuth } from './auth.jsx'

// Shell: top nav + routed page outlet + footer. Steam-like dark chrome.
export default function App() {
  const { user, logout } = useAuth()

  return (
    <div className="app">
      <header className="topbar">
        <div className="topbar-inner">
          <Link to="/" className="brand">
            <span className="brand-mark">▶</span>
            <span className="brand-text">Arcade Launcher</span>
          </Link>
          <nav className="topnav">
            <NavLink to="/" end>Store</NavLink>
            {user && <NavLink to="/library">Library</NavLink>}
            <NavLink to="/download">Download</NavLink>
          </nav>
          <div className="topnav-account">
            {user ? (
              <>
                <span className="account-name">{user.username}</span>
                <button className="linkbtn" onClick={logout}>Sign out</button>
              </>
            ) : (
              <>
                <NavLink to="/login" className="linkbtn">Sign in</NavLink>
                <NavLink to="/register" className="btn-primary btn-sm">Sign up</NavLink>
              </>
            )}
          </div>
        </div>
      </header>
      <main className="content">
        <Outlet />
      </main>
      <footer className="footer">
        <div className="footer-inner">
          <span>Arcade Launcher</span>
          <span className="muted">
            Community catalog · free to add · playtime and ratings aggregated from players
          </span>
        </div>
      </footer>
    </div>
  )
}
