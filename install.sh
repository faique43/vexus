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

# Hosts the full build can't run on get the *structural* build instead:
# the ONNX embedding runtime is compiled out (it has no binaries for these
# hosts — not a packaging gap), so semantic search is off but keyword +
# call-graph search work fully. `status` reports which build is running.
structural_note() {
  echo "install.sh: note: $1" >&2
  echo "install.sh: note: installing the structural-only build — keyword+graph search only, no semantic search" >&2
}

os="$(uname -s)"
arch="$(uname -m)"
variant=""
case "$os-$arch" in
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64)
    target="x86_64-apple-darwin"
    variant="-structural"
    structural_note "vexus's embedding runtime has no build for Intel macOS"
    ;;
  Linux-x86_64 | Linux-aarch64 | Linux-arm64)
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-gnu" ;;
      *) target="aarch64-unknown-linux-gnu" ;;
    esac
    # musl (Alpine): uname says Linux/x86_64, but a glibc binary won't even
    # exec, so this has to be caught before anything is downloaded. Detect
    # via the dynamic loader.
    if [ -e /lib/ld-musl-x86_64.so.1 ] || [ -e /lib/ld-musl-aarch64.so.1 ] ||
      ldd --version 2>&1 | grep -qi musl; then
      # Not a packaging gap, and not fixable by dropping the embedding
      # runtime: sqlite-vec's C source uses the BSD-only u_int8_t /
      # u_int16_t / u_int64_t typedefs, which glibc supplies and musl does
      # not, so it fails to compile against musl even with
      # --no-default-features.
      die "musl systems (Alpine) are not supported — vexus's vector-search dependency does not build against musl. A glibc container (e.g. the -slim Debian images) is the workaround."
    else
      # The full build's embedded ONNX Runtime needs glibc 2.39+. On older
      # distros (Ubuntu 22.04, Debian 12, RHEL 9) it fails at exec with
      # "version GLIBC_2.39 not found" — catch that here instead.
      glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
      if [ -n "$glibc" ] && [ "$(printf '%s\n' "2.39" "$glibc" | sort -V | head -n 1)" != "2.39" ]; then
        if [ "$arch" = "x86_64" ]; then
          variant="-structural"
          structural_note "glibc $glibc is older than the 2.39 the full build's embedding runtime needs"
        else
          die "glibc $glibc is older than the 2.39 floor and no structural build exists for $arch — see the README's limitations"
        fi
      fi
    fi
    ;;
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

name="vexus-${version}-${target}${variant}"
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
echo "next, inside the repo you want indexed:"
echo "  vexus index .                    # build the index (first run downloads a ~160 MB model; large repos take minutes)"
echo "  vexus init --agent claude-code   # install the steering pack + register the MCP server in .mcp.json"
echo
echo "no need to run 'vexus serve' yourself — your agent launches it via .mcp.json"
