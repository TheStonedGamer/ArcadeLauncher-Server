# ArcadeLauncher voice — TURN/STUN setup (ROADMAP T9g)

WebRTC voice is peer-to-peer. Most calls connect with STUN alone, but peers
behind **symmetric NAT** need a **TURN relay**. This directory deploys [coturn]
and the app server mints short-lived TURN credentials per call
(`GET /api/social/turn`, the coturn REST-API / `static-auth-secret` scheme).

## One shared secret, two places

A freshly generated 256-bit secret for this install:

```
526ee929164075abf3c99ac8a943a41ab88f96e63e940a7352014892cfb53907
```

Set the **same** value in both:

- **coturn**: `static-auth-secret=` in `turnserver.conf`
- **app server**: `ARCADE_TURN_SECRET=` in the systemd env file

(Rotate it anytime by regenerating with `openssl rand -hex 32` and updating both.)

## Deploy coturn (recommended: Docker host 10.0.0.180)

```sh
# on 10.0.0.180
mkdir -p /opt/arcade-coturn && cd /opt/arcade-coturn
# copy docker-compose.yml + turnserver.conf.example here
cp turnserver.conf.example turnserver.conf
# edit turnserver.conf: static-auth-secret, realm, external-ip
docker compose up -d
docker logs -f arcade-coturn
```

Open/forward these to 10.0.0.180 (only needed for off-LAN callers):
`3478/udp`, `3478/tcp`, `5349/tcp` (TLS), and `49160-49200/udp` (relay range).

> Alternative: a dedicated Proxmox CT running coturn from the distro package
> works identically — same `turnserver.conf`. The container is simpler to manage.

## Point the app server at it

In the server env file (`deploy/arcadelauncher-server.env.example` documents
these), then restart `arcadelauncher-server`:

```sh
ARCADE_TURN_SECRET=526ee929...53907
ARCADE_TURN_URLS=turn:turn.orlandoaio.net:3478?transport=udp,turns:turn.orlandoaio.net:5349?transport=tcp
ARCADE_STUN_URLS=stun:stun.l.google.com:19302
ARCADE_TURN_TTL=3600
```

With these unset the endpoint returns STUN-only and voice still works on open NATs.

## nginx (10.0.0.203)

TURN media does **not** pass through nginx — it's its own UDP/TCP service. nginx
only needs to keep proxying `GET /api/social/turn` (already covered by the
existing `location /` block; no change required).

**Optional**, only if you want TURN-over-TLS on **443** to punch through hostile
firewalls: add an nginx **stream** block (see `deploy/turn/nginx-stream.conf.example`)
that SNI-routes `turn.<domain>:443` to coturn's `5349`. This requires nginx built
with `--with-stream` and a `stream {}` context in `nginx.conf` (not inside `http {}`).

## Verify

After deploy, an authed `GET /api/social/turn` should return an `iceServers`
array containing your `turn:`/`turns:` URLs with a fresh `username`/`credential`.
Trickle-ICE test: <https://webrtc.github.io/samples/src/content/peerconnection/trickle-ice/>
— paste the URL + credential and confirm a `relay` candidate appears.

[coturn]: https://github.com/coturn/coturn
