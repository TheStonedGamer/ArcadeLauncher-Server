# ArcadeLauncher — Implementation Roadmap

Execution tracker for the product vision in **`social.md`** (the v1→v3+ roadmap:
7 phases + 7 milestones) and the audit in
[`ARCHITECTURE_REVIEW.md`](../ArcadeLauncher-Client/ARCHITECTURE_REVIEW.md).
`social.md` = the *what/why* (product areas); this file = the *how/when*
(sequenced, dependency-ordered increments). Goal: implement **every** feature, in
full, one shippable increment at a time.

**social.md → ROADMAP mapping:** Milestone 1 (Notifications v2, Friend Requests
v2, Cloud Saves v2, Downloads v2, **account hardening**) ≈ Phase 0 + parts of
Phase 1/2.7/3.3 here. Milestone 2 (DMs/Voice/Groups/Presence/Parties) ≈ Phase 1
+ 2.1–2.3. Milestone 3 (Profiles/Activity/Screenshots/Reviews) ≈ 1.4 + 3.7.
Milestones 4–7 ≈ Phase 2 (library) + Phase 3 (communities/platform/scale).
Check items off as they land. Each step should compile, be backward-compatible
where possible, and note any required client/server lockstep.

**Reality check (verified against code 2026-06-14):** presence and DM messages
already persist in MariaDB (`social_presence`, `social_messages`); friendships use
a canonical `(lo,hi)` pair with pending/accepted + `requested_by`; blocks are
directional; avatars are server-synced. `social_hub` is an in-process live-socket
map only. So the roadmap starts from a more advanced baseline than the review
assumed.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done. Each step tagged with the
repos it touches (S=server, C=client) and whether it breaks version lockstep.

---

## Phase 0 — Foundations (durable, sequenced, scalable state)

- [x] **0.1 Persistent notifications** — _Server DONE + DEPLOYED LIVE (v1.2.8,
  2026-06-14); client consumption DONE in 0.2._
  - [x] `social_notifications` table in `ensure_social_schema`.
  - [x] `store_notification()` helper (persist + best-effort live push).
  - [x] Wired friend_request + friend_accepted to persist.
  - [x] `deliver_pending_notifications()` batch on gateway connect.
  - [x] REST `GET /api/social/notifications` + `POST /api/social/notifications/read`.
  - [ ] (later) persist message + voice_invite notifications (with DM step 1.2).
- [x] **0.2 Notification center UI** (C) — _DONE (client, builds clean)._ Bell
  panel now reads the **server-persisted** feed: consumes live `notification`
  frames (toast) + connect `notifications` backlog batch (silent), `RefreshNotifications()`
  REST `GET` on start/reconnect, dedup by `serverId`, unread badge, mark-read on
  open now also `POST`s `/notifications/read{upToId}`, deep-link to friend/DM via
  `actorId`. Patch-level (additive); needs a client release to ship.
- [x] **0.3 Resume / message backfill** (S, C) — _Server DONE + DEPLOYED LIVE
  (v1.2.10, 2026-06-14); client pushed (auto-update)._ Implemented pragmatically on the existing
  monotonic `social_messages.id` rather than a separate `event_seq` log (simpler,
  sufficient): client tracks the highest message id seen (`m_lastMsgId`), sends
  `{"type":"resume","afterMsgId":N}` on reconnect; server's `backfill_messages`
  replies with one `{"type":"chat_backfill","messages":[…]}` batch (≤500, id>N)
  instead of the client re-pulling every conversation's full history. Client merges
  silently (dedup by id, replaces pending echoes, bumps unread, no toast).
  Backward-compatible (unknown frames ignored both ways) but new client wants the
  new server's resume support → **deploy server with/before the client release.**
- [x] **0.4 Redis presence + pub/sub fan-out** (S, infra) — _DEPLOYED LIVE DORMANT
  (v1.2.11, 2026-06-14): code in prod but no `ARCADE_REDIS_URL` set, so it runs
  single-instance identically. To go multi-instance: stand up a Redis container,
  set the env var, run >1 instance behind nginx._ New `src/fanout.rs`:
  optional Redis bus. Text `push` = local `deliver_local` + publish on
  `social:fanout`; each instance's subscriber delivers peer-origin frames locally
  (deduped by per-process instance id, so no double-delivery). Cross-instance
  online registry via `social:online:<uid>` keys (TTL refreshed on heartbeat);
  `presence_online()` checks local hub then Redis. `social_hub` stays the local
  socket registry. Binary voice audio stays local-only (cross-instance voice
  deferred to 2.1–2.3). Backward-compatible / patch-level; no client change.
- [x] **0.5 Server-synced preferences** (S, C) — _DEPLOYED LIVE (v1.2.14,
  2026-06-14); client pushed (auto-update). Endpoints verified 401-gated; tables
  created clean on boot._
  `social_user_prefs` table (one opaque JSON blob/user, last-write-wins) +
  `GET`/`POST /api/social/prefs`. Client mirrors `social_prefs.json` to the server
  on every save (`PushPrefsToServer`) and adopts the server copy on connect
  (`PullPrefsFromServer`, server authoritative) while keeping the local file as an
  offline cache. Additive/patch-level.
