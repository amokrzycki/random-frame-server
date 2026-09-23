# Production application deployment

This is a repository-side plan. The VPS and GitHub Environment still need configuration; this workflow has not deployed production. Application releases do not install nginx, Certbot, environment files, SQLite data, or backups.

## Initial VPS bootstrap

The existing service runs as `rf-sync`. Keep `/etc/random-frame-sync/server.env` (`root:rf-sync`, `0640`), `/var/lib/random-frame-sync/sync.db` (in a private `rf-sync` directory), and `/var/backups/random-frame-sync/` in place. Do not copy an active SQLite file. The backup timer continues to use `backup.py` and the SQLite backup API.

On the VPS, perform and review these commands as an administrator. The examples assume the existing `/opt/random-frame-sync/random-frame-sync-server` is the currently working binary. Leave that file untouched until the new service path works.

```sh
sudo useradd --create-home --shell /bin/bash rf-deploy
sudo install -d -o rf-deploy -g rf-deploy -m 0700 /home/rf-deploy/.ssh
sudo install -o rf-deploy -g rf-deploy -m 0600 /path/to/approved/deploy-public-key.pub /home/rf-deploy/.ssh/authorized_keys
sudo install -d -o root -g root -m 0755 /opt/random-frame-sync
sudo install -d -o root -g root -m 0755 /opt/random-frame-sync/releases
sudo install -d -o rf-deploy -g rf-deploy -m 0700 /opt/random-frame-sync/staging
sudo install -d -o root -g root -m 0755 /opt/random-frame-sync/releases/legacy
sudo install -o root -g root -m 0755 /opt/random-frame-sync/random-frame-sync-server /opt/random-frame-sync/releases/legacy/random-frame-sync-server
sudo ln -s releases/legacy /opt/random-frame-sync/.current-bootstrap
sudo mv -Tf /opt/random-frame-sync/.current-bootstrap /opt/random-frame-sync/current
sudo install -o root -g root -m 0755 deploy-release.sh /usr/local/sbin/random-frame-sync-deploy
sudo install -o root -g root -m 0644 random-frame-sync.service /etc/systemd/system/random-frame-sync.service
sudo systemctl daemon-reload
sudo systemctl restart random-frame-sync.service
curl --fail --silent http://127.0.0.1:8787/health
```

The paths `deploy-release.sh` and `random-frame-sync.service` above mean reviewed copies of the corresponding files from this repository. The private SSH key stays outside the repo and VPS. Record and independently verify the VPS SSH host key before storing its known_hosts line in GitHub; do not derive trust from an unverified `ssh-keyscan` result. SSH uses the VPS IP or SSH hostname, not the Cloudflare-proxied HTTPS hostname.

Install this single-command sudoers rule using `visudo -f /etc/sudoers.d/random-frame-sync-deploy`, then validate it with `visudo -cf /etc/sudoers.d/random-frame-sync-deploy`:

```sudoers
rf-deploy ALL=(root) NOPASSWD: /usr/local/sbin/random-frame-sync-deploy *
```

The script is root-owned and validates its subcommands and full 40-character lowercase SHA arguments. `rf-deploy` can write only staging and invoke this script through sudo; releases and `current` are root-owned. It cannot run `sudo systemctl` directly. The service continues running as `rf-sync` with loopback bind, `UMask=0077`, `NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict`, `ProtectHome`, and write access limited to `/var/lib/random-frame-sync`. Keep port 8787 private. Do not replace the live Certbot-modified nginx config with the bootstrap config in this repo.

Create GitHub Environment `production`. Configure its protection rules and protect `sync-server-v*` tag creation according to who may deploy. The workflow reads these environment values:

| Kind | Name | Format |
| --- | --- | --- |
| Variable | `SYNC_DEPLOY_HOST` | VPS SSH IP or DNS name, no scheme, e.g. `203.0.113.10` |
| Variable | `SYNC_DEPLOY_USER` | `rf-deploy` |
| Variable | `SYNC_PUBLIC_BASE_URL` | `https://server-random-frame.amokrzycki.ovh` |
| Secret | `SYNC_DEPLOY_SSH_KEY` | Complete OpenSSH private key, including BEGIN/END lines; no example key committed |
| Secret | `SYNC_DEPLOY_KNOWN_HOSTS` | Verified OpenSSH known_hosts line for the exact SSH host, e.g. `203.0.113.10 ssh-ed25519 AAAA...` |

