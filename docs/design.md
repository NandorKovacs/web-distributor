# Request handling and certificates

The Rust CLI owns the route inventory, group files, and generated fragment. It validates the inputs, serializes CLI edits with a file lock, and replaces the fragment by rename. The operator validates and reloads the master Caddyfile to apply the change. A failed reload leaves the last loaded configuration serving traffic.

## Routing order

Within a named site, the generated Caddy route runs these handlers in order:

1. Add a two-year Strict-Transport-Security header. If the site has a Basic auth group, require a valid account from that group.
2. If a literal gRPC prefix matches, forward the request to the configured upstream over cleartext HTTP/2. This preserves native gRPC messages and trailers. If both gRPC and Authentik are configured, the backend must enforce the intended authentication for the gRPC branch.
3. On Authentik sites, proxy the outpost namespace directly to the outpost. For other paths, clear incoming identity headers and check access at `/outpost.goauthentik.io/auth/caddy`. Return the outpost's denial or redirect to the caller. On success, propagate its session cookie and forward verified identity headers with the original request. [Authentik Caddy integration](https://docs.goauthentik.io/add-secure-apps/providers/proxy/server_caddy/)
4. Proxy remaining requests to the HTTP upstream, preserving the public host, path and query. Caddy handles uploads and WebSocket upgrades. Upstreams specified by DNS name must resolve from inside the container.

Removing a route stops publishing that hostname as a proxy site. Its Basic and Authentik assignments persist in the inventory, so adding the hostname again retains those access controls.

## Certificates

Caddy obtains and renews certificates for route and certificate-only names. The master Caddyfile controls the issuer, persistent filesystem storage, HTTP redirects and the default TLS site. An unknown HTTP host over HTTPS receives a 404; the fallback name supplies a certificate for missing or unrecognized SNI. That certificate is valid only for names it covers.

The SMTP server consumes a certificate for a name in `certificate_only`. On cobalt, a systemd timer invokes `web-distributor-migrate sync-smtp-tls`. The helper reads the managed pair from the web container, verifies its trust chain, hostname and key, and installs changed files in the SMTP container before reloading Postfix. The guest's Caddy service handles web traffic independently of this host timer.
