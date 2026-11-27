#!/bin/sh
# Axle installer, served by the server it connects to. Usage: curl -fsSL <server>/install.sh | sh -s -- [CODE]
set -eu

SERVER_URL='{{ server_url }}'
VERSION='{{ version }}'

die() {
  echo "axle install: $*" >&2
  exit 1
}

if [ "$(id -u)" = "0" ] && [ "${AXL_ALLOW_ROOT:-}" != "1" ]; then
  die "refusing to run as root (Axle installs per user). Set AXL_ALLOW_ROOT=1 to override."
fi

case "$(uname -s)" in
  Darwin | Linux) ;;
  *) die "unsupported operating system: $(uname -s) (macOS and Linux only)" ;;
esac
case "$(uname -m)" in
  x86_64 | arm64 | aarch64) ;;
  *) die "unsupported architecture: $(uname -m) (x86_64 and arm64 only)" ;;
esac

# Succeeds when version $1 is at least version $2 (numeric major.minor.patch; a -suffix is ignored).
version_ge() {
  a=${1%%-*}
  b=${2%%-*}
  for _ in 1 2 3; do
    a_part=${a%%.*}
    b_part=${b%%.*}
    case "$a" in *.*) a=${a#*.} ;; *) a=0 ;; esac
    case "$b" in *.*) b=${b#*.} ;; *) b=0 ;; esac
    a_part=${a_part:-0}
    b_part=${b_part:-0}
    [ "$a_part" -gt "$b_part" ] && return 0
    [ "$a_part" -lt "$b_part" ] && return 1
  done
  return 0
}

# Succeeds when the host of URL $1 is exactly localhost, 127.0.0.1 or [::1].
is_loopback_host() {
  host=${1#*://}
  host=${host%%/*}
  host=${host%%\\*}
  host=${host%%\?*}
  host=${host%%\#*}
  host=${host##*@}
  case "$host" in
    '['*) host="${host%%]*}]" ;;
    *) host=${host%%:*} ;;
  esac
  case "$host" in
    localhost | 127.0.0.1 | '[::1]') return 0 ;;
    *) return 1 ;;
  esac
}

# Everything runs inside main so sh has read the whole script before the first command.
main() {
  AXL=
  found=$(command -v axl 2>/dev/null) || found=
  if [ -n "$found" ]; then
    have=$("$found" --version 2>/dev/null | awk '{ print $NF }') || have=
    if [ -n "$have" ] && version_ge "$have" "$VERSION"; then
      AXL=$found
    fi
  fi

  if [ -z "$AXL" ]; then
    echo "Installing axl $VERSION..."
    curl -fsSL "https://github.com/ReyemTech/axle/releases/download/v{{ version }}/axle-mcp-installer.sh" | sh
    AXL="${CARGO_HOME:-$HOME/.cargo}/bin/axl"
    [ -x "$AXL" ] || die "the installer did not produce $AXL"
  fi

  # From here on `axl` is the binary resolved above, run with the terminal as stdin when there is
  # one, else with none, so it can never read the rest of this script when piped into sh.
  axl() {
    if (exec </dev/tty) 2>/dev/null; then
      "$AXL" "$@" </dev/tty
    else
      "$AXL" "$@" </dev/null
    fi
  }

  case "$SERVER_URL" in
    http://*)
      if ! is_loopback_host "$SERVER_URL"; then
        echo "Warning: $SERVER_URL is plain http; your token will cross the network unencrypted." >&2
        printf 'Continue? [y/N] ' >&2
        answer=
        read -r answer </dev/tty || die "cannot ask for confirmation without a terminal; use an https server URL"
        [ "$answer" = "y" ] || die "cancelled"
      fi
      ;;
  esac

  if [ -n "${1:-}" ]; then
    axl install --server '{{ server_url }}' --code "$1"
  else
    axl install --server '{{ server_url }}'
  fi
  axl status
}

main "$@"
