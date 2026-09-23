#!/usr/bin/python3
"""Exercise the public opaque Sync API with disposable random credentials."""

import secrets
import sys
import urllib.error
import urllib.request

base = sys.argv[1].rstrip("/")
sync_id = secrets.token_hex(32)
bearer = secrets.token_hex(32)
url = f"{base}/sync/{sync_id}"
original = secrets.token_bytes(67)
updated = secrets.token_bytes(73)


def request(method, token, body=None, condition=None):
    headers = {"Authorization": f"Bearer {token}", "User-Agent": ""}
    if body is not None:
        headers["Content-Type"] = "application/octet-stream"
    if condition is not None:
        headers[condition[0]] = condition[1]
    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=15) as response:
            return response.status, response.headers, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers, error.read()


created = request("PUT", bearer, original, ("If-None-Match", "*"))
if created[0] != 201:
    print(f"create failure: {created[0]} server={created[1].get('Server')} cf-ray={created[1].get('CF-RAY')} body={created[2][:240]!r}")
assert (created[0], created[1].get("ETag")) == (201, '"1"'), created[:2]
got = request("GET", bearer)
assert (got[0], got[1].get("ETag"), got[2]) == (200, '"1"', original)
changed = request("PUT", bearer, updated, ("If-Match", '"1"'))
assert (changed[0], changed[1].get("ETag")) == (204, '"2"'), changed[:2]
stale = request("PUT", bearer, original, ("If-Match", '"1"'))
assert stale[0] == 412, stale[0]
wrong = request("GET", secrets.token_hex(32))
assert wrong[0] == 404, wrong[0]
final = request("GET", bearer)
assert (final[0], final[1].get("ETag"), final[2]) == (200, '"2"', updated)
print(f"sync_id={sync_id} create=201/1 get=200/1 update=204/2 stale=412 wrong=404 final=200/2")
