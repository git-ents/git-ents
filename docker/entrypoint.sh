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
# member (`roots.web-signing`) — on a fresh volume, enroll it before the
# first deploy that ships this entrypoint, or the machine crash-loops
# with the exact command to run in its logs.
git-ents serve --hosted --key "$key" --public-host "$public_host" --port 4880 "$repo" &
nginx -c /etc/git-ents/nginx.conf -g "daemon off;" &

wait -n
code=$?
kill 0 2>/dev/null || true
exit "$code"
