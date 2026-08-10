#!/usr/bin/env bash
# End-to-end smoke test: run the real plugin inside a real zellij and read the
# rendered status bar.
#
# Unit tests cannot see the failure this guards against. The 2026-08-10
# deadlock came from zellij rebuilding the plugin's WASI context on
# change_host_folder, which restarts CLOCK_MONOTONIC; only a real session
# exercises that. Same for the contracts this crate depends on but does not
# own: the zjstatus pipe protocol, the permission cache key format, /host.
#
# Not a `cargo test`: an integration test under tests/ makes cargo build the
# [[bin]] target too, and that binary only links against the zellij wasm host
# (undefined host_run_plugin_command natively).
#
# Usage: scripts/e2e.sh
#   ZELLIJ_BIN     zellij 0.44.3            (default: from PATH)
#   ZJSTATUS_WASM  zjstatus.wasm            (default: ~/.config/zellij-plugins)
#   ZJ_SYSINFO_WASM  plugin under test      (default: build it)

set -euo pipefail

ZELLIJ_VERSION=0.44.3
# Two sampling intervals plus session startup, with margin: the first sample
# can only report "-" because a rate needs two of them.
DEADLINE_SECS=20
SESSION="zj-sysinfo-e2e-$$"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() { printf 'e2e: %s\n' "$1" >&2; exit 1; }

ZELLIJ_BIN="${ZELLIJ_BIN:-$(command -v zellij || true)}"
[ -n "$ZELLIJ_BIN" ] || die "zellij not found; install $ZELLIJ_VERSION or set ZELLIJ_BIN"

# The plugin is written against this version's internals (WASI context rebuild,
# permission cache format). A bump must be a deliberate edit after re-checking
# them, not a silent inheritance from whatever is on PATH.
version="$("$ZELLIJ_BIN" --version)"
case "$version" in
  *"$ZELLIJ_VERSION"*) ;;
  *) die "expected zellij $ZELLIJ_VERSION, found: $version" ;;
esac

ZJSTATUS_WASM="${ZJSTATUS_WASM:-$HOME/.config/zellij-plugins/zjstatus.wasm}"
[ -f "$ZJSTATUS_WASM" ] || die "zjstatus.wasm not found at $ZJSTATUS_WASM; set ZJSTATUS_WASM (releases: https://github.com/dj95/zjstatus/releases)"

if [ -z "${ZJ_SYSINFO_WASM:-}" ]; then
  cargo build --release --locked --target wasm32-wasip1 --manifest-path "$REPO/Cargo.toml"
  ZJ_SYSINFO_WASM="$REPO/target/wasm32-wasip1/release/zj-sysinfo.wasm"
fi
[ -f "$ZJ_SYSINFO_WASM" ] || die "plugin wasm not found at $ZJ_SYSINFO_WASM"

# Resolve before seeding: zellij keys the permission cache by the plugin's
# resolved path, so a symlink (a nix build result, for one) would be granted
# under a name that never matches and the plugin would sit silently unpermitted
# -- the same blank widgets this test is looking for.
ZJ_SYSINFO_WASM="$(realpath "$ZJ_SYSINFO_WASM")"
ZJSTATUS_WASM="$(realpath "$ZJSTATUS_WASM")"

