#!/usr/bin/env bash
# Build t3d, sign it with a stable keychain identity, install it to ~/.local/bin, and restart
# the LaunchAgent once no run is active (so in-flight turns, including the caller's, finish).
#
# A plain `cp target/release/t3d` leaves an ad-hoc signature whose designated requirement is
# the cdhash, so macOS privacy (TCC) sees every rebuild as a new app and drops its file-access
# grants. Signing with the same identity keeps those grants across rebuilds.
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
dest="$HOME/.local/bin/t3d"
label="com.local.t3code-rs"
log="$HOME/.t3-rs/logs/install-t3d.log"
mkdir -p "$(dirname -- "$dest")" "$(dirname -- "$log")"

cargo build --release --bin t3d

identity="$(/usr/bin/security find-identity -v -p codesigning \
    | awk '/"(Apple Development|Developer ID Application):/ { print $2; exit }')"
if [[ -z "$identity" ]]; then
    echo "warning: no signing identity in the keychain; ad-hoc signing (TCC grants reset on rebuild)" >&2
fi

# Sign a sibling copy and rename it over the old binary, so the running server keeps its inode.
tmp="$dest.new.$$"
trap 'rm -f -- "$tmp"' EXIT
cp target/release/t3d "$tmp"
/usr/bin/codesign --force --sign "${identity:--}" --identifier t3d --timestamp=none "$tmp"
/usr/bin/codesign --verify --strict "$tmp"
mv -f -- "$tmp" "$dest"
rev="$(git rev-parse --short HEAD)"
echo "installed $dest ($rev)"

if ! launchctl print "gui/$(id -u)/$label" > /dev/null 2>&1; then
    echo "LaunchAgent $label is not loaded; restart t3d yourself"
    exit 0
fi

# Detached so the restart survives the caller's own run being stopped by it.
nohup /bin/zsh -c '
    log=$1 dest=$2 target=$3 rev=$4
    print "$(date "+%F %T") $rev: waiting for active runs to finish" >> $log
    for i in {1..720}; do
        $dest monitor 2>/dev/null | grep -q "^Active runs: 0 " && break
        sleep 10
    done
    launchctl kickstart -k $target >> $log 2>&1
    sleep 3
    if $dest status >> $log 2>&1; then
        print "$(date "+%F %T") $rev: restarted OK" >> $log
        osascript -e "display notification \"t3d を $rev で再起動しました\" with title \"t3code-rs\""
    else
        print "$(date "+%F %T") $rev: restart FAILED" >> $log
        osascript -e "display notification \"t3d の再起動に失敗。$log を確認\" with title \"t3code-rs\""
    fi
' _ "$log" "$dest" "gui/$(id -u)/$label" "$rev" > /dev/null 2>&1 &
echo "restart scheduled after active runs finish (log: $log)"
