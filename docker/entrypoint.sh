#!/bin/bash
# Bootstraps the single-node hosted root (`roots.single-node-hosted`) on
# first boot, then serves it two ways behind one nginx: stock git's
# smart-HTTP transport on the *.git paths, and the hosted web UI
# (`git ents serve --hosted`) on everything else.
#
# bash, not sh: `wait -n` below is the whole process supervisor — first
# long-running process to exit takes the machine down nonzero, and Fly
# restarts it. Crude but honest for a single-node root; runit is the
# upgrade path if either process starts crash-looping.
set -eu

repo=/data/repo.git
key=/data/hosted_signing_key
public_host="${PUBLIC_HOST:-git.ents.cloud}"

if [ ! -d "$repo" ]; then
    git init --quiet --bare "$repo"
fi
git -C "$repo" config http.receivepack true
git -C "$repo" config http.uploadpack true

# Idempotent: reuses $key if it already exists (persisted on the /data
# volume across deploys), and always reinstalls hooks pointing at this
# binary's own current path.
git-ents setup --hosted --key "$key" "$repo"

mkdir -p /run
spawn-fcgi -s /run/fcgiwrap.sock -M 766 -- /usr/sbin/fcgiwrap

# The web UI refuses to boot until $key's public half is enrolled as a
# member (`roots.web-signing`). Retry rather than die: an unenrolled key
# on a fresh volume must not take nginx — and with it the git transport
# and the ssh path an operator needs to *do* the enrolling — down in a
# crash loop. The web surface stays fail-closed (nothing listens on 4880
# until enrollment succeeds); the enroll command prints every attempt.
#
# Fresh-volume runbook: enrollment is deliberately NOT automated here.
# Auto-enrolling would spend the self-admitting first push
# (`gate.bootstrap`) on the server's own key, making the machine the
# trust root instead of the operator. From a local clone, one command —
# it enrolls the operator (signed by `user.signingkey`), then vouches
# for the server key, discovered from nginx's /.ents/server-key (its
# private half never leaves this volume):
#   git ents bootstrap <you>
# The retry below then admits the web UI within one 15s cycle.
(
    until git-ents serve --hosted --key "$key" --public-host "$public_host" \
        --port 4880 "$repo"; do
        echo "web UI not started; retrying in 15s" >&2
        sleep 15
    done
) &
nginx -c /etc/git-ents/nginx.conf -g "daemon off;" &

wait -n
code=$?
kill 0 2>/dev/null || true
exit "$code"
