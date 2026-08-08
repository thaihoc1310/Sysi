#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$project_dir/Cargo.toml" | head -n1)"
architecture="$(dpkg --print-architecture)"
package_root="$(mktemp -d)"
trap 'rm -rf "$package_root"' EXIT

cargo build --manifest-path "$project_dir/Cargo.toml" --release --locked

install -Dm755 "$project_dir/target/release/sysi" "$package_root/usr/bin/sysi"
install -Dm644 "$project_dir/packaging/io.sysi.Overlay.desktop" \
  "$package_root/usr/share/applications/io.sysi.Overlay.desktop"
install -Dm644 "$project_dir/packaging/io.sysi.Overlay-autostart.desktop" \
  "$package_root/etc/xdg/autostart/io.sysi.Overlay.desktop"
install -Dm644 "$project_dir/assets/sysi-icon.svg" \
  "$package_root/usr/share/icons/hicolor/scalable/apps/io.sysi.Overlay.svg"
install -Dm644 "$project_dir/README.md" \
  "$package_root/usr/share/doc/sysi-overlay/README.md"
install -Dm644 "$project_dir/LICENSE" \
  "$package_root/usr/share/doc/sysi-overlay/copyright"

installed_kib="$(du -sk "$package_root/usr" | cut -f1)"
mkdir -p "$package_root/DEBIAN" "$project_dir/dist"
cat >"$package_root/DEBIAN/control" <<EOF
Package: sysi-overlay
Version: $version
Section: utils
Priority: optional
Architecture: $architecture
Depends: libgtk-3-0t64 (>= 3.24) | libgtk-3-0 (>= 3.24), libx11-6
Installed-Size: $installed_kib
Maintainer: Sysi contributors
Description: Lightweight transparent desktop widgets for Ubuntu
 A native Rust and GTK overlay with system meters, countdown timers,
 pinned note history, manual color modes, and click-through interaction.
EOF

output="$project_dir/dist/sysi-overlay_${version}_${architecture}.deb"
dpkg-deb --root-owner-group --build "$package_root" "$output"
echo "$output"
