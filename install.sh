#!/bin/sh
# Install a prebuilt gorp binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/nlaz/gorp/main/install.sh | sh
#
# Environment:
#   GORP_VERSION      pin a version tag (e.g. v0.1.0); default: latest release
#   GORP_INSTALL_DIR  where to put the binary; default: ~/.local/bin
set -eu

repo="nlaz/gorp"
install_dir="${GORP_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) echo "install.sh: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch="aarch64" ;;
  x86_64 | amd64) arch="x86_64" ;;
  *) echo "install.sh: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
target="${arch}-${os}"
asset="gorp-${target}.tar.gz"

if [ -n "${GORP_VERSION:-}" ]; then
  base="https://github.com/${repo}/releases/download/${GORP_VERSION}"
else
  base="https://github.com/${repo}/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "fetching ${base}/${asset}" >&2
curl -fsSL -o "${tmp}/${asset}" "${base}/${asset}"
curl -fsSL -o "${tmp}/${asset}.sha256" "${base}/${asset}.sha256"

cd "$tmp"
if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 -c "${asset}.sha256" >/dev/null
elif command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c "${asset}.sha256" >/dev/null
else
  echo "install.sh: warning: no shasum/sha256sum found, skipping checksum verification" >&2
fi
tar xzf "$asset"

mkdir -p "$install_dir"
if ! install -m 755 "gorp-${target}/gorp" "${install_dir}/gorp" 2>/dev/null; then
  echo "install.sh: cannot write to ${install_dir}" >&2
  echo "  re-run with GORP_INSTALL_DIR set to a writable directory" >&2
  echo "  (this script never sudos on its own)" >&2
  exit 1
fi

echo "installed $("${install_dir}/gorp" -V) to ${install_dir}/gorp" >&2
case ":${PATH}:" in
  *":${install_dir}:"*) ;;
  *) echo "note: ${install_dir} is not on your PATH — add it to your shell profile" >&2 ;;
esac
