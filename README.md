# Web Distributor

Web Distributor is a Rust CLI for running multiple HTTPS sites behind one Caddy server. It turns a TOML route table and optional login groups into a Caddyfile fragment. Caddy serves the traffic and obtains and renews the certificates.

**[Build and deploy a release to the Incus container on cobalt](docs/deploy.md)**, including validation and rollback.

## Manage routes

Run these commands as root **inside** the `web-distributor` container. The installed `web-distributor` command selects the active release.

```sh
web-distributor list
web-distributor add app.example.com 192.0.2.10:8080
web-distributor add --grpc-prefix /inventory.v1. inventory.example.com 192.0.2.11:8080
web-distributor forward-auth apply app.example.com 192.0.2.12:9000
web-distributor remove app.example.com
```

Route and credential edits regenerate the fragment. After making your changes, validate the complete Caddy configuration as the service user and reload it:

```sh
sudo -u caddy caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
systemctl reload caddy
```

If you edit the TOML directly, run `web-distributor generate` before validation. Use `-c FILE` for a different inventory and `generate --output FILE` to preview the generated fragment. See `web-distributor --help` and `web-distributor login-group --help` for the remaining commands.

## Configuration

| File or directory | Purpose |
| --- | --- |
| `/etc/web-distributor/v2/config.toml` | Source inventory. See the [example TOML](config.toml). |
| `/etc/web-distributor/v2/login_groups/` | One file per Basic auth group, containing usernames and bcrypt password hashes. |
| `/etc/web-distributor/v2/Caddyfile` | Generated site blocks. This file includes password hashes and must be readable by the Caddy service. |
| `/etc/caddy/Caddyfile` | Operator-maintained master that imports the generated fragment and sets server-wide TLS and redirect behavior. |

`[routes]` maps hostnames to HTTP upstreams. `[login_groups]` assigns site-wide Basic auth. `[forward_auth]` assigns an Authentik outpost. `[grpc_prefix]` sends a literal path prefix to the same backend over cleartext HTTP/2; this branch runs after Basic auth and before Authentik. Top-level `certificate_only` lists names such as a mail host that require a certificate and return an HTTPS 404. `home` changes the directory used for login groups and the default fragment.

For a fresh installation, install Caddy 2.10 or later and the generator. On Arch Linux, Caddy is the `caddy` pacman package. Create the inventory directory with `install -d -o root -g caddy -m 2750 /etc/web-distributor/v2`, install your inventory with mode `0600`, and run `web-distributor generate`. Adapt the [example master Caddyfile](ops/Caddyfile.example) with your ACME email, fallback hostname and storage path. The fallback name must be one of your managed sites. Validate the master before enabling and starting `caddy.service`. Public DNS and inbound TCP 80/443 must reach the server for this setup.

See [request handling and certificate ownership](docs/design.md) for routing order and the separate SMTP certificate consumer.
