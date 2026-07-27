#!/bin/sh
# vexus installer.
#
#   curl -fsSL https://raw.githubusercontent.com/faique43/vexus/main/install.sh | sh
#
# Downloads the release archive for this platform, verifies it against the
# release's SHA256SUMS, and installs the binary to ~/.local/bin (override
# with VEXUS_INSTALL_DIR). Set VEXUS_VERSION to pin a version instead of
# taking the latest.

set -eu

repo="faique43/vexus"
# `set -u` would abort on an unset HOME before die() could explain why.
install_dir="${VEXUS_INSTALL_DIR:-${HOME:-}/.local/bin}"

die() {
  echo "install.sh: $1" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

[ "$install_dir" != "/.local/bin" ] || die "neither VEXUS_INSTALL_DIR nor HOME is set"

need curl
need tar

# Prefer sha256sum (Linux), fall back to shasum (macOS). Verification is not
# optional: an unverified binary is worse than no binary.
if command -v sha256sum >/dev/null 2>&1; then
  sha_cmd="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  sha_cmd="shasum -a 256"
else
  die "need sha256sum or shasum to verify the download"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os-$arch" in
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  *)
    die "no prebuilt binary for $os-$arch — build from source: cargo install --git https://github.com/$repo vexus-cli"
    ;;
esac

version="${VEXUS_VERSION:-}"
if [ -z "$version" ]; then
  version="$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$version" ] || die "could not determine the latest release (rate limited? set VEXUS_VERSION)"
fi
version="${version#v}"

name="vexus-${version}-${target}"
base="https://github.com/$repo/releases/download/v${version}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "vexus ${version} (${target})"
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" ||
  die "download failed: $base/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" ||
  die "download failed: $base/SHA256SUMS"

expected="$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | awk '{print $1}')"
[ -n "$expected" ] || die "no checksum for $name.tar.gz in SHA256SUMS"
actual="$(cd "$tmp" && $sha_cmd "$name.tar.gz" | awk '{print $1}')"
[ "$expected" = "$actual" ] || die "checksum mismatch — refusing to install
  expected $expected
  actual   $actual"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$install_dir"
mv "$tmp/$name/vexus" "$install_dir/vexus"
chmod +x "$install_dir/vexus"

echo "installed $install_dir/vexus"

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *)
    echo
    echo "$install_dir is not on your PATH. Add it:"
    echo "  export PATH=\"$install_dir:\$PATH\""
    ;;
esac

echo
echo "next:"
echo "  vexus index .                 # build the index for a repo"
echo "  vexus init --agent claude-code  # install the steering pack"
echo "  vexus serve .                 # run the MCP server"
