#!/usr/bin/env bash
# A throwaway GNOME Shell for working on the glass without logging out.
#
# Runs a headless gnome-shell on its own D-Bus session and XDG dirs, with the
# Sysi extension from this tree and a small helper extension that runs
# JavaScript in the shell and saves screenshots. A shell crash only takes the
# harness down.
#
#   scripts/glass-harness.sh start [--scale2]   shell + a debug build of Sysi
#   scripts/glass-harness.sh js 'return 1 + 1'  body of an async function; `h`
#                                               holds Clutter, Cogl, Main, ...
#   scripts/glass-harness.sh shot out.png       the whole stage
#   scripts/glass-harness.sh stop
#   GDB=1 scripts/glass-harness.sh start ...   run the shell under gdb; a crash
#                                              leaves a backtrace in shell.log
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root="${SYSI_HARNESS_DIR:-${TMPDIR:-/tmp}/sysi-glass-harness}"
# Wayland sockets live here, and their paths must stay under 108 bytes.
runtime="${TMPDIR:-/tmp}/sysi-harness-rt"
helper="$root/data/gnome-shell/extensions/harness@sysi"

run_js() {
  local out="$root/cache/harness/out.txt"
  rm -f "$out"
  printf '%s' "$1" >"$root/cache/harness/cmd.js.tmp"
  mv "$root/cache/harness/cmd.js.tmp" "$root/cache/harness/cmd.js"
  for _ in $(seq 1 150); do
    [ -f "$out" ] && { cat "$out"; return 0; }
    sleep 0.1
  done
  echo "TIMEOUT" >&2
  return 1
}

stop_all() {
  [ -f "$root/shell.pid" ] && kill "$(cat "$root/shell.pid")" 2>/dev/null || true
  [ -f "$root/sysi.pid" ] && kill "$(cat "$root/sysi.pid")" 2>/dev/null || true
  rm -f "$root/shell.pid" "$root/sysi.pid"
}

write_helper() {
  mkdir -p "$helper"
  cat >"$helper/metadata.json" <<'EOF'
{"uuid": "harness@sysi", "name": "Sysi glass harness", "description": "Test only", "shell-version": ["50"], "version": 1}
EOF
  cat >"$helper/extension.js" <<'EOF'
import Clutter from 'gi://Clutter';
import Cogl from 'gi://Cogl';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

Gio._promisify(Shell.Screenshot.prototype, 'screenshot');

export default class Harness extends Extension {
    enable() {
        this._dir = GLib.build_filenamev([GLib.get_user_cache_dir(), 'harness']);
        GLib.mkdir_with_parents(this._dir, 0o700);
        const h = {Clutter, Cogl, GLib, Gio, Meta, Shell, St, Main};
        h.shot = async path => {
            const stream = Gio.File.new_for_path(path)
                .replace(null, false, Gio.FileCreateFlags.NONE, null);
            await new Shell.Screenshot().screenshot(false, stream);
            stream.close(null);
            return path;
        };
        this._timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 200, () => {
            this._poll(h);
            return GLib.SOURCE_CONTINUE;
        });
        GLib.file_set_contents(`${this._dir}/ready.txt`, 'ready\n');
    }

    async _poll(h) {
        const cmd = `${this._dir}/cmd.js`;
        if (this._busy || !GLib.file_test(cmd, GLib.FileTest.EXISTS))
            return;
        this._busy = true;
        let out;
        try {
            const [, bytes] = GLib.file_get_contents(cmd);
            GLib.unlink(cmd);
            const body = new TextDecoder().decode(bytes);
            const value = await new Function('h', `return (async () => { ${body} })();`)(h);
            out = `OK ${typeof value === 'string' ? value : JSON.stringify(value)}\n`;
        } catch (error) {
            out = `ERR ${error}\n${error.stack}\n`;
        }
        GLib.file_set_contents(`${this._dir}/out.txt`, out);
        this._busy = false;
    }

    disable() {
        GLib.source_remove(this._timer);
    }
}
EOF
}

