#!/usr/bin/python3
"""Create and verify a live SQLite backup, keeping the last 14 days."""

import os
import sqlite3
import time
from contextlib import closing
from datetime import datetime, timezone
from pathlib import Path

source = Path("/var/lib/random-frame-sync/sync.db")
directory = Path("/var/backups/random-frame-sync")
destination = directory / f"sync-{datetime.now(timezone.utc):%Y%m%dT%H%M%SZ}.db"
os.umask(0o077)
if destination.exists():
    raise SystemExit(f"Backup already exists: {destination}")

try:
    with closing(sqlite3.connect(f"file:{source}?mode=ro", uri=True)) as database:
        with closing(sqlite3.connect(destination)) as backup:
            database.backup(backup)
            if backup.execute("PRAGMA integrity_check").fetchone() != ("ok",):
                raise RuntimeError("Backup integrity check failed")
except BaseException:
    destination.unlink(missing_ok=True)
    raise

cutoff = time.time() - 14 * 86400
for old in directory.glob("sync-*.db"):
    if old.stat().st_mtime < cutoff:
        old.unlink()
print(destination)
