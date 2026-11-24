# Update the web-distributor deployment

The x86_64 Arch Linux guest `web-distributor` runs on the Incus host `cobalt`.
Root's command is `/usr/bin/web-distributor`, currently a symlink to
`/opt/web-distributor/2.0.0/bin/web-distributor`. Caddy's Arch service reads
`/etc/caddy/Caddyfile`, which imports `/etc/web-distributor/v2/Caddyfile`.
Caddy manages the publicly trusted certificates in its persistent storage.
This procedure updates the CLI and, when its output changes, the imported
fragment.

## Build and transfer from the workstation

Start in the repository in Bash on an x86_64 Arch build machine compatible
with the guest's glibc. Use Rust/Cargo, a C linker, Caddy 2.10 or newer and
curl with HTTP/2 support. Set `REF` to a fetched tag or commit, or leave it
unset to build the selected checkout's HEAD. The detached worktree builds
the exact commit even if the original checkout has local edits.

```bash
set -e
commit=$(git rev-parse --verify "${REF:-HEAD}^{commit}")
release=$commit
build=$(mktemp -d)
bundle=$build/bundle
mkdir "$bundle"
git worktree add --detach "$build/source" "$commit"
cd "$build/source"
export CARGO_TARGET_DIR="$build/target"
cargo test --locked
CADDY="$(command -v caddy)" cargo test --locked --test caddy_runtime -- --ignored
cargo build --locked --release --bins
install -m 0755 "$CARGO_TARGET_DIR/release/"{web-distributor,web-distributor-migrate} "$bundle/"
cd "$bundle"
git -C "$build/source" worktree remove "$build/source"
printf '%s\n' "$release" > RELEASE
./web-distributor --version
sha256sum web-distributor > cli.sha256
sha256sum web-distributor-migrate > helper.sha256
upload=$(ssh cobalt 'mktemp -d /tmp/web-distributor.XXXXXXXX')
scp RELEASE *.sha256 web-distributor web-distributor-migrate "cobalt:$upload/"
ssh -t cobalt "cd '$upload' && exec bash"
```

Each release directory is immutable. The full Git commit distinguishes
builds with the same Cargo version. If an ID already exists, compare its
installed checksum to the manifest. For a distinct rebuild, or to retry a deploy
that stopped after creating the release or staging directories, repeat the
procedure with `release=$commit-2` (or the next unused suffix) in the build
example. Keep the manifests with the release record. Repository credentials
stay on the workstation.

## Stage on cobalt

Run these commands in the host shell opened above, with permission to
manage the default Incus project. The guest receives only the CLI and its
checksum.

```bash
set -e
sha256sum -c cli.sha256 helper.sha256
release=$(cat RELEASE)
guest=web-distributor
incoming=$(incus --force-local --project default exec "$guest" -- mktemp -d /root/web-distributor.XXXXXXXX)
incus --force-local --project default file push web-distributor cli.sha256 "$guest$incoming/"
incus --force-local --project default exec "$guest" --env "release=$release" --env "incoming=$incoming" -- bash
```

## Validate and activate in the guest

Use a window free of other CLI invocations, route or credential edits,
master edits, and Caddy reloads until the checks or rollback are complete.
Start with an active service and the running configuration on disk. Keep
the previous release, fragment, and CLI target through acceptance.

```bash
set -e
cd "$incoming"
sha256sum -c cli.sha256
systemctl is-active --quiet caddy
v2=/etc/web-distributor/v2
live=/usr/bin/web-distributor
master=/etc/caddy/Caddyfile
root=/opt/web-distributor/$release
bin=$root/bin/web-distributor
test -L "$live"
previous=$(readlink -e "$live")
test -x "$previous"
test -s "$v2/config.toml"
mkdir -m 0755 "$root"
install -d -m 0755 "$root/bin"
install -o root -g root -m 0755 web-distributor "$bin"
(cd "$root/bin" && sha256sum -c "$incoming/cli.sha256")
"$bin" --version
```

The v2 directory is root-owned and setgid to the service's `caddy` group.
The generated fragment contains login hashes and should be `root:caddy`
mode `0640`. The candidate reads the current TOML and login groups. Stage
the test master beside the live master so other relative imports still
resolve. The substitution requires exactly one standalone import of the
production fragment.

