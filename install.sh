#!/bin/sh
# copper installer. Installs kiln (control plane) or ingot (node agent) as a
# systemd service. Idempotent: re-run to upgrade or change settings.
#
#   curl -fsSL https://raw.githubusercontent.com/Shreyas220/copper/main/install.sh | sudo sh -s -- kiln
#   curl -fsSL https://raw.githubusercontent.com/Shreyas220/copper/main/install.sh | sudo sh -s -- ingot --kiln http://kiln:8400
#
# Options:
#   --version vX.Y.Z   release to install (default: latest)
#   --binaries DIR     use binaries from DIR instead of downloading (for development)
#   --listen ADDR:PORT kiln only; default is the Tailscale IPv4 address, port 8400
#   --kiln URL         ingot only; default http://kiln:8400
#   --no-image         ingot only; skip downloading the Ubuntu 24.04 base image
#   --dry-run          print what would be done
set -eu

REPO="Shreyas220/copper"
BIN_DIR=/usr/local/bin
STATE_DIR=/var/lib/copper
CONF_DIR=/etc/copper
IMAGE_URL="https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
IMAGE_URL_ARM="https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img"

role=""; version="latest"; binaries=""; listen=""; kiln_url=""; want_image=1; dry=0
while [ $# -gt 0 ]; do
  case "$1" in
    kiln|ingot) role="$1" ;;
    --version) version="$2"; shift ;;
    --binaries) binaries="$2"; shift ;;
    --listen) listen="$2"; shift ;;
    --kiln) kiln_url="$2"; shift ;;
    --no-image) want_image=0 ;;
    --dry-run) dry=1 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
[ -n "$role" ] || { echo "usage: install.sh kiln|ingot [options]" >&2; exit 2; }

run() { if [ "$dry" = 1 ]; then echo "+ $*"; else "$@"; fi; }
say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || [ "$dry" = 1 ] || die "run as root (sudo)"
command -v systemctl >/dev/null || die "systemd is required"

arch=$(uname -m)
case "$arch" in
  x86_64) ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "unsupported architecture $arch" ;;
esac

# ---- binaries --------------------------------------------------------------
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
if [ -n "$binaries" ]; then
  say "using binaries from $binaries"
  src="$binaries"
else
  if [ "$version" = latest ]; then
    url="https://github.com/$REPO/releases/latest/download/copper-$arch-linux.tar.gz"
  else
    url="https://github.com/$REPO/releases/download/$version/copper-$arch-linux.tar.gz"
  fi
  say "downloading $url"
  if [ "$dry" = 0 ]; then
    curl -fsSL "$url" -o "$tmp/copper.tar.gz" || die "download failed; is there a release for $arch?"
    tar -xzf "$tmp/copper.tar.gz" -C "$tmp"
  fi
  src="$tmp"
fi
for b in kiln ingot cu; do
  # Each node gets all three; cu is tiny and handy for debugging on the box.
  [ -f "$src/$b" ] || [ "$dry" = 1 ] || die "missing binary $src/$b"
  run install -m 755 "$src/$b" "$BIN_DIR/$b"
done

# ---- user and directories --------------------------------------------------
if ! id copper >/dev/null 2>&1; then
  say "creating system user copper"
  run useradd --system --home-dir "$STATE_DIR" --shell /usr/sbin/nologin copper
fi
run install -d -o copper -g copper -m 755 "$STATE_DIR"
run install -d -m 755 "$CONF_DIR"

# ---- role specifics --------------------------------------------------------
unit_src="https://raw.githubusercontent.com/$REPO/main/dist/systemd"
fetch_unit() {
  if [ -n "$binaries" ] && [ -f "$binaries/../dist/systemd/$1" ]; then
    cp "$binaries/../dist/systemd/$1" "$tmp/$1"
  elif [ -n "$binaries" ] && [ -f "$binaries/../../dist/systemd/$1" ]; then
    cp "$binaries/../../dist/systemd/$1" "$tmp/$1"
  else
    curl -fsSL "$unit_src/$1" -o "$tmp/$1" || die "could not fetch $1"
  fi
}

case "$role" in
  kiln)
    if [ -z "$listen" ]; then
      ts=$(tailscale ip -4 2>/dev/null || true)
      [ -n "$ts" ] && listen="$ts:8400" || listen="0.0.0.0:8400"
    fi
    say "kiln will listen on $listen"
    if [ "$dry" = 0 ]; then
      printf 'KILN_LISTEN=%s\n' "$listen" > "$CONF_DIR/kiln.env"
      fetch_unit kiln.service
      install -m 644 "$tmp/kiln.service" /etc/systemd/system/kiln.service
    else
      echo "+ write $CONF_DIR/kiln.env (KILN_LISTEN=$listen)"; echo "+ install /etc/systemd/system/kiln.service"
    fi
    ;;
  ingot)
    for need in qemu-img cloud-localds; do
      command -v "$need" >/dev/null || die "$need not found; apt install qemu-utils cloud-image-utils libvirt-daemon-system"
    done
    getent group libvirt >/dev/null || die "libvirt group missing; apt install libvirt-daemon-system"
    [ -n "$kiln_url" ] || kiln_url="http://kiln:8400"
    say "ingot will talk to $kiln_url"
    run install -d -o copper -g copper -m 755 "$STATE_DIR/images" "$STATE_DIR/disks"
    if [ "$want_image" = 1 ]; then
      img="$STATE_DIR/images/ubuntu-24.04.img"
      if [ -f "$img" ]; then
        say "base image already present"
      else
        [ "$arch" = aarch64 ] && IMAGE_URL="$IMAGE_URL_ARM"
        say "downloading Ubuntu 24.04 base image (~600 MB)"
        run curl -fL --progress-bar "$IMAGE_URL" -o "$img.part"
        run mv "$img.part" "$img"
        run chown copper:copper "$img"
      fi
    fi
    if [ "$dry" = 0 ]; then
      printf 'INGOT_KILN=%s\n' "$kiln_url" > "$CONF_DIR/ingot.env"
      fetch_unit ingot.service
      install -m 644 "$tmp/ingot.service" /etc/systemd/system/ingot.service
    else
      echo "+ write $CONF_DIR/ingot.env (INGOT_KILN=$kiln_url)"; echo "+ install /etc/systemd/system/ingot.service"
    fi
    ;;
esac

# ---- start -----------------------------------------------------------------
say "enabling $role.service"
run systemctl daemon-reload
run systemctl enable --now "$role.service"
run systemctl restart "$role.service"

echo
case "$role" in
  kiln)
    cat <<MSG
kiln is running on $listen. Create your admin token:

  sudo -u copper kiln --db $STATE_DIR/kiln.db token --user $(logname 2>/dev/null || echo admin) --role admin

Then from any machine on the tailnet:

  export CU_URL=http://$listen  CU_TOKEN=<token>
  cu project create default
MSG
    ;;
  ingot)
    cat <<MSG
ingot is running and will register with $kiln_url.
Approve it from a machine with an admin token:

  cu node ls
  cu node approve <id>

Logs: journalctl -u ingot -f
MSG
    ;;
esac
