# Caddy replacement design

Caddy owns the public HTTP and HTTPS listeners, reverse proxying and certificate renewal. The Rust CLI owns the route inventory and login groups. It translates the existing TOML and bcrypt files into Caddy site blocks. This keeps existing operator commands usable and gives Caddy the complete routing table in one reload.

## Production inventory

The source configuration is `/etc/web-distributor.toml`. All 43 routes in the surveyed configuration regenerated the corresponding active nginx include and per-domain ACME configuration. The comparison covered complete directive bodies after trimming whitespace. There were four types of named site.

| Sites | Observed behavior | Caddy equivalent |
| ---: | --- | --- |
| 33 | Ordinary HTTP upstream, including apps that use WebSockets or large uploads | `reverse_proxy` to the configured address |
| 3 | Site-wide Basic authentication | A bcrypt challenge with realm `login` before application handling |
| 5 | Authentik forward authentication with a shared outpost | Public outpost callback path, authentication check, then application proxy |
| 2 | Native gRPC below a path prefix, ordinary HTTP on the rest of the site | A case-sensitive literal prefix selects an h2c upstream |

Across those sites, 32 targets use IPv4 addresses and 11 use Incus DNS names. Twenty omit the port and use port 80. All 43 use cleartext to the application, including the target on port 8443. The Incus resolver and access to the containers' bridge networks are runtime dependencies. Three of four configured Basic group memberships refer to active sites. The fourth group has a credential file but its hostname has no route. Only the route table creates proxy sites.

Three responsibilities live outside the generated routes.

| Existing owner | Behavior to carry through cutover |
| --- | --- |
| A hand-written nginx IPv4 default TLS server | Present the configured fallback certificate and return 404 for an unknown HTTP host. Configure both absent-SNI and unmatched-SNI certificate selection in the Caddy entry point. |
| A separate ACME certificate definition | The mail hostname needs a public certificate. A daily timer on the Incus host copies the chain and private key to the SMTP container and reloads an active Postfix after a certificate change. |
| A separate systemd unit running socat | The TCP listener on port 9999 forwards webhooks directly to a container. Its ownership stays with that unit. |

## Request handling

Named sites accept HTTPS over IPv4 and IPv6, negotiate HTTP/2 and send a two-year HSTS header. The ordinary HTTP proxy retains the incoming Host, request URI and query, supports WebSocket upgrades and streams uploads. Preserve the `X-Real-IP`, `X-Scheme` and `X-Forwarded-*` information that applications use to reconstruct their public URL and identify the peer. At the public edge, Caddy derives forwarding information from the connection. [Caddy reverse proxy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)

The gRPC selector must match a service-method path such as `/inventory.v1.Stock/Get`. The period in the prefix is literal. Keep the service path intact and select HTTP/2 cleartext for that request. Other paths on the same site use the ordinary proxy. Test an actual HTTP/2 call including its trailers alongside an HTTP request to the same authority.

On protected sites, route ordering gives the Authentik outpost its own callback namespace. For application requests, the gateway receives a bodyless authentication check at `/outpost.goauthentik.io/auth/caddy`. Its redirect or denial goes to the client. On successful authentication, forward the original method, body and URI with trusted user, groups, email and display-name headers. Propagate session cookies to the client. Incoming identity headers must be cleared before copying outpost values. [Authentik's Caddy integration](https://docs.goauthentik.io/add-secure-apps/providers/proxy/server_caddy/) · [Caddy handler ordering](https://caddyserver.com/docs/caddyfile/directives/route)

## Configuration and certificate ownership

Keep the proxy-facing change small. Existing `add`, `remove`, `list`, `generate`, `login-group` and `forward-auth` commands use the existing TOML and group file formats. Changes to credentials require regeneration and reload because Caddy holds the users in its running configuration. Treat the generated file as credential material. Validate the entire Caddy entry point, including its generated import, before reloading the service. [Caddy Basic authentication](https://caddyserver.com/docs/caddyfile/directives/basic_auth) · [Caddy CLI](https://caddyserver.com/docs/command-line)

Explicit certificate-only names give Caddy an inventory for non-web consumers. The mail name serves a static 404 over HTTPS. Host-wide settings such as the HTTPS fallback, HTTP redirect, issuer and persistent certificate storage belong in the operator's entry point. This gives Caddy responsibility for challenges and renewals previously split across the port-80 redirect daemon, hourly renewal timer and nginx reload hooks. [Caddy automatic HTTPS](https://caddyserver.com/docs/automatic-https) · [Caddy SNI options](https://caddyserver.com/docs/caddyfile/options)

For the transition, use the existing valid certificates to serve traffic while Caddy obtains its managed certificates. Retain the old credentials and unit configuration for rollback. Switch the mail consumer to the managed chain and key only after verifying the hostname, trust, lifetime and correspondence between the key and certificate. Verify the resulting certificate at the SMTP listener as well as the HTTPS listener. The private inventory, credentials, issued certificates and issuer account data belong to the deployment.
