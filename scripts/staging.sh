#!/bin/bash
# The on-demand long-running test on the staging Mac mini (TESTING.md), from a developer Mac:
#
#   scripts/staging.sh install    build the probe and copy it over (restarts a run in progress)
#   scripts/staging.sh start      start a run: the probe reads the tag every minute until stopped
#   scripts/staging.sh status     is a run on, and its latest entries
#   scripts/staging.sh stop       stop the run, upload its summary and logs, print the summary
#   scripts/staging.sh results    the latest run's summary from the logs bucket (needs gcloud)
#
# STAGING_HOST picks the Mac mini (default user@Mac-mini.local); SSH logs in with a key.
set -euo pipefail

host=${STAGING_HOST:-user@Mac-mini.local}
machine=Mac-mini-F6E28B59
label=dev.drey.nfc-browser-bridge.staging-probe
root=$(cd "$(dirname "$0")/.." && pwd)
# -4: Bonjour names also resolve to IPv6 link-local addresses, which ssh may fail to reach.
ssh_opts=(-4 -o BatchMode=yes -o ConnectTimeout=10)
app='$HOME/Library/Application Support/NFCBrowserBridgeStaging'
logs='$HOME/Library/Logs/NFCBrowserBridgeStaging'
agent="\$HOME/Library/LaunchAgents/$label.plist"

remote() {
    ssh "${ssh_opts[@]}" "$host" "$@" || {
        local status=$?
        [ $status -eq 255 ] && echo "Cannot log in to $host with an SSH key (TESTING.md)." >&2
        return $status
    }
}

running() {
    remote "launchctl print gui/\$(id -u)/$label >/dev/null 2>&1"
}

install() {
    local arch target bin
    arch=$(remote uname -m)
    case "$arch" in
        arm64) target=aarch64-apple-darwin ;;
        x86_64) target=x86_64-apple-darwin ;;
        *) echo "Unexpected CPU on $host: $arch" >&2; exit 1 ;;
    esac
    if [ "$(rustc -vV | sed -n 's/^host: //p')" = "$target" ]; then
        (cd "$root" && cargo build --release -p bridge-app --bin staging-probe)
        bin="$root/target/release/staging-probe"
    else
        (cd "$root" && cargo build --release -p bridge-app --bin staging-probe --target "$target") || {
            echo "Cannot build for $target: install that Rust target." >&2
            exit 1
        }
        bin="$root/target/$target/release/staging-probe"
    fi
    remote "mkdir -p \"$app\" \"$logs\" && cat > \"$app/staging-probe.new\"" <"$bin"
    remote "sed -e \"s|__PROBE__|$app/staging-probe|\" -e \"s|__LOGS__|$logs|\" > \"$app/$label.plist\"" \
        <"$root/packaging/staging/$label.plist"
    local was_running=no
    if running; then
        was_running=yes
        remote "launchctl bootout gui/\$(id -u)/$label"
    fi
    remote "mv -f \"$app/staging-probe.new\" \"$app/staging-probe\" && chmod 755 \"$app/staging-probe\""
    if [ $was_running = yes ]; then
        remote "launchctl bootstrap gui/\$(id -u) \"$agent\""
        echo "Installed the probe on $host and restarted the run in progress."
    else
        echo "Installed the probe on $host. Start a run with: scripts/staging.sh start"
    fi
}

start() {
    if running; then
        echo "A run is already on. See: scripts/staging.sh status" >&2
        exit 1
    fi
    remote "test -x \"$app/staging-probe\"" || {
        echo "The probe isn't installed on $host. Run: scripts/staging.sh install" >&2
        exit 1
    }
    if remote "test -s \"$logs/run.log\""; then
        echo "An earlier run wasn't finished; finishing it first."
        finish
    fi
    remote "mkdir -p \"\$HOME/Library/LaunchAgents\" && cp \"$app/$label.plist\" \"$agent\" &&
        launchctl bootstrap gui/\$(id -u) \"$agent\""
    sleep 5
    echo "Run started on $host. It keeps going (across restarts) until: scripts/staging.sh stop"
    remote "tail -3 \"$logs/run.log\""
}

status() {
    if running; then
        echo "A run is on."
        remote "printf 'reads ok: %s, failed: %s\n' \$(grep -c ' ok ' \"$logs/run.log\") \$(grep ' fail ' \"$logs/run.log\" | grep -vc ' fail disconnected'); tail -5 \"$logs/run.log\""
    else
        echo "No run is on."
    fi
}

finish() {
    remote "\"$app/staging-probe\" finish"
}

stop() {
    if running; then
        remote "launchctl bootout gui/\$(id -u)/$label; rm -f \"$agent\""
    else
        echo "No run is on."
        remote "rm -f \"$agent\""
        remote "test -s \"$logs/run.log\"" || exit 0
        echo "Finishing the last run's log."
    fi
    finish
}

results() {
    local bucket=gs://logs-wvmye.firebasestorage.app/nfc-browser-bridge/$machine latest
    latest=$(gcloud storage ls "$bucket/*/summary.*.txt" 2>/dev/null | sort -t/ -k6 | tail -1) || true
    if [ -z "$latest" ]; then
        echo "No staging summary in $bucket." >&2
        exit 2
    fi
    echo "$latest"
    gcloud storage cat "$latest"
}

case "${1:-}" in
    install | start | status | stop | results) "$1" ;;
    *) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
