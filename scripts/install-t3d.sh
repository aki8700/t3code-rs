#!/usr/bin/env bash
# Build t3d, sign it with a stable keychain identity, install it to ~/.local/bin, and restart
# the LaunchAgent once no run is active (so in-flight turns, including the caller's, finish).
#
# A plain `cp target/release/t3d` leaves an ad-hoc signature whose designated requirement is
# the cdhash, so macOS privacy (TCC) sees every rebuild as a new app and drops its file-access
# grants. Signing with the same identity keeps those grants across rebuilds.
#
# If `/usr/bin/codesign` prompts for the login keychain password on every install (Xcode-created
# keys often have only `teamid:59GAB85EFG` in their partition list), authorize codesign once:
#   security set-key-partition-list -S apple-tool:,apple:,codesign:,teamid:59GAB85EFG \
#     -s ~/Library/Keychains/login.keychain-db
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

# Sign a sibling copy and only swap it over the installed binary right before restarting
# the LaunchAgent. Overwriting $dest while the old t3d is still running causes macOS TCC
# to fail code-signature attribution with -67034 ("the code on disk does not match what
# is running").
staged="$dest.staged.$$"
cleanup_staged=1
cleanup() {
    if [[ "$cleanup_staged" -eq 1 ]]; then
        rm -f -- "$staged"
    fi
}
trap cleanup EXIT
cp target/release/t3d "$staged"
/usr/bin/codesign --force --sign "${identity:--}" --identifier t3d --timestamp=none "$staged"
/usr/bin/codesign --verify --strict "$staged"
rev="$(git rev-parse --short HEAD)"

# Supersede any older staged binary waiting for restart.
for old in "$dest".staged.*; do
    [[ "$old" == "$staged" || ! -e "$old" ]] && continue
    rm -f -- "$old"
done

if ! launchctl print "gui/$(id -u)/$label" > /dev/null 2>&1; then
    mv -f -- "$staged" "$dest"
    cleanup_staged=0
    echo "installed $dest ($rev)"
    echo "LaunchAgent $label is not loaded; restart t3d yourself"
    exit 0
fi

# If t3d is not currently serving (or first install), swap immediately and kickstart.
if [[ ! -x "$dest" ]] || ! "$dest" status > /dev/null 2>&1; then
    mv -f -- "$staged" "$dest"
    cleanup_staged=0
    echo "installed $dest ($rev)"
    launchctl kickstart -k "gui/$(id -u)/$label" >> "$log" 2>&1
    echo "restarted $label ($rev)"
    exit 0
fi

# Detach into a new session (setsid) so the waiter survives caller process-group cleanup
# and the LaunchAgent restart itself. Swap the staged binary right before kickstart so
# the running t3d never sees its on-disk executable replaced underneath it.
python3 - "$log" "$dest" "$staged" "gui/$(id -u)/$label" "$rev" <<'PY'
import subprocess
import sys

log, dest, staged, target, rev = sys.argv[1:]
script = r"""
log=$1 dest=$2 staged=$3 target=$4 rev=$5
print "$(date "+%F %T") $rev: waiting for active runs to finish" >> "$log"
idle=0
for i in {1..720}; do
    [[ -f "$staged" ]] || {
        print "$(date "+%F %T") $rev: superseded by a newer install" >> "$log"
        exit 0
    }
    if "$dest" monitor 2>/dev/null | grep -q "^Active runs: 0 "; then
        idle=1
        break
    fi
    sleep 10
done
if [[ "$idle" -ne 1 ]]; then
    print "$(date "+%F %T") $rev: timed out waiting for active runs; keeping staged binary $staged" >> "$log"
    exit 1
fi
[[ -f "$staged" ]] || {
    print "$(date "+%F %T") $rev: superseded by a newer install" >> "$log"
    exit 0
}
mv -f -- "$staged" "$dest" >> "$log" 2>&1
launchctl kickstart -k "$target" >> "$log" 2>&1
healthy=0
for i in {1..15}; do
    sleep 2
    if "$dest" status > /dev/null 2>&1; then
        healthy=1
        break
    fi
done
if [[ "$healthy" -eq 1 ]]; then
    print "$(date "+%F %T") $rev: restarted OK" >> "$log"
    osascript -e "display notification \"t3d を $rev で再起動しました\" with title \"t3code-rs\""
else
    print "$(date "+%F %T") $rev: restart FAILED" >> "$log"
    osascript -e "display notification \"t3d の再起動に失敗。$log を確認\" with title \"t3code-rs\""
fi
"""
subprocess.Popen(
    ["/bin/zsh", "-c", script, "_", log, dest, staged, target, rev],
    start_new_session=True,
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=True,
)
PY
cleanup_staged=0
echo "staged $staged ($rev); swap and restart scheduled after active runs finish (log: $log)"
