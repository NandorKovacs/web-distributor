# Deploy a release to cobalt

Run these commands from the repository on an x86_64 Arch Linux workstation with a glibc compatible with cobalt and its `web-distributor` container. The build uses Git, Rust/Cargo, a C linker, Caddy 2.10 or later and curl with HTTP/2 support. Set `CADDY` to the executable path if it is outside your `PATH`. Deployment uses `ssh cobalt` and the host’s local Incus project `default`.

## Build, deploy and check

Each release command defaults to the current Git commit (`HEAD`) when you run it. To select a different version, pass the same fetched tag or commit to both `build` and `deploy`.

```sh
./ops/release.sh build
./ops/release.sh deploy
```

`build` runs the tests, including the live Caddy test, and saves a checksummed bundle in `scratch/releases/<commit>/`. Reuse that bundle for subsequent deploys. `deploy` transfers the CLI, installs it under `/opt/web-distributor/<commit>/`, renders from the current inventory and validates the candidate as the Caddy user. It then switches root’s `web-distributor` command and reloads Caddy if the fragment changed. Keep route, credential and master Caddyfile edits and other reloads out of this change window.

Check a real production route from the workstation. Replace the example URL and status with the expected first response for that route, such as 200 for a public page, 302 for a login redirect or 401 for Basic auth.

```sh
./ops/release.sh check https://app.example.com/ 200
```

The production master has a standalone import of `/etc/web-distributor/v2/Caddyfile`. Validation runs as `caddy` via `runuser`. A master that depends on environment variables supplied only by systemd also needs those variables during validation.

## Roll back

If the public check fails and your checkout is still at the deployed commit, run the commands below. If your checkout has moved, pass the deployed tag or commit to `rollback`. Each scripted deployment records the executable and fragment it replaced, including the first upgrade from the existing installation.

```sh
./ops/release.sh rollback
./ops/release.sh check https://app.example.com/ 200
```

For another deployment of the current commit after rollback, use `./ops/release.sh deploy --attempt 2` and use the same number on its rollback command. If routes or credentials have changed since deployment, deploy the earlier known-good commit with a fresh attempt instead. This renders from the current inventory.

## Update the SMTP helper on cobalt

When the host’s certificate helper changes, use the same build bundle and run between timer invocations. The script uses sudo, installs `/usr/local/bin/web-distributor-migrate`, runs the sync service, and displays its journal and timer.

```sh
./ops/release.sh deploy-smtp
```

To restore the previous helper and run the service again:

```sh
./ops/release.sh rollback-smtp
```
