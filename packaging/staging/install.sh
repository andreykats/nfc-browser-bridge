#!/bin/bash
# Install or update the staging probe on the staging Mac mini (TESTING.md), from this Mac:
#
#   packaging/staging/install.sh user@192.168.1.141
#
# Builds the probe for the Mac mini's CPU, copies it and its LaunchAgent over SSH (key login), and
# (re)starts it. The Mac mini needs the bridge itself installed from the latest release.
set -euo pipefail

host=${1:?usage: install.sh USER@HOST}
root=$(cd "$(dirname "$0")/../.." && pwd)
label=dev.drey.nfc-browser-bridge.staging-probe
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10)

arch=$(ssh "${ssh_opts[@]}" "$host" uname -m) || {
    echo "Cannot log in to $host with an SSH key (see TESTING.md for ssh-copy-id)." >&2
    exit 1
}
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
        echo "Cannot build for $target: install that Rust target (rustup target add $target)." >&2
        exit 1
    }
    bin="$root/target/$target/release/staging-probe"
fi

remote_home=$(ssh "${ssh_opts[@]}" "$host" 'printf %s "$HOME"')
app_dir="$remote_home/Library/Application Support/NFCBrowserBridgeStaging"
logs="$remote_home/Library/Logs/NFCBrowserBridgeStaging"
agent="$remote_home/Library/LaunchAgents/$label.plist"

ssh "${ssh_opts[@]}" "$host" "mkdir -p '$app_dir' '$logs' '$remote_home/Library/LaunchAgents'"
ssh "${ssh_opts[@]}" "$host" "cat > '$app_dir/staging-probe.new'" <"$bin"
sed -e "s|__PROBE__|$app_dir/staging-probe|" -e "s|__LOGS__|$logs|" \
    "$root/packaging/staging/$label.plist" | ssh "${ssh_opts[@]}" "$host" "cat > '$agent'"
ssh "${ssh_opts[@]}" "$host" "
    set -e
    uid=\$(id -u)
    launchctl bootout gui/\$uid/$label 2>/dev/null || true
    mv -f '$app_dir/staging-probe.new' '$app_dir/staging-probe'
    chmod 755 '$app_dir/staging-probe'
    launchctl bootstrap gui/\$uid '$agent'
    sleep 3
    launchctl print gui/\$uid/$label | grep -E '^\s*state ='
    tail -3 \"$logs/probe.\$(date -u +%F).log\"
"
echo "Installed the staging probe on $host."
