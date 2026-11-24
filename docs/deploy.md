# Deploy a release to cobalt

Run these commands from the repository on an x86_64 Arch Linux workstation with a glibc compatible with cobalt and its `web-distributor` container. The build uses Git, Rust/Cargo, a C linker, Caddy 2.10 or later and curl with HTTP/2 support. Set `CADDY` to the executable path if it is outside your `PATH`. Deployment uses `ssh cobalt` and the host’s local Incus project `default`.

## Build, deploy and check

Pin the commit you want to deploy. Use a fetched release tag in place of `HEAD` to select a tagged version.

```sh
release=$(git rev-parse HEAD)
./ops/release.sh build "$release"
./ops/release.sh deploy "$release"
```

`build` runs the tests, including the live Caddy test, and saves a checksummed bundle in `scratch/releases/<commit>/`. Reuse that bundle for subsequent deploys. `deploy` transfers the CLI, installs it under `/opt/web-distributor/<commit>/`, renders from the current inventory and validates the candidate as the Caddy user. It then switches root’s `web-distributor` command and reloads Caddy if the fragment changed. Keep route, credential and master Caddyfile edits and other reloads out of this change window.

Check a real production route from the workstation. Replace the example URL and status with the expected first response for that route, such as 200 for a public page, 302 for a login redirect or 401 for Basic auth.

```sh
./ops/release.sh check https://app.example.com/ 200
```

The production master has a standalone import of `/etc/web-distributor/v2/Caddyfile`. Validation runs as `caddy` via `runuser`. A master that depends on environment variables supplied only by systemd also needs those variables during validation.

## Roll back

If the public check fails, pass the commit of the deployment being undone. Each scripted deployment records the executable and fragment it replaced, including the first upgrade from the existing installation. Repeat the same public check.

```sh
./ops/release.sh rollback "$release"
./ops/release.sh check https://app.example.com/ 200
```

For another deployment of the same build after rollback, use `./ops/release.sh deploy "$release" --attempt 2` and use the same number on its rollback command. If routes or credentials have changed since deployment, deploy the earlier known-good commit with a fresh attempt instead. This renders from the current inventory.

## Update the SMTP helper on cobalt

When the host’s certificate helper changes, use the same build bundle and run between timer invocations. The script uses sudo, installs `/usr/local/bin/web-distributor-migrate`, runs the sync service, and displays its journal and timer.

```sh
./ops/release.sh deploy-smtp "$release"
```

To restore the previous helper and run the service again:

```sh
./ops/release.sh rollback-smtp "$release"
```
