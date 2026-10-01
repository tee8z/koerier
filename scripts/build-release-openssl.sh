#!/usr/bin/env bash
# Build the verified OpenSSL source used by both native Linux release runners.
set -euo pipefail

if [ "$#" -ne 1 ] || [[ $1 != /* || $1 == / ]]; then
  echo "Usage: $0 ABSOLUTE_INSTALL_PREFIX" >&2
  exit 2
fi

prefix=$1
version=3.5.9
checksum=603f5602e2eef00d77fbd429d34dcd5822bb301757a1bc9cdb24c670f1eb859a
if [ -e "$prefix" ]; then
  echo "OpenSSL install prefix already exists: $prefix" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
archive="$work/openssl.tar.gz"
curl --fail --location --retry 3 --proto '=https' --tlsv1.2 \
  "https://github.com/openssl/openssl/releases/download/openssl-$version/openssl-$version.tar.gz" \
  --output "$archive"
printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --strict
tar -xzf "$archive" -C "$work"
cd "$work/openssl-$version"
./Configure no-shared no-module no-tests no-comp no-zlib no-zlib-dynamic \
  --prefix="$prefix" --openssldir=/etc/ssl --libdir=lib
make -j "$(getconf _NPROCESSORS_ONLN)" build_sw
make install_sw
if [ "$("$prefix/bin/openssl" version)" != 'OpenSSL 3.5.9 29 Sep 2026 (Library: OpenSSL 3.5.9 29 Sep 2026)' ]; then
  echo "Unexpected OpenSSL version in the release installation" >&2
  exit 1
fi
install -Dm 0644 LICENSE.txt "$prefix/share/licenses/openssl/LICENSE.txt"