start() {
  local scale2=false
  [ "${1:-}" = "--scale2" ] && scale2=true
  stop_all
  mkdir -p "$root"/{config/glib-2.0/settings,data,cache/harness,bin} "$runtime"
  chmod 700 "$runtime"
  rm -f "$runtime/gnome-shell-disable-extensions" "$root/cache/harness/ready.txt"
  cat >"$root/config/glib-2.0/settings/keyfile" <<'EOF'
[org/gnome/shell]
enabled-extensions=['harness@sysi', 'sysi-panel@thaihoc']
disable-user-extensions=false
disable-extension-version-validation=true
welcome-dialog-last-shown-version='999'
EOF
  write_helper
  mkdir -p "$root/data/gnome-shell/extensions/sysi-panel@thaihoc"
  cp "$project_dir"/packaging/gnome-shell-extension/* \
    "$root/data/gnome-shell/extensions/sysi-panel@thaihoc/"
  cargo build --manifest-path "$project_dir/Cargo.toml"
  # The extension spawns `sysi --panel-action`; make that this build.
  ln -sf "$project_dir/target/debug/sysi" "$root/bin/sysi"

  local monitor=1280x800
  $scale2 && monitor=2560x1600
  # HARNESS_MONITORS="3840x2160 1728x3072" gives several virtual monitors;
  # HARNESS_LAYOUT then places them (an ApplyMonitorsConfig logical list).
  local monitors="--virtual-monitor $monitor"
  if [ -n "${HARNESS_MONITORS:-}" ]; then
    monitors=""
    for size in $HARNESS_MONITORS; do monitors="$monitors --virtual-monitor $size"; done
  fi
  cat >"$root/env.sh" <<EOF
export XDG_CONFIG_HOME=$root/config XDG_DATA_HOME=$root/data XDG_CACHE_HOME=$root/cache
export XDG_RUNTIME_DIR=$runtime GSETTINGS_BACKEND=keyfile NO_AT_BRIDGE=1 PATH=$root/bin:\$PATH
unset WAYLAND_DISPLAY DISPLAY GDK_BACKEND
EOF
  (
    # shellcheck source=/dev/null
    source "$root/env.sh"
    setsid dbus-run-session -- bash -c "
      echo \"export DBUS_SESSION_BUS_ADDRESS='\$DBUS_SESSION_BUS_ADDRESS'\" >>'$root/env.sh'
      echo \$\$ >'$root/shell.pid'
      exec ${GDB:+gdb -batch -ex run -ex bt -ex 'call (void) gjs_dumpstack()' --args} gnome-shell --headless --wayland --wayland-display=sysi-harness \
        $monitors" >"$root/shell.log" 2>&1 &
  )
  for _ in $(seq 1 150); do
    [ -f "$root/cache/harness/ready.txt" ] && break
    sleep 0.2
  done
  [ -f "$root/cache/harness/ready.txt" ] || { echo "the shell did not start; see $root/shell.log" >&2; exit 1; }
  # shellcheck source=/dev/null
  source "$root/env.sh"
  if $scale2; then
    local layout="${HARNESS_LAYOUT:-}"
    [ -n "$layout" ] || layout="[(0, 0, 2.0, 0, true, [('Meta-0', '2560x1600@60.000', {})])]"
    local serial
    serial="$(gdbus call --session --dest org.gnome.Mutter.DisplayConfig \
      --object-path /org/gnome/Mutter/DisplayConfig \
      --method org.gnome.Mutter.DisplayConfig.GetCurrentState | grep -o '^(uint32 [0-9]*' | grep -o '[0-9]*$')"
    gdbus call --session --dest org.gnome.Mutter.DisplayConfig \
      --object-path /org/gnome/Mutter/DisplayConfig \
      --method org.gnome.Mutter.DisplayConfig.ApplyMonitorsConfig "$serial" 1 \
      "$layout" "{}" >/dev/null
    export GDK_SCALE=2
  fi
  DISPLAY=":$(grep -ao 'public X11 display :[0-9]*' "$root/shell.log" | tail -1 | grep -o '[0-9]*$')"
  XAUTHORITY="$(ls -t "$runtime"/.mutter-Xwaylandauth.* | head -1)"
  export DISPLAY XAUTHORITY
  setsid "$root/bin/sysi" >"$root/sysi.log" 2>&1 &
  echo $! >"$root/sysi.pid"
  echo "harness up: DISPLAY=$DISPLAY, logs in $root"
}

case "${1:-}" in
  start) shift; start "$@" ;;
  js) run_js "$2" ;;
  shot) run_js "return await h.shot('$(realpath -m "$2")')" ;;
  stop) stop_all ;;
  *) sed -n '2,14p' "$0"; exit 2 ;;
esac