- [~] **0.6 Account hardening** (S, C) — _Server slice DEPLOYED LIVE (v1.2.14);
  throttle verified (5×401 → 429 w/ "try again in 300s")._
  - [x] `auth_audit` table + `audit()` helper wired into login success/fail,
    challenge login, logout, password change, TOTP enable/disable.
  - [x] Login brute-force throttle now enforced on launcher login (password +
    challenge paths), returning 429 with retry-after; previously only admin login
    used it.
  - [x] `GET /api/account/security` — recent security events for the account.
  - [ ] (later 0.6b) refresh tokens + rotation, multi-device sessions (needs the
    one-token-per-user model reworked), email verification, passwordless/remember-me.
  - [ ] (later) client "sessions & security" panel consuming `/account/security`.

## Phase 1 — Social parity (Steam/Discord/Battle.net)

- [~] **1.1 Friend-request state machine** (S) — _Server slice DEPLOYED LIVE
  (v1.2.16, 2026-06-14); additive/patch-level, no client change needed.
  `GET /api/social/privacy` verified 401-gated; schema created clean on boot._
  - [x] `ignore` action — silently drops an incoming pending request (deletes the
    row, sends **no** `friend_removed`, so the requester isn't told).
  - [x] cancel vs decline vs remove already distinct verbs; `ignore` adds the
    silent variant. decline/cancel/remove still notify both parties.
  - [x] Pending-request **expiry** — `expires_at` column (30-day TTL on new
    requests); `sweep_expired_requests()` cleans expired pending rows on the
    request/list paths (no dedicated sweeper task).
  - [x] **Rate limiting** — cap of 50 outstanding outgoing pending requests per
    account (`FRIEND_REQUEST_MAX_OUTGOING`), returns 429.
  - [x] **Privacy control (who-can-friend)** — `social_friend_settings.friend_policy`
    = everyone (default) | mutual (shared accepted friend required) | nobody;
    enforced in `api_social_request`; `GET`/`PUT /api/social/privacy`.
  - [x] **Block auto-decline** — already in place: blocking deletes the friendship
    row and `is_blocked_either` rejects future requests with 403.
  - [ ] (later 1.1b) DM privacy policy (who-can-DM), per-sender ignore/mute that
    survives re-requests, client UI for the privacy setting + ignore button.
- [ ] **1.2 DM upgrades** — read receipts, reactions, replies, edit/delete,
  pagination/infinite history, offline send queue, client SQLite cache.
- [ ] **1.3 DM attachments + screenshots** — MinIO object storage, presigned PUT.
- [ ] **1.4 User profiles** — avatar (done) + banner, bio, level/XP, profile view.
- [ ] **1.5 Friend organization** — groups/categories, pinning, friend notes
  (server-synced), friend search, mutual friends, suggested friends.
- [ ] **1.6 Presence depth** — custom status text, DND, invisible (done partially),
  idle auto-detect, device indicator, join/spectate, party presence.

## Phase 2 — Comms & library depth

- [ ] **2.1 Voice v2** — Opus codec, jitter buffer, packet-loss concealment,
  device selection, push-to-talk, voice-activation, noise suppression/AEC.
- [ ] **2.2 Voice NAT traversal** — Coturn (STUN/TURN), ICE; relay as fallback.
- [ ] **2.3 Voice rooms / group calls** — multi-party; SFU (LiveKit/mediasoup) if
  group/screen-share becomes real.
- [ ] **2.4 Library tracking** — playtime, last-played, completion, ratings,
  tags, notes.
- [ ] **2.5 Library organization** — smart/dynamic collections, folders, custom
  tags, duplicate detection.
- [ ] **2.6 Launch profiles** — per-game args, emulator profiles, per-game
  controller profiles, multi-disc, pre-launch validation.
- [ ] **2.7 Cloud Saves v2** — conflict resolution, multiple slots, version
  history, backups, compression, encryption, selective sync.
- [ ] **2.8 Cloud config sync** — settings, favorites, controller mappings,
  metadata edits.

## Phase 3 — Communities, platform & scale

- [ ] **3.1 Group chats / communities / channels** (text + voice), party chat,
  temporary game lobbies.
- [ ] **3.2 Notification redesign completion** — categories, priorities, grouping,
  quiet hours, sound routing, per-category mute, push.
- [ ] **3.3 Download v2** — multi-threaded chunking, delta patching, verify/repair,
  download scheduler/priorities, LAN/peer cache, disk preallocation.
- [ ] **3.4 Observability** — `tracing` structured logs (Loki), Prometheus
  metrics + Grafana, `audit_log` table, admin analytics.
- [ ] **3.5 Platform systems** — plugin system, theme engine, localization,
  crash reporting, diagnostics export, backup/restore, portable mode, feature
  flags, API versioning (`/api/v1`), full-text search.
- [ ] **3.6 Big Picture polish** — full controller setup, on-screen keyboard,
  keyboard-less first-run.
- [ ] **3.7 Steam-like extras (triaged)** — activity feed, user screenshots,
  achievements (where cores expose them), library/family sharing, game pages,
  news/events. (Skip: trading cards, marketplace, monetized cosmetics.)

---

### Working notes
- Server schema convention: idempotent `CREATE TABLE IF NOT EXISTS` + best-effort
  `ALTER TABLE ... ADD COLUMN` in `ensure_schema`/`ensure_social_schema`
  (`db_setup.rs` / `social_api.rs`). No migration framework — follow this style.
- Version lockstep: `[minor]` only when a change breaks client↔server
  compatibility; additive REST/WS frames stay patch-level.
- Don't push/deploy without the user's go-ahead; prod deploy to 10.0.0.210 needs
  per-turn authorization.
