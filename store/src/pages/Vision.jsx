// "Our Vision" — the pitch for what Arcade Launcher is trying to be.
//
// Deliberately static: no API calls, no auth gate. It is the page a visitor
// reads *before* they have an account, so it renders identically signed in or
// out, and links onward to the download and the catalog.

import { Link } from 'react-router-dom'
import { useAuth } from '../auth.jsx'

const PRINCIPLES = [
  {
    title: 'Ask for what you actually want',
    body:
      'The catalog is not a fixed list you pick from. Request a PC game, other people ' +
      'vote it up, and the ones people actually want are the ones that get added. The ' +
      'library grows towards what this group plays instead of what someone decided to ' +
      'stock.',
  },
  {
    title: 'Free to start, cheap to stay',
    body:
      'Nothing here costs you anything to browse, add or play. The point is to remove ' +
      'money as the thing standing between you and trying a game — no cart, no ' +
      'checkout, no per-title price tag to weigh up before you find out whether you ' +
      'even like it.',
  },
  {
    title: 'No storefront theater',
    body:
      'No ads, no season passes, no "recommended for you" that is really a paid ad ' +
      'slot, no dark patterns built to make you spend. The homepage recommends things ' +
      'because they match what you actually play — nothing is paid placement, because ' +
      'there is nothing to pay for placement with.',
  },
  {
    title: 'One click from browse to playing',
    body:
      'Finding a PC game should not mean hunting a download, reading an install guide, ' +
      'chasing dependencies and mapping a controller before you see a title screen. ' +
      'Pick it, and the launcher handles the fetch, the install and the setup.',
  },
  {
    title: 'One library, every device',
    body:
      'Your library, playtime, ratings and reviews live on the server, not on one ' +
      'machine. Add a game on the website and it is waiting in the launcher on your ' +
      'desktop. Reinstall, switch PCs, and your library and hours follow you.',
  },
  {
    title: 'Curated by the people playing',
    body:
      'Ratings, reviews and playtime come from the people here, not from a review ' +
      'aggregator or a marketing budget. A small honest catalog of games people ' +
      'actually wanted beats a huge padded one.',
  },
]

const STEPS = [
  {
    n: '1',
    title: 'Find it — or ask for it',
    body:
      'Search the catalog here on the web. Every game has art, a description, ' +
      'screenshots and what other players thought. Not there? Request it, and other ' +
      'people can vote it up.',
  },
  {
    n: '2',
    title: 'Add to your library',
    body: 'One button. No cart, no checkout, no payment details.',
  },
  {
    n: '3',
    title: 'Play',
    body:
      'Open the launcher on your desktop and hit install. It pulls the files, sets up ' +
      'whatever the game needs to run, and launches straight into it.',
  },
]

export default function Vision() {
  const { user } = useAuth()

  return (
    <div className="vision">
      <section className="vision-hero">
        <h1>Our Vision</h1>
        <p className="vision-lede">
          PC games should be easy to get, free to start, and instant to play. Arcade
          Launcher is a shared games library built around that idea — you browse it
          like a storefront, ask for the games you actually want, and play them
          without any of the parts of a storefront that exist to take your money.
        </p>
      </section>

      <section className="vision-why">
        <h2>Why this exists</h2>
        <p>
          Getting a PC game you want is rarely one step. It is spread across half a
          dozen launchers that each want to be the one you open, a price tag you have
          to weigh up before you know whether you even like the thing, and — for
          anything outside the mainstream — a hunt through dead links and install
          guides, chasing dependencies until it finally runs.
        </p>
        <p>
          Meanwhile the storefronts that <em>are</em> easy to use spend most of their
          effort on selling: ads dressed as recommendations, seasonal urgency,
          currencies, and a library you only ever license. The convenience is real.
          It is just wrapped around something that is not working for you.
        </p>
        <p className="vision-thesis">
          So: the polish of a modern store, pointed at the PC games people here
          actually want, free to start, with the whole path from “that looks
          interesting” to “I’m playing it” collapsed into a couple of clicks.
        </p>
      </section>

      <section className="vision-principles">
        <h2>What we’re building</h2>
        <div className="vision-grid">
          {PRINCIPLES.map((p) => (
            <div className="vision-card" key={p.title}>
              <h3>{p.title}</h3>
              <p>{p.body}</p>
            </div>
          ))}
        </div>
      </section>

      <section className="vision-how">
        <h2>How it works</h2>
        <ol className="vision-steps">
          {STEPS.map((s) => (
            <li key={s.n}>
              <span className="vision-step-n">{s.n}</span>
              <div>
                <h3>{s.title}</h3>
                <p>{s.body}</p>
              </div>
            </li>
          ))}
        </ol>
      </section>

      <section className="vision-next">
        <h2>Where this is going</h2>
        <p>
          It is a small project and it is still being built. The shape of it is
          already here — the catalog, the shared library, requests and voting, the
          desktop launcher, the mobile app, reviews from the people playing — and the
          work from here is mostly depth: more PC games, better discovery, richer game
          pages, and fewer steps between finding something and running it.
        </p>
        <p className="muted">
          If something feels clunky, that is worth saying out loud — most of what
          gets built next comes from someone pointing at a rough edge.
        </p>
      </section>

      <section className="vision-cta">
        <Link className="btn-primary btn-lg" to="/download">
          Get the launcher
        </Link>
        {user ? (
          <>
            <Link className="btn-secondary btn-lg" to="/">
              Browse the catalog
            </Link>
            {/* Requests are a separate server-rendered app, not an SPA route. */}
            <a className="btn-secondary btn-lg" href="/requests">
              Request a game
            </a>
          </>
        ) : (
          <Link className="btn-secondary btn-lg" to="/register">
            Create an account
          </Link>
        )}
      </section>
    </div>
  )
}
