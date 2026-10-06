#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$project_dir/Cargo.toml" | head -n1)"
architecture="$(dpkg --print-architecture)"
package_root="$(mktemp -d)"
trap 'rm -rf "$package_root"' EXIT

if command -v node >/dev/null; then
  for script in extension.js glass.js system.js timer.js duration.js; do
    node --check --input-type=module \
      <"$project_dir/packaging/gnome-shell-extension/$script"
  done
  # The timer field's parser, checked on a few inputs of each form.
  (cd "$project_dir/packaging/gnome-shell-extension" &&
    node --input-type=module -e "import('./duration.js').then(m => m.check())")
fi

cargo build --manifest-path "$project_dir/Cargo.toml" --release --locked

install -Dm755 "$project_dir/target/release/sysi" "$package_root/usr/bin/sysi"
install -Dm644 "$project_dir/packaging/io.sysi.Overlay.desktop" \
  "$package_root/usr/share/applications/io.sysi.Overlay.desktop"
install -Dm644 "$project_dir/packaging/io.sysi.Overlay-autostart.desktop" \
  "$package_root/etc/xdg/autostart/io.sysi.Overlay.desktop"
install -Dm644 "$project_dir/assets/sysi-icon.svg" \
  "$package_root/usr/share/icons/hicolor/scalable/apps/io.sysi.Overlay.svg"
install -Dm644 "$project_dir/packaging/gnome-shell-extension/metadata.json" \
  "$package_root/usr/share/gnome-shell/extensions/sysi-panel@thaihoc/metadata.json"
install -Dm644 "$project_dir/packaging/gnome-shell-extension/extension.js" \
  "$package_root/usr/share/gnome-shell/extensions/sysi-panel@thaihoc/extension.js"
install -Dm644 "$project_dir/packaging/gnome-shell-extension/glass.js" \
  "$package_root/usr/share/gnome-shell/extensions/sysi-panel@thaihoc/glass.js"
install -Dm644 "$project_dir/packaging/gnome-shell-extension/system.js" \
  "$package_root/usr/share/gnome-shell/extensions/sysi-panel@thaihoc/system.js"
install -Dm644 "$project_dir/packaging/gnome-shell-extension/timer.js" \
  "$package_root/usr/share/gnome-shell/extensions/sysi-panel@thaihoc/timer.js"
install -Dm644 "$project_dir/packaging/gnome-shell-extension/duration.js" \
  "$package_root/usr/share/gnome-shell/extensions/sysi-panel@thaihoc/duration.js"
install -Dm644 "$project_dir/packaging/gnome-shell-extension/stylesheet.css" \
  "$package_root/usr/share/gnome-shell/extensions/sysi-panel@thaihoc/stylesheet.css"
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
Recommends: gnome-shell (>= 45), tesseract-ocr, tesseract-ocr-vie, pipewire-bin | pulseaudio-utils
Installed-Size: $installed_kib
Maintainer: Sysi contributors
Description: Lightweight transparent desktop widgets for Ubuntu
 A native Rust and GTK overlay with system meters, countdown timers,
 pinned note history, automatic foreground contrast, and click-through interaction.
EOF

output="$project_dir/dist/sysi-overlay_${version}_${architecture}.deb"
dpkg-deb --root-owner-group --build "$package_root" "$output"
echo "$output"
