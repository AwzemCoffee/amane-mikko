# Amane (`amane-mikko`)

Personal fork of [`MystiaFin/amane`](https://github.com/MystiaFin/amane) (a pure-Rust Wayland shell framework) that powers **Kultsi Shell** (`/home/mikko/Projects/kultsi-shell`) on Linux (primarily NixOS, plus CachyOS) with the Umbriel window manager.

## Repo Boundary

- **Owns:** Reusable framework primitives—Wayland/layer-shell surfaces, immediate-mode widgets, `wgpu`/`vello` rendering, compositor backends, and system D-Bus services.
- **Does not own:** Shell UI, matugen theming, TOML user config, launcher UX, or distro packaging—all of those live in `/home/mikko/Projects/kultsi-shell`.

## Fork Extensions

Amane provides `Notifications`, `Apps`, `Audio`, `Network`, `Bluetooth`, `Battery`, `Cpu`, `Memory`, `Brightness`, `Media`, `LayerWindow`, and `App::ipc`. This fork adds the framework-level pieces Kultsi Shell requires:

1. **Umbriel WM backend (`src/compositor/umbriel.rs`):** Detects `UMBRIEL_SOCKET` and integrates with `noctalia-dev/umbriel` (`umbriel workspaces`, `umbriel subscribe workspaces`, `umbriel msg workspace-switch`).
2. **StatusNotifierItem (SNI) system tray (`src/services/tray.rs`):** Reactive D-Bus `StatusNotifierWatcher` / `StatusNotifierItem` / `DBusMenu` service for tray icons and context menus.
3. **Session lock & suspend bridge (`src/wayland/lock.rs`, `src/services/lock.rs`, `src/services/session.rs`):** `ext-session-lock-v1` lifecycle (`Lock::active`, configurable PAM service) and `systemd-logind` `PrepareForSleep` / `Session.Lock` integration.

## Context Pointers

Read the relevant doc **before** editing framework internals or public primitives:

- **Framework internals (`ARCHITECTURE.md`):** Read before modifying rendering, the `calloop` event loop, `Service` reactivity, or Wayland windowing.
- **Amane API & widget docs (`docs/` and `https://mystiafin.github.io/amane/print.html`):** Read when adding or changing public widgets, services, or styling APIs.
- **Downstream shell (`/home/mikko/Projects/kultsi-shell/AGENTS.md`, `Docs/architecture.md`, and `GLOSSARY.md`):** Read before changing framework seams or verifying requirements needed by Kultsi Shell.
- **Host compositor config (`/etc/nixos/home/umbriel.nix`):** Read when testing Umbriel integration, fractional scaling (`DP-1` 4K @ `1.5x`), or `layer_rule` blur behavior.

## Conventions

- **Library-first consumption:** Kultsi Shell depends on the root `amane` library crate directly via Cargo (and Nix flakes on NixOS), bypassing `amane-cli`'s embedded project generator (`cli/`). Keep the root library API clean, distro-agnostic, and self-contained.
- **Upstream mergeability:** Keep fork additions isolated in dedicated modules (e.g., `src/compositor/umbriel.rs`, `src/services/tray.rs`) with minimal edits to upstream files so rebasing or merging from `MystiaFin/amane` stays low-friction.
- **Build environment:** Standard Cargo build on Linux (e.g., CachyOS); on NixOS, use `nix develop` (from `flake.nix`) for the Rust toolchain and Wayland/Vulkan/PulseAudio/PAM libraries.
- **Frame & latency profiling (`AMANE_FRAMES=1`):** Set `AMANE_FRAMES=1` when diagnosing slow UI opens, dropped frames, or redraw storms; logs per-frame `view`/`draw`/`gpu` ms and `change <Service>` triggers to stderr (`src/timing.rs`).