## Normal deployment

The workflow runs on a pushed tag matching `sync-server-v*` (and requires a `sync-server-vMAJOR.MINOR.PATCH` name equal to `Cargo.toml`'s package version), or through `workflow_dispatch` on the repository default branch. `Cargo.toml` is currently `0.1.0`, so the first version tag would be `sync-server-v0.1.0`. Use a new tag for each release. The production Environment can require approval. Workflow concurrency allows only one production run at a time; the VPS script also takes a nonblocking `flock`. A newer queued workflow may supersede an older queued workflow, while a running workflow is never canceled automatically.

The GitHub runner installs Rust 1.98.1, checks formatting, Clippy, and tests, builds `cargo build --release --locked`, and embeds the checkout's full git SHA in the binary. It uploads only that binary over host-key-verified SSH to `/opt/random-frame-sync/staging/<sha>`; it never copies the repo, DB, or configuration. The root script contract is:

```text
random-frame-sync-deploy current
random-frame-sync-deploy deploy <40-lowercase-hex-sha> <expected-current-id>
random-frame-sync-deploy rollback <expected-current-sha> <target-id>
```

`current` prints `legacy` or a SHA. `target-id` may be `legacy` or an existing SHA. `deploy` refuses to proceed if `current` changed since the workflow read it. It copies staging into a root-owned `/opt/random-frame-sync/releases/<sha>/random-frame-sync-server`, then atomically changes `/opt/random-frame-sync/current` to `releases/<sha>`. An existing release is reused without overwriting its binary; dispatching an already active SHA only checks its health. Staging is `rf-deploy`-owned and private; release directories and the executable are `root:root` with `0755`. The unit executes `/opt/random-frame-sync/current/random-frame-sync-server`. The script retries local `http://127.0.0.1:8787/health` and checks both `status=ok` and the expected SHA. The workflow then checks the same fields over the public HTTPS endpoint, with normal TLS verification. `git_sha=unknown` is used for builds outside CI unless `RF_SYNC_GIT_SHA` was set at compile time.

Exit codes: `0` success, `2` invalid input or precondition, `3` deployment lock busy, `20` new release failed but previous release was restored and healthy, `21` rollback health failed. An unexpected OS command error can return its native nonzero code. Release directories are not automatically pruned; after several successful deployments, an administrator may remove old inactive releases manually while retaining several rollback candidates. Never remove `current` or a rollback candidate during a deployment.

## Rollback / diagnostics

If the new process or local SHA check fails, the VPS script restores the old symlink, restarts systemd, checks its health, and returns failure. If the public HTTPS status/SHA check fails after local success, the workflow requests `rollback <new-sha> <previous-id>`; the script refuses this if another deployment changed `current`. The GitHub run remains failed even if rollback succeeds. If a runner is forcibly canceled or loses SSH at the wrong moment, inspect state manually.

For an emergency manual binary rollback, connect as an administrator, inspect existing release IDs, then run:

```sh
sudo /usr/local/sbin/random-frame-sync-deploy current
sudo ls -la /opt/random-frame-sync/releases/
sudo /usr/local/sbin/random-frame-sync-deploy rollback <current-sha> <previous-sha-or-legacy>
curl --fail --silent http://127.0.0.1:8787/health
curl --fail --silent https://server-random-frame.amokrzycki.ovh/health
sudo journalctl -u random-frame-sync.service -n 100 --no-pager
```

Compare `git_sha` with the expected full commit SHA in both health responses. The legacy binary predates this metadata and is checked by `status` only. The initial SQL file is idempotent (`CREATE TABLE IF NOT EXISTS`); this deployment does not change or restore SQLite. Future schema changes must preserve compatibility with the previous binary before using automatic binary rollback. If a release introduces a backward-incompatible migration, treat it as a separate migration plan with its own recovery procedure. SQLite backups remain independent of application releases.
