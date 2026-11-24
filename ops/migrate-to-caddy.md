# Migrate Nginx and acme-redirect to Caddy

## Capture and review

Capture and store a private snapshot outside the checkout, with the same
paths relative to the snapshot root that the services use on the host.
Include `etc/web-distributor.toml`, the configured `home/nginx` directory,
`etc/nginx`, `etc/acme-redirect.conf`, and the configured ACME definition
directory. Save `nginx -T` stdout as `nginx-effective.conf`. Save installed
unit definitions and active/enabled states for Nginx, acme-redirect and its
renew service/timer, and record listeners and firewalls. Preserve backup
copies of the existing binaries, configs, credential files, units and ACME
state, including `/var/lib/acme-redirect`, in root-only storage on the host.
The comparison tool needs the generated Nginx and ACME definitions.
Credentials and private keys can stay on the service host.

```sh
cargo build --release --bins
target/release/web-distributor-migrate prepare \
    --snapshot /path/to/private/snapshot \
    --output /path/to/private/caddy-candidate.toml
```

Review the report before proceeding. The helper compares generated vhosts
against the legacy templates and checks which files appear in `nginx -T`.
It names active non-generated configuration for manual inspection,
preserves the route and authentication tables (including inactive mappings),
and adds matching standalone single-name ACME certificates to
`certificate_only`. Review custom ACME hooks, multi-name certificates and
custom Nginx directives individually, including any use of the old
certificate files by other services. The candidate file is created with
mode 0600. Use the snapshot's login-group files unchanged in the candidate
`home/login_groups`, preserving ownership and access restrictions as
appropriate. When staging off host, change `home` in a private copy to the
staged directory with the copied group files.

The inspected deployment had 43 web vhosts matching the templates, a
manually configured TLS fallback with a 404 response, one standalone SMTP
certificate, and a separate TCP webhook forwarding unit. The webhook
listener or other independent service keeps its port and unit.

## Stage the master Caddyfile

Use the example master Caddyfile in staging.
Set `default_sni` and `fallback_sni` to the existing fallback
certificate's hostname and set the ACME account contact. Keep the
Caddy filesystem storage path writable by the service user and
persistent across restarts and redeployments. The example preserves
the legacy HTTP 301 status and returns 404 for unknown ACME challenge
tokens. Caddy answers its own active ACME challenges before the
configured HTTP routes. [Caddyfile.example](Caddyfile.example)

For the first switch, place one PEM bundle per currently served certificate
in `/etc/caddy/bootstrap`, including the fallback and the standalone SMTP
certificate. Each bundle contains the full certificate chain and private key.
Keep the directory and files readable by root and the Caddy service only.
For example, as root with the `caddy` service group:

```sh
install -d -o root -g caddy -m 2750 /etc/caddy/bootstrap
umask 027
cat /var/lib/acme-redirect/live/app.example.com/fullchain \
    /var/lib/acme-redirect/live/app.example.com/privkey \
    > /etc/caddy/bootstrap/app.example.com.pem
```

Check that every leaf covers its hostname, has sufficient validity left
for the migration window, and matches its private key. Folder loading puts
the certificates in Caddy's shared cache for the initial switch and
preserves the fallback certificate selection by SNI.

