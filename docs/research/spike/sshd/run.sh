#!/usr/bin/env bash
# Throwaway sshd for the russh spike. Listens on 127.0.0.1:22200.
# Cleans up the previous instance, then prints the pid.
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
PORT="${1:-22200}"

[ -f "$DIR/sshd.pid" ] && kill "$(cat "$DIR/sshd.pid")" 2>/dev/null || true
sleep 0.3

[ -f "$DIR/host_ed25519" ] || ssh-keygen -q -t ed25519 -f "$DIR/host_ed25519" -N '' -C spike-host
[ -f "$DIR/user_ed25519" ] || ssh-keygen -q -t ed25519 -f "$DIR/user_ed25519" -N '' -C spike-user
cp "$DIR/user_ed25519.pub" "$DIR/authorized_keys"
chmod 600 "$DIR/authorized_keys"

# sshd must run as root to switch users; if not root we can only serve our own
# account, so pin AllowUsers to the current user.
ME="$(id -un)"
sed -e "s|@DIR@|$DIR|g" -e "s|@PORT@|$PORT|g" -e "s|@USER@|$ME|g" \
    "$DIR/sshd_config.tmpl" > "$DIR/sshd_config"

mkdir -p /run/sshd 2>/dev/null || true
/usr/sbin/sshd -f "$DIR/sshd_config" -E "$DIR/sshd.log"
sleep 1
echo "sshd pid $(cat "$DIR/sshd.pid") on 127.0.0.1:$PORT (serving user $ME)"
