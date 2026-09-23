# Random Frame Sync server

A single-process Rust/Axum service that stores encrypted envelopes as opaque SQLite BLOBs. It cannot decrypt snapshots or enumerate frame IDs. The client owns encryption, validation, and merge. The server owns bearer authorization and atomic revision checks.

## Run

Requires Rust and a writable directory for SQLite. `sqlx` was chosen for its maintained SQLite support, Tokio integration, and direct SQL queries; no ORM or background worker is needed.

```sh
cargo build --release --locked
RF_SYNC_DB=./sync.db ./target/release/random-frame-sync-server
```

Configuration: `RF_SYNC_BIND` defaults to `127.0.0.1:8787`; `RF_SYNC_DB` defaults to `sync.db`; `RF_SYNC_MAX_PAYLOAD` defaults to `16000068` and accepts `1..=16000068`; `RUST_LOG` defaults to `info`. The service creates the DB file, but its parent directory must exist. Keep the DB directory private: the database and WAL hold the opaque payloads and auth verifiers. The bind is deliberately loopback-only by default.

## API

`sync_id` and bearer token are each exactly 64 lowercase ASCII hex characters. The bearer is hashed as `SHA-256(ASCII(token))` and compared to the stored 32-byte verifier in constant time. Both PUT forms require `Content-Type: application/octet-stream` and `1..=RF_SYNC_MAX_PAYLOAD` body bytes. No envelope parsing occurs.

| Method | Path | Preconditions | Success | Conflict/Auth |
|---|---|---|---|---|
| PUT | `/sync/{sync_id}` | `Authorization: Bearer <token>`, `If-None-Match: *` | `201`, `ETag: "1"`, empty body | Existing ID: `412` |
| GET | `/sync/{sync_id}` | `Authorization: Bearer <token>` | `200`, octet-stream, exact bytes, strong revision ETag | Missing or wrong bearer: identical empty `404` |
| PUT | `/sync/{sync_id}` | Bearer, `If-Match: "<positive decimal revision>"`, octet-stream | `204`, incremented ETag, empty body | Authenticated stale/future revision: `412`; missing or wrong bearer: `404` |
| GET | `/health` | None | `200`, `{"status":"ok"}` after `SELECT 1` | SQLite failure: `500` |

Malformed IDs, tokens, conditional headers, content type, or empty PUT bodies return empty `400`; bodies over the limit return `413`. Internal SQLite errors return empty `500`. The update `If-Match` parser accepts canonical positive signed-64-bit decimal revisions only: no zero, leading zero, weak tags, lists, or wildcard. Missing and unauthorized GET/UPDATE have the same public status and empty body; timing is not equalized. A create attempt on an existing ID returns `412` regardless of bearer, as required by create semantics.

## SQLite and durability

`migrations/001_initial.sql` creates `sync_chains(sync_id TEXT PRIMARY KEY, auth_verifier BLOB CHECK length=32, revision INTEGER CHECK >0, payload BLOB CHECK length>0, created_at INTEGER, updated_at INTEGER)`; timestamps are Unix seconds. On startup, SQLite runs in WAL mode with `synchronous=FULL`, a 5-second busy timeout, and at most 5 pooled connections. There are no foreign keys. `INSERT ... ON CONFLICT(sync_id) DO NOTHING` makes competing creates single-winner. Update first checks the verifier, then uses `UPDATE ... WHERE sync_id = ? AND revision = ? AND revision < i64::MAX RETURNING revision`; this is the atomic CAS. Revision overflow returns `500` without changing data. A locked/unavailable, corrupt, or other internal DB error is classified in logs and returns `500`, never `404`/`412`.

Operational logs contain method, route template, status, latency, and internal DB error kind. They omit headers, full `sync_id`, token, verifier, and payload.

## VPS example

The files in `deploy/` are examples, not an active deployment. The intended path is Internet → nginx HTTPS → `127.0.0.1:8787` → service → SQLite. Install the binary at `/usr/local/bin/random-frame-sync-server`; create a dedicated system user and private directories:

```sh
sudo useradd --system --home /var/lib/random-frame-sync --shell /usr/sbin/nologin rf-sync
sudo install -d -m 0700 -o rf-sync -g rf-sync /var/lib/random-frame-sync
sudo install -d -m 0750 -o root -g rf-sync /etc/random-frame-sync
sudo install -m 0640 -o root -g rf-sync deploy/server.env.example /etc/random-frame-sync/server.env
sudo install -m 0644 deploy/random-frame-sync.service /etc/systemd/system/random-frame-sync.service
sudo systemctl daemon-reload
sudo systemctl enable --now random-frame-sync.service
```

Adapt `deploy/nginx.conf.example` with the actual hostname and certificate paths, place its `limit_req_zone` in nginx `http {}`, and its `server {}` there too. Validate with `nginx -t` before reload. The example listens only on HTTPS, uses a `17m` nginx body cap, 30 requests/minute per IP with burst 10, and disables access logs because URLs contain `sync_id`. The application enforces its own stricter byte limit and auth even without nginx. Clients must use HTTPS; the application has no TLS listener.

Back up live SQLite via its backup API, not a raw copy of `sync.db` while WAL is active. For example, with a private backup destination:

```sh
sudo -u rf-sync sqlite3 /var/lib/random-frame-sync/sync.db ".backup '/var/lib/random-frame-sync/sync-backup.db'"
```

Move the completed backup to protected off-host storage. Local device `SeenStore` data also survives a server failure, but the remote chain should still be backed up.

## Client contract (`reqwest` next stage)

Use an HTTPS base URL. Send the 64-character lowercase `sync_id` as a path segment and the independent 64-character lowercase auth token as `Authorization: Bearer <token>`. Send exact `EncryptedEnvelopeV1` bytes as `application/octet-stream`; never send plaintext or a recovery key. On first upload, `PUT /sync/{sync_id}` with `If-None-Match: *`, expect `201` and `ETag: "1"`. To download, `GET /sync/{sync_id}`, expect exact response bytes and a strong quoted decimal ETag; `404` means unavailable or unauthorized. To update, `PUT` new envelope bytes with the last observed `If-Match: "<revision>"`; `204` returns the next ETag. On `412`, fetch, decrypt and merge locally, then retry CAS. On `400`, fix the request; on `413`, shrink the envelope; on `429` or `5xx`, retry with backoff. Never assume the server validated crypto or merged snapshots.