Generate the candidate fragment with
`web-distributor -c /path/to/private/caddy-candidate.toml --output /path/to/stage/Caddyfile generate`.
Adjust the master import for the candidate fragment during validation,
then restore the permanent import path on install. Run `caddy validate
--config PATH --adapter caddyfile` on the combined config in the same
environment and with the same access as the service.
Set the fragment's parent directory to a group the Caddy user can read
and traverse, with setgid to retain the group on replacement. Verify Caddy
can still read the fragment after a second generation.
[Directory permissions for generation as root](../README.md#configure-caddy)

Exercise a private staging instance using high ports, an isolated admin
endpoint/storage or separate host, and fixture certificates or Caddy
`tls internal` for synthetic names. Use a DNS name delegated to a test
endpoint for a public ACME canary against the Let's Encrypt staging
directory. Match production's firewall and A/AAAA routing to TCP 80/443
for that canary. Use separate storage for the staging issuer. Confirm the
test canary can complete issuance and be renewed before issuing against
the production CA. [Caddy ACME issuer configuration](https://caddyserver.com/docs/caddyfile/directives/tls)

Before the switch record one representative check each for HTTPS origin
routing and paths/queries, redirects, usernames and denied/accepted basic
auth, an Authentik sign-in round trip and callback, WebSocket upgrade, a
real native gRPC request and an ordinary request on the same hostname,
the default unknown/no-SNI response, and the webhook. Use both address
families where advertised. Preserve credentials outside command history
and diagnostic logs.

## Switch listeners and transfer certificate management

Schedule a switch window. Install the validated master and fragment with
private file modes and keep backups of the previous files. Use the
bootstrap master on the first start, so Caddy has valid certificates as
soon as it accepts connections. Leave the SMTP synchronizer reading its
previous valid certificate during this first step.

1. Stop and temporarily disable the acme-redirect renewal timer. Stop and
   wait for any in-flight `acme-redirect-renew.service`, then stop the
   acme-redirect daemon and Nginx. Check that TCP 80 and 443 are free
   before starting the Caddy service. Keep the webhook and other
   independent listeners running.
2. Start Caddy and repeat the baseline smoke checks. Confirm HTTP
   redirects, TLS chain/SNI on both IPv4 and IPv6, route/authentication
   behavior and the fallback 404. Check Caddy logs, listening sockets
   and health from outside the host.
3. After confirming public challenges can reach Caddy on 80/443 and the
   ACME limits permit the planned issuance, change
   `auto_https disable_redirects disable_certs` to
   `auto_https disable_redirects ignore_loaded_certs`.
   Validate and reload. Caddy will request managed certificates even
   while the copied certificates remain available. Check logs and the
   pinned issuer's storage for all expected names. If issuance is slow,
   retain the bootstrap configuration and certificates, then investigate
   public address records, DNS CAA, firewalls and the issuer's reported
   retry time. If certificates cannot be issued before the bootstrap
   copies approach expiry, roll back and resume the legacy renewal timer.
   [Caddy automatic HTTPS options](https://caddyserver.com/docs/caddyfile/options#auto-https)
4. When all production names have trusted managed certificates, remove
   the bootstrap `tls { load ... }` block and use
   `auto_https disable_redirects`. Validate, reload and repeat the TLS and
   routing checks. Keep the fallback SNI settings and the `https://:443`
   site that returns 404. Verify the default certificate is available
   even for a clean restart. Enable the Caddy service on boot and disable
   the replaced services as appropriate. Keep the legacy configuration
   and certificates for rollback.

## Keep SMTP certificate sync working

The existing daily SMTP sync on the Incus host pulls the standalone mail
certificate from the web-distributor guest and installs it into Postfix.
Configure that hostname under `certificate_only` and use the pinned
production Let's Encrypt issuer and filesystem storage shown above.
Wait for Caddy to obtain and store that cert before changing the sync
job. Keep the existing timer schedule and Postfix target paths.

Install `web-distributor-migrate` on the Incus host used by the sync
job. It needs `incus`, `openssl`, and the same permission to read files
from the web-distributor instance as the old sync. In the current
`cmd_sync_smtp_cert` consumer, replace both legacy `incus file pull`
commands with this call, keeping the existing temp directory and cleanup
trap. Use the deployed mail hostname and storage root.

```sh
/usr/local/bin/web-distributor-migrate pull-smtp-tls \
    --instance web-distributor --hostname mail.example.com \
    --storage /var/lib/caddy/storage --output "${tmp}" \
    || die "could not pull and verify the Caddy-managed SMTP certificate"
```

The helper uses Caddy's
`certificates/acme-v02.api.letsencrypt.org-directory/<hostname>/<hostname>.{crt,key}`
paths inside the selected storage root. It retries transient network or
renewal races, verifies that the cert and key match, and checks the
certificate chain, hostname and dates with the host's OpenSSL trust
store. On success it provides private `fullchain.pem` and `privkey.pem`
files for the existing consumer. Keep the output directory private.
Update the consumer's change check to compare both files with their
destination counterparts. A previous failed key push can leave the
certificate fingerprint unchanged. Reload a running Postfix after
both files are current, including on retry after a failed reload.
Run the sync service once after switching the reads. Verify the remote
files form a pair and that Postfix serves the expected cert over
STARTTLS. Check the next timer run and the first Caddy renewal.

## Rollback

Keep the old configs, binaries, cert state and unit states through the
acceptance window. To roll back, stop Caddy and confirm it has released
the HTTP and HTTPS listeners. Restore the previous web-distributor
configuration and Nginx/ACME definitions if changed. Run `nginx -t`,
restart the previous acme-redirect daemon and Nginx, and restore the
previous renewal timer and enabled states. Revert the SMTP consumer to
its legacy reads before running its sync again. Verify the old mail
certificate is valid and renew it through acme-redirect if needed once
that daemon owns the public HTTP listener. Repeat the baseline checks
for HTTP, HTTPS, authentication, gRPC and SMTP before closing the
rollback window. Retain Caddy's storage for a later retry.
