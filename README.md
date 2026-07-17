# zj-sysinfo

A background [zellij](https://zellij.dev) WASM plugin that feeds network
speed and load averages to [zjstatus](https://github.com/dj95/zjstatus)
pipe widgets — with **zero process spawning**.

## Why

The usual way to show net speed / load in zjstatus is `command_*` modules
that fork `bash -c` pipelines every 1–2 seconds **per tab**. On a busy
session that is hundreds of execs per second; under memory pressure those
fire-and-forget children can pile up (on the author's machine: ~2,200 live
bash processes and a 10-minute OOM storm). zj-sysinfo replaces all of it
with one plugin instance per session that reads `/proc` directly through
the WASI host filesystem and broadcasts ready-made strings to every
zjstatus instance.

- `pipe_netspeed` — `D: 1.2 MB/s U: 340 KB/s` (default-route interface,
  auto-detected from /proc/net/route, with fallback to the first non-lo
  interface with traffic)
- `pipe_uptime` — `0.52 0.48` (load1 load5 from /proc/loadavg)
- 2-second refresh, counter-wrap and interface-switch safe, never panics
  (failures render `-`)

Linux only (reads /proc). On other systems it loads and shows `-`.

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

The plugin needs `FullHdAccess` (to remount `/host` at `/` and read /proc)
and `MessageAndLaunchOtherPlugins` (to pipe to zjstatus). **A background
plugin has no pane, so zellij never shows it a permission prompt** — grant
it by seeding zellij's permission cache before the first session start.
Append to `~/.cache/zellij/permissions.kdl` (create the file if missing):

```kdl
"/home/<you>/.config/zellij-plugins/zj-sysinfo.wasm" {
    FullHdAccess
    MessageAndLaunchOtherPlugins
}
```

Use the expanded absolute path (no `~`, no `file:` prefix) — it must match
the path zellij resolves for the plugin. With home-manager you can automate
this with a `home.activation` script that appends the entry only when
absent (zellij rewrites this file, so never overwrite it wholesale).

## Provenance

Extracted from [chinrw/shell-config](https://github.com/chinrw/shell-config)
where it was built (spec + incident analysis in `docs/design.md`).

## License

MIT
