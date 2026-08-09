# zj-sysinfo

A background [zellij](https://zellij.dev) WASM plugin that feeds network
speed and load averages to [zjstatus](https://github.com/dj95/zjstatus)
pipe widgets — with **zero process spawning** on Linux.

## Why

The usual way to show net speed / load in zjstatus is `command_*` modules
that fork `bash -c` pipelines every 1–2 seconds **per tab**. On a busy
session that is hundreds of execs per second; under memory pressure those
fire-and-forget children can pile up (on the author's machine: ~2,200 live
bash processes and a 10-minute OOM storm). zj-sysinfo replaces all of it
with one active sampler per session that reads `/proc` directly through
the WASI host filesystem and broadcasts ready-made strings to every
zjstatus instance. Zellij creates a plugin copy for each client, but only
client slot 1 runs zj-sysinfo. Zellij 0.44.3 retains that slot after its
client disconnects and replaces it when the ID is reused; every other copy
stays dormant.

- `pipe_netspeed` — `D: 1.2 MB/s U: 340 KB/s` (default-route interface,
  auto-detected, with fallback to the first non-loopback interface with
  traffic)
- `pipe_uptime` — `0.52 0.48` (load1 load5)
- 2-second refresh, counter-wrap and interface-switch safe, never panics
  (failures render `-`)

## Platforms

The host is detected at runtime (the wasm artifact is platform-neutral).

| | Source | Processes spawned |
|---|---|---|
| Linux | `/proc/net/route`, `/proc/net/dev`, `/proc/loadavg` via WASI | none |
| macOS | `sysctl` + `route` + `netstat` under one `/bin/sh` | 4 per session per 2 s |

The macOS tick is a single `run_command`, but that is four processes: the
shell plus the three tools. A probe that stops answering is abandoned with
doubling backoff (up to ~8 min between attempts) and both widgets fall back
to `-`. Zellij cannot cancel `run_command`; a permanently wedged host can
retain old processes, but replacement attempts are capped at one every
~8 minutes.

macOS has no `/proc` and exposes counters only through syscalls, so a pure
WASI read is impossible there. The fallback is still far below the per-tab
`command_*` polling it replaces, and Linux keeps the zero-fork path.

## Install (nix flake)

```nix
# flake.nix
inputs.zj-sysinfo.url = "github:chinrw/zj-sysinfo";

# home-manager: install the artifact
home.file.".config/zellij-plugins/zj-sysinfo.wasm".source =
  "${inputs.zj-sysinfo.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/zj-sysinfo.wasm";
```

Or build manually: `cargo build --release --target wasm32-wasip1`
(artifact at `target/wasm32-wasip1/release/zj-sysinfo.wasm`), or
`nix build .#default`.

## Configure zellij

Load the plugin once per session (config.kdl):

```kdl
load_plugins {
    "file:~/.config/zellij-plugins/zj-sysinfo.wasm"
}
```

Consume the widgets in your zjstatus layout:

```kdl
format_left  "{mode} {command_git_branch}{pipe_netspeed}"
format_right "{pipe_uptime}{datetime}"

pipe_netspeed_format      "#[fg=blue] {output} "
pipe_netspeed_rendermode  "static"
pipe_uptime_format        "#[fg=blue] {output} "
pipe_uptime_rendermode    "static"
```

## Permissions (important)

The plugin needs `FullHdAccess` (remount `/host` at `/`),
`MessageAndLaunchOtherPlugins` (pipe to zjstatus) and `RunCommands` (the
macOS probe; requested on every platform because the artifact cannot tell
them apart at build time). **A background plugin has no pane, so zellij
never shows it a permission prompt** — seed the permission cache before
the first session start, or both widgets stay silently blank.

zellij resolves the cache through `ProjectDirs`, so it is
`$XDG_CACHE_HOME/zellij/permissions.kdl` on Linux (defaulting to
`~/.cache/zellij`) and
`~/Library/Caches/org.Zellij-Contributors.Zellij/permissions.kdl` on macOS
— darwin does not use XDG:

```kdl
"/home/<you>/.config/zellij-plugins/zj-sysinfo.wasm" {
    FullHdAccess
    MessageAndLaunchOtherPlugins
    RunCommands
}
```

Use the expanded absolute path (no `~`, no `file:` prefix) — it must match
the path zellij resolves for the plugin.

If you automate this from home-manager, **replace the whole block rather
than appending it when absent**: zellij grants from cache only when the
cached entry covers every permission requested, so an entry left over from
an older version blocks the upgrade — invisibly, since the prompt cannot
be shown. Leave the rest of the file alone; zellij owns and rewrites it.

## Provenance

Extracted from [chinrw/shell-config](https://github.com/chinrw/shell-config)
where it was built (spec + incident analysis in `docs/design.md`).

## License

MIT