```bash
install -d -o root -g caddy -m 2750 "$v2"
work=$v2/.deploy-$release
mkdir -m 2750 "$work"
printf '%s\n' "$previous" > "$work/previous-target"
cp -a "$v2/Caddyfile" "$work/previous-fragment"
"$bin" -c "$v2/config.toml" --output "$work/candidate" generate
test "$(stat -c '%U:%G:%a' "$work/candidate")" = root:caddy:640
check=$(mktemp /etc/caddy/.web-distributor.XXXXXXXX)
awk -v file="$work/candidate" '
  $1 == "import" && $2 == "/etc/web-distributor/v2/Caddyfile" && NF == 2 { $2=file; count++ }
  { print }
  END { if (count != 1) exit 1 }
' "$master" > "$check"
chown root:caddy "$check"
chmod 0640 "$check"
runuser -u caddy -- caddy validate --config "$check" --adapter caddyfile
rm "$check"
```

Use the service's environment for validation if the master depends on
variables from its unit or drop-ins (`systemctl cat caddy`). Stop on any
error. Once validation passes, the following same-filesystem renames
promote the executable link and the exact fragment just validated.

```bash
ln -s "$bin" "$live.next-$release"
mv -Tf "$live.next-$release" "$live"
if ! cmp -s "$work/candidate" "$v2/Caddyfile"; then
    mv -Tf "$work/candidate" "$v2/Caddyfile"
    systemctl reload caddy
fi
test "$(readlink -e "$live")" = "$bin"
"$live" --version
"$live" list
systemctl is-active caddy
journalctl -u caddy -n 25 --no-pager
```

From outside the guest, request a known route with its public hostname
and normal certificate verification, for example `curl -sS -o /dev/null
-w '%{http_code}\n' https://app.example.com/` with the actual hostname.
Compare its response with the expected application or authentication
status. A reload error or failed acceptance check calls for rollback.

## Roll back the guest

Run as root in the guest. Set `release` to the ID of the update being
undone if using a fresh shell. From the upload directory on cobalt,
`incus --force-local --project default exec web-distributor --env "release=$(cat RELEASE)" -- bash`
opens one with the release ID set. These commands restore the saved target
and exact prior fragment, including its ownership and mode, even after a
partial activation. Roll back during the same change window so the saved
fragment still corresponds to the current routes and credentials.

```bash
set -e
v2=/etc/web-distributor/v2
work=$v2/.deploy-${release:?set release to the failed release ID}
live=/usr/bin/web-distributor
previous=$(cat "$work/previous-target")
test -x "$previous"
ln -s "$previous" "$live.rollback-$release"
mv -Tf "$live.rollback-$release" "$live"
cp -a "$work/previous-fragment" "$work/restore"
mv -Tf "$work/restore" "$v2/Caddyfile"
runuser -u caddy -- caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
systemctl reload caddy
systemctl is-active caddy
readlink -e "$live"
```

Repeat the public route check. If route data changed after acceptance,
generate and validate a fresh candidate from that data using the retained
older executable before switching back.

## Optional SMTP helper update on cobalt

On cobalt, `sync-smtp-cert.timer` invokes a service that uses
`/usr/local/bin/web-distributor-migrate`. Update it separately when the
helper changes. Exit the guest to return to the uploaded bundle on
`cobalt`. Run between timer invocations and record the printed backup path.

```bash
set -e
sha256sum -c helper.sha256
./web-distributor-migrate sync-smtp-tls --help
helper=/usr/local/bin/web-distributor-migrate
backup=$(sudo mktemp -d /usr/local/bin/.smtp-helper.XXXXXXXX)
printf '%s\n' "$backup"
sudo cp -a "$helper" "$backup/previous"
sudo install -o root -g root -m 0755 web-distributor-migrate "$backup/new"
sudo mv -Tf "$backup/new" "$helper"
sudo systemctl start sync-smtp-cert.service
sudo journalctl -u sync-smtp-cert.service -n 20 --no-pager
systemctl list-timers sync-smtp-cert.timer
```

For host rollback, restore the saved executable at the same path with
`sudo mv -Tf "$backup/previous" /usr/local/bin/web-distributor-migrate`,
then run and check the sync service again.
