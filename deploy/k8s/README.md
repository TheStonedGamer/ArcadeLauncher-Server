# ArcadeLauncher server on k3s

> **Live as of 2026-07-24.** Deployed to a dedicated single-node k3s VM
> `arcade-k3s` (10.0.0.112, Ubuntu 24.04, on pve3). The public site
> `https://arcade.orlandoaio.net` is served from this cluster; the old CT
> (10.0.0.210) is kept running as instant rollback. See "What was actually
> deployed" at the bottom.


Runs the server as horizontally-scalable replicas. Content stays on NFS (no
object-store migration): every API replica mounts the existing game library
read-only over a ReadWriteMany volume, and a single scanner pod owns catalog
scanning. Cross-pod social presence/roster goes through Redis (`src/fanout.rs`,
already built — this just supplies `ARCADE_REDIS_URL`). MariaDB runs in-cluster.

## Topology

| Workload | Replicas | Library | Scanner | Exposed |
|---|---|---|---|---|
| `arcade-api` | 2 (HPA→6) | RO | off | public API (Ingress), admin (internal Svc) |
| `arcade-scanner` | 1 (Recreate) | RW | on | nothing (no Service) |
| `arcade-mariadb` | 1 (StatefulSet) | — | — | in-cluster only |
| `arcade-redis` | 1 | — | — | in-cluster only |

The one code change enabling this is `ARCADE_ENABLE_SCANNER` (src/models.rs,
src/main.rs): default `true` keeps the CT single-instance behavior; the API
Deployment sets it `false` so only the scanner pod watches/rescans the library.

## Before you apply — fill these in

1. **`10-secret.yaml`** — `cp 10-secret.example.yaml 10-secret.yaml`, set real
   values. It is gitignored; never commit it.
2. **`40-library-nfs.yaml`** — set `spec.nfs.server` and `spec.nfs.path` to the
   NFS export whose contents equal `/srv/arcade-library` on the app CT. The k3s
   nodes must be able to mount it.
3. **`20-mariadb.yaml`** — set `volumeClaimTemplates.storageClassName` to your
   cluster's storage class if it has no default.
4. **Image tag** — manifests pin `brianthemint/arcadelauncher-server:0.13.0`.
   Bump when you push a new tag.

## Apply order (build first)

```sh
# Build + push the image (on the workstation Docker Desktop, per the build rule):
docker build -t brianthemint/arcadelauncher-server:$(cat ../../VERSION) \
             -t brianthemint/arcadelauncher-server:latest -f ../../Dockerfile ../..
docker push brianthemint/arcadelauncher-server:$(cat ../../VERSION)
docker push brianthemint/arcadelauncher-server:latest

# From the cluster (ssh atlas@10.0.0.81, sudo -n k3s kubectl):
kubectl apply -f 00-namespace.yaml
kubectl apply -f 10-secret.yaml          # your filled-in copy, NOT the .example
kubectl apply -f 11-config.yaml
kubectl apply -f 20-mariadb.yaml
kubectl apply -f 30-redis.yaml
kubectl apply -f 40-library-nfs.yaml
kubectl apply -f 50-arcade-api.yaml
kubectl apply -f 60-arcade-scanner.yaml
kubectl apply -f 70-ingress.yaml
```

## Stage 4 — data migration (GATED: do not run without explicit go)

Load prod data into the in-cluster StatefulSet, then verify counts match:

```sh
# On the app CT (10.0.0.210), dump the live DB:
mysqldump --single-transaction --routines arcadelauncher > arcade.sql
# Stream it into the cluster MariaDB (root password from the Secret):
kubectl -n arcade exec -i statefulset/arcade-mariadb -- \
  sh -c 'exec mariadb -uroot -p"$MARIADB_ROOT_PASSWORD" arcadelauncher' < arcade.sql
# Sanity: row counts on both sides must match.
kubectl -n arcade exec statefulset/arcade-mariadb -- \
  mariadb -uroot -p"$MARIADB_ROOT_PASSWORD" -e \
  'SELECT COUNT(*) games FROM arcadelauncher.games;'
```

Library content needs no migration — the NFS PV points at the same files.

## Stage 5 — cutover (GATED: do not run without explicit go)

Pre-cutover smoke test against the cluster (internal, before touching public DNS):

```sh
kubectl -n arcade port-forward svc/arcade-api 8721:8721 &
curl -s localhost:8721/api/health           # expect the running version
# Prove multi-replica fan-out: scale to 2, connect two /ws/social sockets, confirm
# a presence frame from a socket on pod A reaches a socket on pod B (needs Redis).
```

Then flip the **one** upstream line in the public nginx (10.0.0.203) from
`10.0.0.210:8721` to the k3s ingress (or NodePort 30721). Keep the CT running —
rollback is reverting that single line. Watch `/api/metrics` (Prometheus already
scrapes the target).

## What was actually deployed (2026-07-24)

Reality differed from the generic manifests above in a few ways — recorded here:

- **Cluster:** dedicated single-node k3s VM `arcade-k3s` @ **10.0.0.112** (Ubuntu
  24.04 cloud-init, 4 vCPU / 8 GiB / 80 GiB, VMID 112 on pve3), *not* the shared
  Atlas cluster. All pods land on this one node.
