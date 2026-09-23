CREATE TABLE IF NOT EXISTS sync_chains (
    sync_id TEXT PRIMARY KEY NOT NULL,
    auth_verifier BLOB NOT NULL CHECK (length(auth_verifier) = 32),
    revision INTEGER NOT NULL CHECK (revision > 0),
    payload BLOB NOT NULL CHECK (length(payload) > 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
