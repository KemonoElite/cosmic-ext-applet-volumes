# Volumes

A panel applet for the [COSMIC](https://github.com/pop-os/cosmic-epoch) desktop that lists your
volumes (USB sticks, SD cards, discs, external disks) and lets you open, mount and safely remove
them. Like macOS, where every disk shows up under `/Volumes`.

> **No maintainer, no support, no warranty.** This project is not maintained. There is nobody to
> report bugs to and nobody who will fix them. It is provided as is and you use it entirely at your
> own risk. It mounts, unmounts and powers off storage devices; do not rely on it for anything where
> losing data would matter. See the license for the full disclaimer.

It started as a rewrite of
[cosmic-ext-applet-drives](https://github.com/cosmic-utils/cosmic-ext-applet-drives), restructured
the way [cosmic-applets](https://github.com/pop-os/cosmic-applets) is built: a Cargo workspace with
the same dependency pins, an applet crate that exposes `run()` plus a thin `main.rs`, `i18n.toml` +
Fluent files, a `data/` directory with the desktop entry and icon, and a `justfile` with the same
recipes.

## What it does

- Lists removable drives (USB sticks, SD cards, optical discs, external disks) and any internal
  volume you mounted under `/run/media` or `/media`, **mounted or not**.
- Click a mounted drive to open it in your default file manager (via `xdg-open`, with a Wayland
  activation token so the window gets focus). Click an unmounted drive to mount and open it.
- The eject button unmounts the drive and, if it is removable and nothing else on it is still
  mounted, powers it off (or ejects the disc), i.e. "safe to remove".
- The list updates live when drives are plugged in, unplugged, mounted or unmounted.
- Failures (device busy, permission denied, ...) are shown in the popup with the daemon's own
  message instead of being swallowed.

## What changed from the original

The original was already Rust and already used libcosmic. The rewrite is about how it gets its data
and how it behaves:

| | Original | This version |
|---|---|---|
| Data source | Reads `/proc/mounts` and enumerates udev inside `view()`, i.e. on every redraw, on the UI thread; `unwrap()`s on failure | Talks to UDisks2 over D-Bus from a subscription; the UI only renders state |
| Updates | Only when the view happens to be redrawn | Pushed: reacts to UDisks2 signals (debounced), reconnects if the service restarts |
| Unmount | Runs `umount` synchronously inside `update()` (freezes the UI), ignores the exit status, relies on the user already having permission | UDisks2 `Unmount`/`Eject`/`PowerOff` as async tasks; polkit decides, errors are reported |
| Mount points | Only the `\040` (space) escape of `/proc/mounts` is decoded | Raw bytes from UDisks2, so any path round-trips |
| Open | Hardcoded `cosmic-files` | Default handler via `xdg-open`, with activation token |
| Empty state | A button that ran `cosmic-files ""` | Plain label |
| Dependencies | `drives`, `lfs-core`, `open`, `notify-rust` declared (unused or dead code), plus `procfs`, `udev` (needs libudev) | Only what is used; no libudev |
| Config | Template leftover (`demo: String`) | None needed, so none |
| `.desktop` | `MimeType=inode/directory` on an applet | Removed |

## Building

Needs Rust (see `rust-toolchain.toml`; any recent stable that supports edition 2024 works) and the
usual libcosmic build dependencies (`libxkbcommon-dev`, `libwayland-dev`, `libfontconfig-dev`,
`libfreetype-dev`, ...). Unlike the original it does **not** need `libudev-dev`.

```sh
just build-release          # cargo build --release
sudo just install           # binary, desktop entry, icon  (PREFIX: `just prefix=/usr/local install`)
just uninstall
```

Then add "Volumes" to your panel in COSMIC Settings. `just run` starts it outside the panel (needs
a Wayland session).

At run time it needs the UDisks2 service (`udisks2`, present on practically every desktop distro)
and a running polkit authentication agent for actions that need authorisation.

## Nix

A `flake.nix` is included.

```sh
nix build            # ./result/bin/cosmic-ext-applet-volumes
nix develop          # cargo, clippy, rustfmt, rust-analyzer, just, dbus-daemon
nix flake check      # unit tests + the end-to-end test
```

On NixOS, use the module. It installs the applet and makes sure UDisks2 (which the applet talks to)
is enabled; `services.desktopManager.cosmic` already enables it as a side effect of enabling gvfs, so
this only makes the dependency explicit:

```nix
{
  inputs.volumes.url = "github:YOU/cosmic-ext-applet-volumes"; # or a git+file:// / path: URL
  # ...
  # in your configuration:
  imports = [ inputs.volumes.nixosModules.default ];
  programs.cosmic-ext-applet-volumes.enable = true;
}
```

Or just add `pkgs.cosmic-ext-applet-volumes` after applying `overlays.default`, or use
`packages.<system>.default` directly. Then add "Volumes" to the panel in COSMIC Settings.

Notes:

- The package is built like nixpkgs' `cosmic-applets` (`libcosmicAppHook`, installed through the
  `justfile`) with nixpkgs' Rust; `rust-toolchain.toml` is ignored there.
- The Cargo git dependencies are fetched at their locked revisions (`allowBuiltinFetchGit`), so
  there is no `cargoHash` to keep up to date. That is fine for your own flake but would not be
  accepted into nixpkgs itself.
- No `flake.lock` is included. Run `nix flake lock` (or just `nix build`) to create and pin one.
  Inside a git repository, `git add` the files first; flakes only see tracked files.
- Tests are in `checks`, not in the package build, so `nix build` does not depend on them.

## Tests

```sh
just test        # unit tests + end-to-end test
just check       # clippy, pedantic
```

- Unit tests (`src/udisks.rs`) cover the filtering rules, labels, drive kinds, LUKS handling,
  the safe-removal decision, mount-point decoding and size formatting.
- `tests/e2e.rs` starts a private `dbus-daemon` as a stand-in system bus and an in-process fake
  UDisks2 service, then drives the real watcher and the real mount/unmount/power-off calls
  against it. It needs the `dbus-daemon` binary and is skipped without it. No root, no real disks.

## Layout

```
Cargo.toml                       workspace, dependency pins as in cosmic-applets
flake.nix                        Nix package, overlay, NixOS module, dev shell, checks
cosmic-ext-applet-volumes/
  src/main.rs                    thin binary: tracing + run()
  src/lib.rs                     run() (what a multicall binary like cosmic-applets calls)
  src/app.rs                     cosmic::Application: state, update, popup view
  src/udisks.rs                  UDisks2 types, parsing, filtering, mount/unmount/eject calls
  src/watch.rs                   snapshot + signal-driven refresh, reconnect on failure
  src/localize.rs, i18n.toml     Fluent localisation
  data/                          desktop entry, symbolic icon
  tests/e2e.rs                   end-to-end test against a fake UDisks2
```

### Building it inside cosmic-applets

The crate is written to be a workspace member there. Three edits:

1. root `Cargo.toml`: add `"cosmic-ext-applet-volumes"` to `members`
2. `cosmic-applets/Cargo.toml`: `cosmic-ext-applet-volumes = { path = "../cosmic-ext-applet-volumes" }`
3. `cosmic-applets/src/main.rs`: `"cosmic-ext-applet-volumes" => cosmic_ext_applet_volumes::run(),`

and, to install it with the others, a symlink/desktop entry line in that repo's `justfile`
(and an xdgen entry in `cosmic-applets/build.rs` plus strings in `i18n/*/desktop_entries.ftl` if
you want the desktop entry generated and translated like theirs). It adds no new external crates
to their `Cargo.lock`.

## Not covered

- **Network shares** (SMB/NFS/SFTP mounted through GVFS): UDisks2 does not know about them. The
  original declared a `Network` device type but never produced one either.
- **Unlocking encrypted (LUKS) volumes**: an already unlocked volume is shown and removable;
  locked ones are not listed.
- **Flatpak**: the original shipped a manifest that shelled out via `flatpak-spawn --host`. That is
  gone. A sandboxed build would need `--system-talk-name=org.freedesktop.UDisks2`.
- **AppStream metainfo** is not included: it needs a named developer and contact, and this
  project deliberately has neither.
- **Translations**: Czech, Polish and Swedish carry the original's one string. The new strings
  (`mount`, `drive-service-unavailable`) exist only in English and fall back to it.

## Identity

| | |
|---|---|
| Display name | Volumes |
| Application ID | `local.cosmicext.Volumes` |
| Crate / binary | `cosmic-ext-applet-volumes` |

The application ID is a placeholder in the reserved `local` namespace, chosen so that it does not
claim anyone's real domain or GitHub account. It is **not** the original applet's ID, so the two can
be installed side by side, and a panel layout that referenced the old applet will not pick this one
up automatically: add "Volumes" in COSMIC Settings. If you fork this under a domain you own, change
`APP_ID` in `src/app.rs` and the `appid` variable in the `justfile`, and rename the desktop entry and
icon in `data/` to match.

## License and credit

GPL-3.0-only (see `LICENSE`), which includes its own disclaimer of warranty. The license and the
credit below stay because this work derives from GPL-licensed projects, not because anyone
maintains it: the original applet is cosmic-utils/cosmic-ext-applet-drives, and the structure and
several files follow pop-os/cosmic-applets.
