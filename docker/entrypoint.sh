#!/bin/sh
# Bootstraps the single-node hosted root (`roots.single-node-hosted`) on
# first boot, then serves it over stock git's smart-HTTP transport.
set -eu

repo=/data/repo.git
key=/data/hosted_signing_key

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
exec nginx -c /etc/git-ents/nginx.conf -g "daemon off;"
