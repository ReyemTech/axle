# Run with Docker

The image is `ghcr.io/reyemtech/axle`, multi-arch (amd64 and arm64), built on distroless and
running as a non-root user (uid 65532). Its entrypoint is `axl` and its default command is
`serve --bind 0.0.0.0:7420`. SQLite data goes in the `/data` volume.

Tags: `latest` and the release version (for example `0.1.0`).

## SQLite

```sh
docker run -d --name axle \
  -p 127.0.0.1:7420:7420 \
  -v axle-data:/data \
  -e DATABASE_URL=sqlite:///data/axl.db \
  -e AXL_ADMIN_SECRET="$(openssl rand -hex 16)" \
  ghcr.io/reyemtech/axle:latest
```

Inside the container the server binds `0.0.0.0`, so it refuses to start until you choose a
UI sign-in. The example uses a secret; read it back with
`docker inspect axle --format '{{range .Config.Env}}{{println .}}{{end}}' | grep AXL_ADMIN_SECRET`,
or set your own value. The other choices:

- `-e AXL_TRUSTED_HEADER=X-Auth-Request-Email` behind an authenticating proxy that sets and
  strips that header.
- `-e AXL_UI_AUTH=none` when the port is reachable only from your machine or a VPN. Add
  `-e AXL_ALLOWED_HOSTS=<name>` for any host name you browse to other than `localhost`.

The `-p 127.0.0.1:…` mapping publishes the port on loopback only. Widen it only behind TLS.

Machines connect from **Tokens → Connect a machine** in the web UI (set `AXL_PUBLIC_URL`;
see [below](#letting-machines-connect)). To create a token by hand instead, run this inside
the container:

```sh
docker exec axle axl token create --label laptop
```

The token is printed once. Put it in `~/.axl/token` (mode 0600) on the machine that runs the
harness, never in a prompt or a committed file.

## Postgres

```sh
docker run -d --name axle \
  -p 127.0.0.1:7420:7420 \
  -e DATABASE_URL=postgres://axle:password@db.example.com:5432/axle \
  -e AXL_ADMIN_SECRET="$(openssl rand -hex 16)" \
  ghcr.io/reyemtech/axle:latest
```

No volume is needed; the schema is migrated on start.

## Compose

```yaml
services:
  axle:
    image: ghcr.io/reyemtech/axle:latest
    restart: unless-stopped
    ports:
      - "127.0.0.1:7420:7420"
    environment:
      DATABASE_URL: sqlite:///data/axl.db
      AXL_ADMIN_SECRET: ${AXL_ADMIN_SECRET:?set AXL_ADMIN_SECRET (16+ characters)}
    volumes:
      - axle-data:/data
volumes:
  axle-data:
```

```sh
export AXL_ADMIN_SECRET="$(openssl rand -hex 16)"
docker compose up -d
docker compose exec axle axl token create --label laptop
```

## Letting machines connect

Set `AXL_PUBLIC_URL` to the address people will use, so **Tokens → Connect a machine** and
`/install.sh` can hand it out. Add one line to either `docker run` above (or to the `environment:` of your compose file):

```sh
  -e AXL_PUBLIC_URL=https://axle.example.com \
```

Behind a reverse proxy also set `AXL_TRUSTED_PROXY_HEADER` (for example `X-Forwarded-For`)
so the enrollment limit counts clients, not the proxy; see
[security](../security.md#enrollment).

## Behind a reverse proxy

Terminate TLS in the proxy and set `AXL_COOKIE_SECURE=true`. The UI and `/v1/events` use
server-sent events, so turn off response buffering and allow idle connections of at least
an hour. For nginx:

```nginx
location / {
    proxy_pass http://127.0.0.1:7420;
    proxy_set_header Host $host;
    proxy_buffering off;
    proxy_read_timeout 3600s;
    proxy_send_timeout 3600s;
}
```

## Build the image yourself

```sh
docker build -t axle .
```

`tests/smoke.sh` builds the image and checks it end to end.

## Amazon ECR

Releases can also be mirrored to Amazon ECR when the maintainers enable it; see
[releasing](../releasing.md).
