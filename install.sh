#!/bin/sh
# Installs Reason Network for macOS or Linux. In a terminal it first hands over to Reason's own setup
# (https://reasonmachines.com/install.sh … network: sign in, create or join a network, and get its invite line);
# otherwise, or when told not to, it installs this engine on its own.
#
#   curl -fsSL <this url> | sh                      install, then set up
#   curl -fsSL <this url> | sh -s -- <team link>    install, then join that team
#   curl -fsSL <this url> | sh -s -- <team link> --as <name>     the same, non-interactive
#   curl -fsSL <this url> | sh -s -- init <team> --as <name>     install and start a team, non-interactive
#
# Environment: REASON_NETWORK_BIN_DIR (default ~/.local/bin), REASON_NETWORK_VERSION (a tag like v0.10.4; default latest), REASON_NETWORK_REPO,
# REASON_NETWORK_DOWNLOAD_BASE (a mirror serving the release files), REASON_NETWORK_NO_PROMPT (never ask anything: updates).
set -eu

repo="${REASON_NETWORK_REPO:-reason-machines/reason-network}"
bin_dir="${REASON_NETWORK_BIN_DIR:-$HOME/.local/bin}"
version="${REASON_NETWORK_VERSION:-latest}"

say() { printf '%s\n' "$*"; }
die() { say "reason-network install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) die "reason-network runs on macOS and Linux here (on Windows, use install.ps1)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) die "unsupported CPU: $(uname -m)" ;;
esac
target="$arch-$os"

# Starting fresh in a terminal: Reason's setup, the same as `curl -fsSL https://reasonmachines.com/install.sh | bash -s
# -- network`. You sign in to Reason in your browser, create or join a network there, and this computer's coding agents
# are set up; it ends with the invite line. Reason's CLI then installs and updates this engine. Never asked when joining
# with a link, for agents, or for updates.
if [ $# -eq 0 ] && [ -z "${REASON_NETWORK_NO_PROMPT:-}" ] && [ ! -e "$bin_dir/reason-network" ] && (exec </dev/tty) 2>/dev/null; then
  say "Reason Network: sign in to Reason in your browser (free: Google, GitHub, Apple or email), then create or join a network."
  printf 'Use your Reason account? [Y/n] '
  read -r answer </dev/tty || answer=n
  case "$answer" in
    [nN]*) ;;
    *)
      installer="$(curl -fsSL https://reasonmachines.com/install.sh)" || die "couldn't reach reasonmachines.com; try again"
      exec bash -c "$installer" install.sh network </dev/tty
      ;;
  esac
fi

if [ -n "${REASON_NETWORK_DOWNLOAD_BASE:-}" ]; then
  base="$REASON_NETWORK_DOWNLOAD_BASE"
elif [ "$version" = latest ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/$version"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading reason-network for $target..."
curl -fsSL "$base/reason-network-$target.tar.gz" -o "$tmp/reason-network.tar.gz" || die "download failed ($base/reason-network-$target.tar.gz)"
curl -fsSL "$base/reason-network-$target.tar.gz.sha256" -o "$tmp/sum" || die "checksum download failed; not installing"
want="$(cut -d' ' -f1 < "$tmp/sum")"
if command -v shasum >/dev/null 2>&1; then
  got="$(shasum -a 256 "$tmp/reason-network.tar.gz" | cut -d' ' -f1)"
else
  got="$(sha256sum "$tmp/reason-network.tar.gz" | cut -d' ' -f1)"
fi
[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch; not installing"
# As whoever runs this owns it, not as the archive says (releases up to v0.12.3 say 1001): as root, tar otherwise gives it
# to that owner, which a restricted root (in a user namespace) can't.
tar --no-same-owner -xzf "$tmp/reason-network.tar.gz" -C "$tmp"
"$tmp/reason-network" --version >/dev/null 2>&1 || die "the downloaded binary doesn't run here; not installing"
# macOS knows a program by its code signature when it asks whether it may reach the local network. A build signed by
# a developer team keeps that answer across updates; an unsigned one is a new program every time. So a signed
# reason-network is only ever replaced by one its team signed.
team() { codesign -dv "$1" 2>&1 | sed -n 's/^TeamIdentifier=//p'; }
if [ "$os" = apple-darwin ] && [ -e "$bin_dir/reason-network" ] && command -v codesign >/dev/null 2>&1; then
  have="$(team "$bin_dir/reason-network")"
  case "$have" in
    "" | "not set") ;;
    *) [ "$(team "$tmp/reason-network")" = "$have" ] ||
         die "this build isn't signed by $have, as the installed one is; keeping that one (try again later)" ;;
  esac
fi
mkdir -p "$bin_dir"
# A new file moved into place: whatever runs the old one (a hook, an agent's tools) never sees a half-written binary.
install -m 755 "$tmp/reason-network" "$bin_dir/.reason-network.new"
mv -f "$bin_dir/.reason-network.new" "$bin_dir/reason-network"
say "Installed $bin_dir/reason-network ($("$bin_dir/reason-network" --version))"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) say "Note: $bin_dir is not on your PATH. Add this to your shell profile:"
     say "  export PATH=\"$bin_dir:\$PATH\"" ;;
esac

rm -rf "$tmp"
trap - EXIT INT TERM

# Hand over to reason-network: on its own (without Reason sign-in), it asks a few questions in your terminal. Under
# `curl | sh` our stdin is the script, so read answers from the terminal itself when there is one.
# (In a subshell: in dash, Ubuntu's sh, a failed redirection on `:` would end the whole script.)
if [ -z "${REASON_NETWORK_NO_PROMPT:-}" ] && (exec </dev/tty) 2>/dev/null; then
  case "${1:-}" in
    "") exec "$bin_dir/reason-network" </dev/tty ;;
    -*|init|join|setup) exec "$bin_dir/reason-network" "$@" </dev/tty ;;
    *) exec "$bin_dir/reason-network" join "$@" </dev/tty ;;
  esac
fi
case "${1:-}" in
  "") say ""; say "Next, in a terminal: reason-network" ;;
  -*|init|join|setup) exec "$bin_dir/reason-network" "$@" ;;
  *) exec "$bin_dir/reason-network" join "$@" ;;
esac
