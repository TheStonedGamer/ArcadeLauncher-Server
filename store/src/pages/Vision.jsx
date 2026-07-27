// "Our Vision" — the pitch for what Arcade Launcher is trying to be.
//
// Deliberately static: no API calls, no auth gate. It is the page a visitor
// reads *before* they have an account, so it renders identically signed in or
// out, and links onward to the download and the catalog.

import { Link } from 'react-router-dom'
import { useAuth } from '../auth.jsx'

const PRINCIPLES = [
  {
    title: 'Free first',
    body:
      'The catalog leads with games that cost nothing to play — freeware, open source, ' +
      'fan projects, demos, and the long tail of titles nobody sells anymore. If ' +
      'something does cost money, that is the exception and it is labelled, not buried ' +
      'behind a checkout flow you discover at the last step.',
  },
  {
    title: 'One library, every device',
    body:
      'Your library, playtime, ratings and reviews live on the server, not on one ' +
      'machine. Add a game on the website, and it is waiting in the launcher on your ' +
      'desktop. Play on a different PC and your hours follow you.',
  },
  {
    title: 'No storefront theater',
    body:
      'No ads, no season passes, no "recommended for you" that is really an ad slot, no ' +
      'dark patterns designed to make you spend. The homepage recommends things because ' +
      'they match what you actually play — nothing is paid placement, because there is ' +
      'nothing to pay for placement with.',
  },
  {
    title: 'One click from browse to playing',
    body:
      'Finding a game should not mean hunting a download mirror, reading an install ' +
      'guide, configuring an emulator, and mapping a controller. Pick it, and the ' +
      'launcher handles the fetch, the runtime and the controls.',
  },
  {
    title: 'Curated by the people playing',
    body:
      'Ratings, reviews and playtime come from the people here, not from a review ' +
      'aggregator or a marketing budget. A small honest catalog beats a huge padded one.',
  },
  {
    title: 'Yours, not rented',
    body:
      'Nothing phones home to decide whether you are allowed to play. The library is ' +
      'self-hosted — it is a server you or someone you trust runs, and it keeps working ' +
      'on your terms.',
  },
]

const STEPS = [
  {
    n: '1',
    title: 'Browse',
    body: 'Search the catalog here on the web. Every game has art, a description, screenshots and what other players thought.',
  },
  {
    n: '2',
    title: 'Add to your library',
    body: 'One button. No cart, no checkout, no payment details.',
  },
  {
    n: '3',
    title: 'Play',
    body: 'Open the launcher on your desktop and hit install. It pulls the files, sets up whatever it needs to run, and launches straight into the game.',
  },
]

export default function Vision() {
  const { user } = useAuth()

  return (
    <div className="vision">
      <section className="vision-hero">
        <h1>Our Vision</h1>
        <p className="vision-lede">
          Games should be easy to find, free to start, and instant to play. Arcade
          Launcher is a games library built around that idea — a catalog you can
          browse like a storefront, without any of the parts of a storefront that
          exist to take your money.
        </p>
      </section>

      <section className="vision-why">
        <h2>Why this exists</h2>
        <p>
          There is an enormous amount of gaming worth playing that costs nothing —
          freeware and open-source projects, decades of games that were never
          re-released, fan work, demos and jams. It is scattered across dead links,
          forum posts and archives, and actually playing any of it usually means
          hunting a download, working out which runtime it needs, configuring it,
          and fixing the controls before you see a title screen.
        </p>
        <p>
          Meanwhile the storefronts that <em>are</em> easy to use spend most of their
          effort on selling: ads dressed as recommendations, currencies, seasonal
          urgency, and a library you only ever license. We wanted the convenient part
          without the extractive part.
        </p>
        <p className="vision-thesis">
          So: the polish of a modern store, pointed at games that are free or nearly
          free, with the whole path from “that looks interesting” to “I’m playing it”
          collapsed into a couple of clicks.
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
          already here — the catalog, the shared library, the desktop launcher, the
          mobile app, reviews from the people playing — and the work from here is
          mostly depth: better discovery, richer game pages, more of the catalog
          properly described and illustrated, and fewer steps between finding
          something and running it.
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
          <Link className="btn-secondary btn-lg" to="/">
            Browse the catalog
          </Link>
        ) : (
          <Link className="btn-secondary btn-lg" to="/register">
            Create an account
          </Link>
        )}
      </section>
    </div>
  )
}
