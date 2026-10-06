#!/bin/sh
# Atlas container entrypoint.
#
# 1. First start: seed $ATLAS_HOME/config.json from a config mounted at
#    /app/config.json. After that atlas owns the copy in the data volume
#    (it saves groups, the api key, settings there), soo later edits to the
#    mounted file are not picked up. Delete the copy to re-seed.
#
# 2. PUID / PGID / UMASK work like the linuxserver.io images: the data folder
#    is handed to that user and atlas runs as it, soo files in bind mounts
#    (atlas-data, and the sabnzbd-config shared with the sabnzbd container)
#    end up owned by the same user on the host. Unset (or 0) = run as root.
set -e

: "${ATLAS_HOME:=/app/data}"
mkdir -p "$ATLAS_HOME"

# docker bind mounts a missing host file as an empty directory
if [ ! -f "$ATLAS_HOME/config.json" ] && [ -d /app/config.json ]; then
    echo "warning: /app/config.json is a directory, the config file to seed from is missing on the host." >&2
    echo "         remove the empty docker/config.json folder docker made, then:" >&2
    echo "         cp docker/config.example.json docker/config.json   (and fill in the logins)" >&2
fi

if [ ! -f "$ATLAS_HOME/config.json" ] && [ -f /app/config.json ]; then
    cp /app/config.json "$ATLAS_HOME/config.json"
    chmod 600 "$ATLAS_HOME/config.json"
    echo "seeded $ATLAS_HOME/config.json from /app/config.json"
fi

if [ -n "${UMASK:-}" ]; then
    umask "$UMASK"
fi

# already started as a non root user (compose `user:`), nothing to switch
if [ "$(id -u)" != "0" ] || { [ -z "${PUID:-}" ] && [ -z "${PGID:-}" ]; }; then
    exec atlas "$@"
fi

PUID="${PUID:-1000}"
PGID="${PGID:-$PUID}"

case "$PUID$PGID" in
    *[!0-9]*) echo "PUID and PGID must be numbers (got PUID=$PUID PGID=$PGID)" >&2; exit 1 ;;
esac

if [ "$PUID" = "0" ]; then
    exec atlas "$@"
fi

# hand the data folder to that user, only touching what isnt theirs yet
find "$ATLAS_HOME" \( ! -user "$PUID" -o ! -group "$PGID" \) -exec chown "$PUID:$PGID" {} +

# sabnzbd's config is mounted at $HOME/.sabnzbd (/root/.sabnzbd), let the
# user get through /root to it. the folder itself belongs to the sabnzbd
# container's user and is left alone
if [ -d "$HOME" ]; then
    chmod o+x "$HOME"
fi

echo "running atlas as $PUID:$PGID"
exec setpriv --reuid="$PUID" --regid="$PGID" --clear-groups atlas "$@"
