#!/bin/sh
# Prepare an Apple silicon Mac as a copper node.
#
#   curl -fsSL https://raw.githubusercontent.com/Shreyas220/copper/main/dist/macos/prepare.sh | sh -s -- --kiln http://KILN:8400
#
# Run it as the user that will stay logged in (Virtualization.framework needs a
# user session). It asks for your password for the few steps that need sudo.
# Safe to re-run; every step is idempotent.
#
# Options:
#   --kiln URL         kiln base URL (default http://kiln:8400, needs MagicDNS)
#   --authorize-key K  add an SSH public key for remote administration
#   --no-image         skip the Ubuntu arm64 cloud image download
#   --no-ingot         prepare the machine but don't install/run ingot
set -eu

KILN="http://kiln:8400"
KEY=""
IMAGE=1
INGOT=1
REPO="Shreyas220/copper"
STATE="$HOME/.copper"
IMAGE_URL="https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img"

while [ $# -gt 0 ]; do
  case "$1" in
    --kiln) KILN="$2"; shift 2 ;;
    --authorize-key) KEY="$2"; shift 2 ;;
    --no-image) IMAGE=0; shift ;;
    --no-ingot) INGOT=0; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }

[ "$(uname -s)" = Darwin ] || die "this script is for macOS"
[ "$(uname -m)" = arm64 ] || die "copper on macOS needs Apple silicon"
[ "$(id -u)" != 0 ] || die "run as your normal user, not root"
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"

# ---- remote access -----------------------------------------------------------
say "enabling Remote Login (sshd)"
sudo systemsetup -setremotelogin on >/dev/null
if [ -n "$KEY" ]; then
  mkdir -p "$HOME/.ssh"; chmod 700 "$HOME/.ssh"
  touch "$HOME/.ssh/authorized_keys"; chmod 600 "$HOME/.ssh/authorized_keys"
  grep -qF "$KEY" "$HOME/.ssh/authorized_keys" || echo "$KEY" >> "$HOME/.ssh/authorized_keys"
  say "authorized SSH key"
fi

# ---- stay awake, come back after power loss ---------------------------------
say "disabling sleep"
sudo pmset -a sleep 0 disksleep 0 autorestart 1 womp 1 >/dev/null
sudo pmset -a displaysleep 10 >/dev/null

# ---- toolchain ---------------------------------------------------------------
if ! xcode-select -p >/dev/null 2>&1; then
  say "installing Xcode command line tools (a dialog opens; re-run this script when it finishes)"
  xcode-select --install
  exit 0
fi
if ! command -v brew >/dev/null; then
  say "installing Homebrew"
  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
fi
say "installing tart and qemu"
brew list cirruslabs/cli/tart >/dev/null 2>&1 || brew install cirruslabs/cli/tart
brew list qemu >/dev/null 2>&1 || brew install qemu
if ! command -v cargo >/dev/null; then
  say "installing Rust"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
fi

# ---- state and images --------------------------------------------------------
mkdir -p "$STATE/images" "$STATE/bin"
if [ "$IMAGE" = 1 ] && [ ! -f "$STATE/images/ubuntu-24.04.img" ]; then
  say "downloading Ubuntu 24.04 arm64 cloud image"
  curl -fL --progress-bar -o "$STATE/images/ubuntu-24.04.img.part" "$IMAGE_URL"
  mv "$STATE/images/ubuntu-24.04.img.part" "$STATE/images/ubuntu-24.04.img"
fi

[ "$INGOT" = 1 ] || { say "machine prepared; ingot not installed (--no-ingot)"; exit 0; }

# ---- ingot -------------------------------------------------------------------
say "installing ingot"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
url="https://github.com/$REPO/releases/latest/download/copper-aarch64-darwin.tar.gz"
if curl -fsSL "$url" -o "$tmp/copper.tar.gz" 2>/dev/null; then
  tar -xzf "$tmp/copper.tar.gz" -C "$tmp"
  cp "$tmp/ingot" "$tmp/cu" "$STATE/bin/"
  say "installed release binaries"
else
  say "no macOS release found; building from source"
  if [ -d "$STATE/src/.git" ]; then git -C "$STATE/src" pull -q
  else git clone -q "https://github.com/$REPO" "$STATE/src"; fi
  (cd "$STATE/src" && cargo build --release -p ingot -p cu)
  cp "$STATE/src/target/release/ingot" "$STATE/src/target/release/cu" "$STATE/bin/"
fi

# launchd does not know ingot's "stop, don't restart" exit code 78 the way
# systemd does, so a wrapper turns 78 into a clean exit that KeepAlive respects.
cat > "$STATE/bin/ingot-run" <<EOF
#!/bin/sh
export PATH="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
"$STATE/bin/ingot" --kiln "$KILN" --state-dir "$STATE" --reserve-memory-mb 2048 --reserve-vcpus 2
rc=\$?
if [ \$rc = 78 ]; then echo "ingot stopped: node pending, retired or unauthorized; fix and run: launchctl kickstart -k gui/\$(id -u)/com.copper.ingot"; exit 0; fi
exit \$rc
EOF
chmod +x "$STATE/bin/ingot-run"

plist="$HOME/Library/LaunchAgents/com.copper.ingot.plist"
mkdir -p "$HOME/Library/LaunchAgents"
cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.copper.ingot</string>
  <key>ProgramArguments</key><array><string>$STATE/bin/ingot-run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>StandardOutPath</key><string>$STATE/ingot.log</string>
  <key>StandardErrorPath</key><string>$STATE/ingot.log</string>
</dict>
</plist>
EOF
launchctl bootout "gui/$(id -u)/com.copper.ingot" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist"

say "done"
cat <<EOF
ingot runs as a LaunchAgent for $USER and starts at login.
  log:      tail -f $STATE/ingot.log
  restart:  launchctl kickstart -k gui/$(id -u)/com.copper.ingot
  stop:     launchctl bootout gui/$(id -u)/com.copper.ingot
Next: approve the node from an admin machine with: cu node ls; cu node approve <id>
Also turn on automatic login for $USER in System Settings > Users & Groups,
so VMs come back after a reboot without someone at the keyboard.
EOF
