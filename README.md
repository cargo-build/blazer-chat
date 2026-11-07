# rchat-blazar

A Rocket.Chat-compatible desktop client: native Wayland interface, io_uring
networking. License: Apache-2.0.

**Stack:** Rust 2024. compio (io_uring runtime) owns I/O on one executor
thread; tako (`tako-rs`, compio feature) owns HTTP/WebSocket transport; GPUI
(Zed's GPU-accelerated UI framework, published crate) owns the window and the
render loop. The layers exchange messages through `async-channel` — no nested
executors. The GUI is a native Wayland surface, never XWayland.

## Layout

```
crates/rchat-blazar/   the application
  src/main.rs          entry point
  src/service.rs       compio executor: tako endpoint + client fetches
  src/ui.rs            GPUI window fed by compio events
```

## Development

Nix flake devShell (`direnv allow` or `nix develop -c`), operator rustup
toolchain.

- `just build` / `just check` / `just test`
- `just run` — launches the Wayland window
- `just gate` — formatting, clippy `-D warnings`, tests; the minimum before a
  change lands
