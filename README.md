# Web Distributor

Web Distributor generates a Caddy configuration from a TOML route table. Caddy
serves the domains on a shared IP address, proxies requests to upstream
services, and manages HTTPS certificates and redirects.

## Configure Caddy

Build and install the generator with `cargo install --path .`. Use Caddy 2.10
or newer. Install Caddy with your system's service manager and configure its
service to load a master Caddyfile. For a default installation the master is
`/etc/caddy/Caddyfile`.

```caddyfile
import /etc/web-distributor/v2/Caddyfile
```

The generator reads `/etc/web-distributor/v2/config.toml` by default and writes
`Caddyfile` under the configured `home` (default `/etc/web-distributor/v2`). Set
up the output directory so the operator can write it and the Caddy service
account can traverse the directory and read generated files. For a service
running as the `caddy` group and generation as root, this is one setup.

```sh
install -d -o root -g caddy -m 2750 /etc/web-distributor/v2
web-distributor generate
sudo -u caddy caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
systemctl reload caddy
```

Run the validation with the Caddy service's user, filesystem access and
environment. Use `systemctl start caddy` on the first install. Public
hostnames must resolve to this server, with TCP ports 80 and 443 reachable
for redirects and ACME challenges. Allow UDP 443 if serving HTTP/3.
Keep Caddy's certificate storage persistent and private. [Caddy Automatic HTTPS](https://caddyserver.com/docs/automatic-https)

## Manage routes

`config.toml` is an example. The supported settings are:

- `home` holds the generated Caddy fragment and `login_groups` directory.
- `[routes]` maps a hostname to an upstream, for example
  `"app.example.com" = "192.0.2.10:8080"`.
- `[login_groups]` maps a hostname to a file in `<home>/login_groups`.
  The file uses the existing `name:bcrypt-hash` format. The generated
  fragment includes the hashes and needs the same access controls.
- `[forward_auth]` maps a hostname to its Authentik outpost upstream.
- `[grpc_prefix]` maps a hostname to a path prefix for native gRPC over h2c
  to that route's upstream. Other paths use the regular HTTP proxy. As in the
  legacy configuration, a gRPC location bypasses Authentik forward-auth.
- `certificate_only` lists DNS names Caddy manages certificates for, with
  a 404 HTTPS response. Set it at the top level
  of the TOML file.

```sh
web-distributor add app.example.com 192.0.2.10:8080
web-distributor add --grpc-prefix /inventory.v1. inventory.example.com 192.0.2.11:8080
web-distributor forward-auth apply app.example.com 192.0.2.12:9000
web-distributor --help
```

Route and authentication changes regenerate the fragment. Validate the
master Caddyfile and reload Caddy to apply the changes. When editing TOML
manually, run `web-distributor generate` first. Use `-c FILE` for another
TOML file and `--output FILE` to generate a candidate fragment into a
separate validation directory.

The migration runbook covers snapshot review, certificate continuity,
SMTP certificate sync, cutover and rollback. [Nginx and acme-redirect migration runbook](ops/migrate-to-caddy.md)