- **Library (no object-store migration):** staged on the node host at
  `/srv/arcade-library`, exposed via a **hostPath** PV (see `40-library-nfs.yaml`),
  because single-node makes NFS-reexport unnecessary:
  - `/srv/arcade-library/games` → NFS mount of the NAS roms export
    `10.0.0.102:/mnt/Assets Pool/Roms` (~1.3 TiB), pinned in the VM's `/etc/fstab`.
  - the rest (emulators ~1.9 GiB + metadata) rsynced once from the CT; owned by
    uid 10001 to match the container user.
- **Config/secret:** instead of the ConfigMap+Secret split, the live
  `arcade-secrets` Secret is generated imperatively from the CT's env file with
  cluster overrides (`ARCADE_DB_HOST`→service, `ARCADE_HOST`/`ADMIN_HOST`→0.0.0.0,
  `ARCADE_REDIS_URL` added, a fresh `MARIADB_ROOT_PASSWORD`). Both deployments
  `envFrom` just that Secret, so runtime config matches prod exactly. `11-config.yaml`
  is kept as documentation of the non-secret knobs but is not applied.
- **Cutover mechanism:** the public nginx (10.0.0.203) upstream was repointed from
  `10.0.0.210:8721` to the **NodePort** `10.0.0.112:30721` (`arcade-api-nodeport`,
  in `70-ingress.yaml`), not the Traefik Ingress — avoids Host-routing coupling.
  Rollback = revert the one nginx upstream line to `10.0.0.210:8721` + reload
  (a `.pre-k3s.bak` backup sits in `/etc/nginx/backups/`).
- **Verified live:** `/api/health` = 0.12.0, `/api/catalog` = 2050 games (parity
  with prod), `/art/:id` = 200 JPEG, a `/files/:id/*` range read = 206 from the NAS
  mount, both API replicas connected to Redis fan-out, `/requests/` = 200, and
  `/ws/social` auth behavior identical to the old backend.

## Public storefront SPA (2026-07-24)

A Steam-style public store website now serves `https://arcade.orlandoaio.net`. It is
a separate Vite/React SPA (source in `../../store/`) built into an nginx image
`brianthemint/arcadelauncher-store` and deployed by `80-store.yaml` (2 replicas +
`arcade-store-nodeport` on **30080**).

- **Public API surface:** new *unauthenticated* endpoints in `src/store_api.rs`
  (server ≥ 0.13.0): `GET /api/store/summary`, `GET /api/store/games`,
  `GET /api/store/games/:id`. They expose only catalog metadata + aggregate
  community stats (playtime/ratings/reviews) — never tokens, user rows, or
  `content_path`. Art (`/art/:id`) was already public. The desktop/mobile client
  endpoints (`/api/catalog`, manifests, saves…) stay auth-gated — a browser hitting
  `/api/catalog` correctly gets 401.
- **Edge routing (10.0.0.203):** the arcade vhost now splits traffic — `/ws/`,
  `/requests/`, and `/(api|art|files|chunks|textures)` go **direct** to the API
  NodePort `10.0.0.112:30721` (fewest hops for big transfers); everything else
  (`/`, `/game/:id`, `/assets/*`) goes to the store pod NodePort `10.0.0.112:30080`,
  which serves the bundle and does SPA history fallback. Backups in
  `/etc/nginx/backups/arcade.*.pre-store.bak`.
- **⚠ nginx gotcha (cost real debugging time):** on this edge box,
  `/etc/nginx/sites-enabled/arcade.orlandoaio.net` is **a real file, NOT a symlink**
  to `sites-available/`. `nginx.conf` includes `sites-enabled/`, so editing
  `sites-available` has **no effect** — and it was silently stale, still pointing at
  the old CT `10.0.0.210:8721`. Always edit/verify the `sites-enabled` copy (or
  `grep proxy_pass` it) after any change here. Rollback = restore the `.pre-store.bak`
  over the `sites-enabled` file + `systemctl reload nginx`.
- **Verified live:** `/` = 200 store SPA, `/game/:id` = 200 (SPA fallback),
  `/api/store/summary` = 2050 games / 9 platforms, `/api/health` = 0.13.0,
  `/art/:id` = 200, `/requests/` = 200, `/api/catalog` = 401 (expected).

### Building/deploying the store SPA

```sh
# workstation Docker Desktop (per the build rule):
cd store && docker build -t brianthemint/arcadelauncher-store:<ver> -t brianthemint/arcadelauncher-store:latest .
docker push brianthemint/arcadelauncher-store:<ver> && docker push brianthemint/arcadelauncher-store:latest
# on the node (ssh brian@10.0.0.112):
sudo -n k3s kubectl -n arcade set image deploy/arcade-store store=brianthemint/arcadelauncher-store:<ver>
sudo -n k3s kubectl -n arcade rollout status deploy/arcade-store
```

### Regenerating the secret (if creds rotate)

```sh
# on the app CT: dump env with cluster overrides, copy to the VM, then:
kubectl -n arcade create secret generic arcade-secrets \
  --from-env-file=cluster.env --dry-run=client -o yaml | kubectl apply -f -
kubectl -n arcade rollout restart deploy/arcade-api deploy/arcade-scanner
# then shred cluster.env — never leave it on disk.
```