WORK="$(mktemp -d)"
cleanup() {
  # Killing the pty client leaves the zellij server running, so the session
  # has to be torn down explicitly or it outlives the test.
  TMPDIR="$WORK" "$ZELLIJ_BIN" kill-session "$SESSION" >/dev/null 2>&1 || true
  TMPDIR="$WORK" "$ZELLIJ_BIN" delete-session "$SESSION" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# A paneless background plugin can never answer zellij's permission prompt, so
# the grant has to be seeded. The key is the wasm's absolute path with no
# "file:" prefix, and the entry must cover every permission the plugin asks
# for, or zellij re-prompts instead of auto-granting.
#
# zjstatus needs its own entry too. It has a pane and could prompt, but an
# unanswered prompt replaces the bar with the permission screen and nothing
# renders -- indistinguishable from the bug under test.
mkdir -p "$WORK/cache/zellij"
cat >"$WORK/cache/zellij/permissions.kdl" <<EOF
"$ZJ_SYSINFO_WASM" {
    FullHdAccess
    RunCommands
    ReadApplicationState
    MessageAndLaunchOtherPlugins
}
"$ZJSTATUS_WASM" {
    ReadApplicationState
    ChangeApplicationState
    RunCommands
}
EOF

# show_release_notes: on a HOME that has never run this version, zellij opens
# a "welcome to Zellij" screen over the layout, so the bar never renders and
# the run looks exactly like a silent plugin. A developer's machine hides this
# because its data dir already records the version -- CI's does not.
cat >"$WORK/config.kdl" <<EOF
show_release_notes false
load_plugins {
    "file:$ZJ_SYSINFO_WASM"
}
EOF

# Angle brackets, not square: a bare "[" in a zjstatus format string is eaten
# by its "#[...]" styling syntax and the whole segment silently fails to
# render, which looks exactly like the bug under test.
cat >"$WORK/layout.kdl" <<EOF
layout {
    default_tab_template {
        children
        pane size=1 borderless=true {
            plugin location="file:$ZJSTATUS_WASM" {
                format_left   "NS<{pipe_netspeed}>UP<{pipe_uptime}>"
                format_center ""
                format_right  ""
                format_space  ""

                pipe_netspeed_format     "{output}"
                pipe_netspeed_rendermode "static"
                pipe_uptime_format       "{output}"
                pipe_uptime_rendermode   "static"
            }
        }
    }
}
EOF

# -f flushes the capture after every write. Without it the poll loop below
# reads a stale file and only sees the rendered values once the process exits,
# which makes the whole test a coin flip.
#
# zellij refuses to start without a tty, and its socket (TMPDIR), permission
# cache (XDG_CACHE_HOME) and data dir (XDG_DATA_HOME) must not touch a
# developer's live session. Isolating the data dir also makes every run a
# first run, so a developer's machine behaves like CI's clean HOME instead of
# hiding first-run-only screens.
TMPDIR="$WORK" XDG_CACHE_HOME="$WORK/cache" XDG_DATA_HOME="$WORK/data" \
  script -qfc "'$ZELLIJ_BIN' --config '$WORK/config.kdl' --session '$SESSION' --new-session-with-layout '$WORK/layout.kdl'" \
  "$WORK/screen.raw" >/dev/null 2>&1 &
zellij_pty=$!

# Generate traffic so the rate is non-zero on an otherwise idle machine.
( for _ in $(seq 1 "$DEADLINE_SECS"); do
    cat /proc/net/dev >/dev/null 2>&1
    ping -c 1 -W 1 127.0.0.1 >/dev/null 2>&1 || true
    sleep 1
  done ) &
traffic=$!

last=""
found=""
died=""
for _ in $(seq 1 $((DEADLINE_SECS * 4))); do
  sleep 0.25
  # A session that exits early is a different failure from a silent plugin,
  # and waiting out the deadline for it just delays the same diagnosis.
  if ! kill -0 "$zellij_pty" 2>/dev/null; then
    died=1
    break
  fi
  [ -f "$WORK/screen.raw" ] || continue
  # Strip CSI and OSC sequences before matching.
  rendered="$(sed -e 's/\x1b\[[0-9;?]*[ -/]*[@-~]//g' -e 's/\x1b\][^\x07\x1b]*\(\x07\|\x1b\\\)//g' \
                  "$WORK/screen.raw" | tr -d '\r' | grep -o 'NS<[^>]*>UP<[^>]*>' | tail -1 || true)"
  [ -n "$rendered" ] || continue
  last="$rendered"
  netspeed="${rendered#NS<}"; netspeed="${netspeed%%>*}"
  uptime="${rendered#*UP<}"; uptime="${uptime%%>*}"
  # "-" is correct on the first sample: a rate needs two of them.
  if [ -n "$netspeed" ] && [ -n "$uptime" ] && [ "$netspeed" != "-" ] && [ "$uptime" != "-" ]; then
    found="$rendered"
    break
  fi
done

kill "$zellij_pty" "$traffic" 2>/dev/null || true
wait "$zellij_pty" "$traffic" 2>/dev/null || true

if [ -z "$found" ]; then
  if [ -n "$died" ]; then
    printf 'e2e: the zellij session exited before producing a sample\n' >&2
  fi
  printf 'e2e: no live sample within %ss\n' "$DEADLINE_SECS" >&2
  printf 'e2e: last rendered: %s\n' "${last:-<nothing>}" >&2
  printf 'e2e: an empty widget means the plugin published nothing at all;\n' >&2
  printf 'e2e: "-" means it published but had no comparable sample;\n' >&2
  printf 'e2e: nothing at all means the bar never rendered (plugin load or\n' >&2
  printf 'e2e: permission failure, which looks identical from the outside).\n' >&2
  # Without these a CI failure is undiagnosable: the screen capture alone
  # cannot distinguish a silent plugin from a session that never started.
  # Unfiltered on purpose -- a filter that guesses wrong prints nothing and
  # costs another CI round trip.
  log="$WORK/zellij-$(id -u)/zellij-log/zellij.log"
  if [ -f "$log" ]; then
    printf '\ne2e: --- zellij.log (tail) ---\n' >&2
    tail -40 "$log" >&2
  else
    printf '\ne2e: no zellij log at %s\n' "$log" >&2
    ls -la "$WORK" >&2 2>/dev/null || true
  fi
  strip() {
    sed -e 's/\x1b\[[0-9;?]*[ -/]*[@-~]//g' -e 's/\x1b\][^\x07\x1b]*\(\x07\|\x1b\\\)//g' \
        "$WORK/screen.raw" 2>/dev/null | tr -d '\r'
  }
  # The head matters as much as the tail: a config or startup error is printed
  # once, at the top, and then scrolled away by the redraw.
  printf '\ne2e: --- captured screen (first 1200 bytes) ---\n' >&2
  strip | head -c 1200 >&2
  printf '\ne2e: --- captured screen (last 400 bytes) ---\n' >&2
  strip | tail -c 400 >&2
  printf '\n' >&2
  exit 1
fi

case "$found" in
  *"/s"*) ;;
  *) die "netspeed should carry a rate unit: $found" ;;
esac

printf 'e2e: ok — %s\n' "$found"
