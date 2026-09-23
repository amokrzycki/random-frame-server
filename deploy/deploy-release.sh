#!/usr/bin/env bash
set -euo pipefail

root=/opt/random-frame-sync
service=random-frame-sync.service
binary=random-frame-sync-server

fail() { echo "deploy: $*" >&2; exit 2; }
valid_sha() { [[ $1 =~ ^[0-9a-f]{40}$ ]]; }
valid_release() { valid_sha "$1" || [[ $1 == legacy ]]; }

[[ $EUID -eq 0 ]] || fail 'run as root via restricted sudo'
[[ $# -ge 1 ]] || fail 'usage: deploy SHA EXPECTED_CURRENT | rollback EXPECTED_SHA TARGET | current'
case $1 in
    deploy) [[ $# -eq 3 ]] && valid_sha "$2" && valid_release "$3" || fail 'expected full lowercase git SHA and current release ID' ;;
    rollback) [[ $# -eq 3 ]] && valid_sha "$2" && valid_release "$3" || fail 'expected current SHA and rollback target' ;;
    current) [[ $# -eq 1 ]] || fail 'current takes no arguments' ;;
    *) fail 'unknown command' ;;
esac

exec 9>/run/lock/random-frame-sync-deploy.lock
flock -n 9 || { echo 'deploy: another deployment is running' >&2; exit 3; }

current_id() {
    local target
    [[ -L $root/current ]] || fail 'current symlink is missing'
    target=$(readlink "$root/current")
    [[ $target == releases/* ]] || fail 'invalid current symlink'
    local id=${target#releases/}
    valid_release "$id" || fail 'invalid current release ID'
    [[ -f $root/releases/$id/$binary ]] || fail 'current release is incomplete'
    printf '%s' "$id"
}

health() {
    local expected=$1 body
    for ((attempt=0; attempt<20; attempt++)); do
        if body=$(curl --fail --silent --max-time 2 http://127.0.0.1:8787/health) &&
            python3 -c 'import json,sys; h=json.load(sys.stdin); sys.exit(0 if h.get("status") == "ok" and (sys.argv[1] == "legacy" or h.get("git_sha") == sys.argv[1]) else 1)' "$expected" <<<"$body" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    return 1
}

switch_to() {
    local id=$1 link=$root/.current-next
    rm -f "$link"
    ln -s "releases/$id" "$link"
    mv -Tf "$link" "$root/current"
}

previous=$(current_id)
if [[ $1 == current ]]; then
    echo "$previous"
    exit 0
fi
if [[ $1 == deploy && $previous != "$3" ]]; then
    fail 'current release changed; refusing deploy'
fi

if [[ $1 == deploy ]]; then
    id=$2
    staged=$root/staging/$id
    release=$root/releases/$id
    incoming=$root/releases/.incoming-$id
    if [[ $previous == "$id" ]]; then
        health "$id" || fail 'active release is unhealthy'
        echo "previous=$previous"
        echo "deploy: already active=$id"
        exit 0
    fi
    if [[ -e $release || -L $release ]]; then
        [[ -d $release && ! -L $release && -f $release/$binary && ! -L $release/$binary ]] || fail 'existing release is incomplete'
    else
        [[ ! -e $incoming ]] || fail 'incomplete temporary release exists'
        [[ -f $staged && ! -L $staged ]] || fail 'staged binary missing or not a regular file'
        [[ $(stat -c %U "$staged") == rf-deploy ]] || fail 'staged binary has wrong owner'
        install -d -o root -g root -m 0755 "$incoming"
        trap 'if [[ -d $incoming ]]; then rm -f "$incoming/$binary"; rmdir "$incoming"; fi' EXIT
        # -P preserves a raced symlink, so it cannot cause root to copy its target.
        cp -P -- "$staged" "$incoming/$binary"
        [[ -f $incoming/$binary && ! -L $incoming/$binary ]] || fail 'staged binary changed during copy'
        chown root:root "$incoming/$binary"
        chmod 0755 "$incoming/$binary"
        mv -T "$incoming" "$release"
        trap - EXIT
    fi
else
    id=$3
    [[ $previous == "$2" ]] || fail 'current release changed; refusing rollback'
    [[ -f $root/releases/$id/$binary && ! -L $root/releases/$id/$binary ]] || fail 'rollback target is incomplete'
fi

switch_to "$id"
if systemctl restart "$service" && health "$id"; then
    if [[ $1 == deploy ]]; then
        echo "previous=$previous"
        echo "deploy: active=$id"
    else
        echo "deploy: rollback active=$id"
    fi
    exit 0
fi

echo "deploy: health failed for $id; restoring $previous" >&2
switch_to "$previous"
if systemctl restart "$service" && health "$previous"; then
    echo "deploy: restored $previous" >&2
    exit 20
fi
echo "deploy: rollback health failed; manual intervention required" >&2
exit 21
