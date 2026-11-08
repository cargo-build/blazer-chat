# blazer-chat

A Rocket.Chat-compatible desktop client: native Wayland interface, io_uring
networking. License: Apache-2.0.

**Stack:** Rust 2024. compio (io_uring runtime) owns I/O on one executor
thread; tako (`tako-rs`, compio feature) owns HTTP/WebSocket transport; GPUI
(Zed's GPU-accelerated UI framework, published crate) owns the window and the
render loop. The layers exchange messages through `async-channel` — no nested
executors. The GUI is a native Wayland surface, never XWayland.

## Layout

```
crates/blazer-chat/
  src/main.rs          entry point: compio thread + GPUI loop, channels across
  src/settings.rs      AppSettings <- public.toml (server url; no secrets)
  src/http.rs          transport layer: HTTP/1.1 on compio behind `Transport`
  src/rc/              protocol layer: Rocket.Chat REST client (login, rooms,
                       history, send), typed domain models, private wire shapes
  src/app.rs           application layer: command/event loop, pure ChatState
                       fold, redb store wiring, unix control socket
  src/store.rs         local persistence (redb): resumable session + rooms
  src/ui/              interface layer: window (mod), chat view (root),
                       composer vendored from gpui examples (input)
dev/docker-compose.yml local Rocket.Chat + mongo dev server
```

## Development

Nix flake devShell (`direnv allow` or `nix develop -c`), operator rustup
toolchain.

- `just build` / `just check` / `just test`
- `just run` — launches the Wayland window
- `just gate` — formatting, clippy `-D warnings`, tests; the minimum before a
  change lands

A local server for live testing: `docker compose -f dev/docker-compose.yml up
-d`, then `export RCHAT_USER=... RCHAT_PASSWORD=...` and point `public.toml`
at `http://127.0.0.1:3000`.

## API contract

The Rocket.Chat REST subset this client consumes is pinned as an
OpenAPI 3.0 document: [`docs/api/openapi.json`](docs/api/openapi.json).
Open the Swagger UI by double-clicking
[`docs/api/index.html`](docs/api/index.html) (the spec is inlined; the
page needs network access only for the swagger-ui CDN assets), or serve
it with `just swagger` at http://localhost:8321.

`just api-check` (part of `just gate`) fails when client code calls an
endpoint missing from the spec, or when the Swagger page drifts from
`openapi.json`.
