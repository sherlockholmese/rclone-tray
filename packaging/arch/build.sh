#!/usr/bin/env bash
set -euo pipefail

recipe=$(dirname "$(realpath "${BASH_SOURCE[0]}")")
root=$(realpath "$recipe/../..")
output="$root/target/arch"
version=$(awk -F= '$1 == "pkgver" {print $2; exit}' "$recipe/PKGBUILD")
mkdir -p "$output"

# Include current local changes, never credentials, caches, or build artifacts.
tar -czf "$output/rclone-tray-$version.tar.gz" \
    --transform "s,^,rclone-tray-$version/," \
    -C "$root" Cargo.toml Cargo.lock src data README.md
cp "$recipe/PKGBUILD" "$output/PKGBUILD"
makepkg --dir "$output" --geninteg >> "$output/PKGBUILD"
makepkg --dir "$output" --cleanbuild --force "$@"
