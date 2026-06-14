# ArcadeLauncher — Implementation Roadmap

Source of truth for the step-by-step build-out described in
[`ARCHITECTURE_REVIEW.md`](../ArcadeLauncher-Client/ARCHITECTURE_REVIEW.md).
Goal: implement **every** feature, in full, one shippable increment at a time.
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

- [~] **0.1 Persistent notifications** — _Server DONE (compiles, additive,
  patch-level); client consumption pending in 0.2._
  - [x] `social_notifications` table in `ensure_social_schema`.
  - [x] `store_notification()` helper (persist + best-effort live push).
  - [x] Wired friend_request + friend_accepted to persist.
  - [x] `deliver_pending_notifications()` batch on gateway connect.
  - [x] REST `GET /api/social/notifications` + `POST /api/social/notifications/read`.
  - [ ] (later) persist message + voice_invite notifications (with DM step 1.2).
- [ ] **0.2 Notification center UI** (C) — bell panel reads the persisted list,
  shows unread badge, mark-read on open, deep-link to friend/DM.
- [ ] **0.3 Event sequencing + resume** (S, C) — per-user monotonic `event_seq`;
  client sends `resume{last_seq}`; server backfills exactly what was missed
  (replaces "re-pull everything" on reconnect).
- [ ] **0.4 Redis presence + pub/sub fan-out** (S, infra) — move presence and
  cross-user routing off the in-process `social_hub` map so the server can run
  >1 instance. Keep `social_hub` as the local socket registry.
- [ ] **0.5 Server-synced preferences** (S, C) — move `social_prefs.json`
  (favorites, nicknames, notif toggles) into a `user_prefs` table; client caches.

## Phase 1 — Social parity (Steam/Discord/Battle.net)

- [ ] **1.1 Friend-request state machine** — ignore, cancel vs decline, expiry,
  rate limiting, privacy controls (who-can-friend/DM), block auto-decline.
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
