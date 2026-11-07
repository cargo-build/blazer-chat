# AGENTS.md — rchat-blazar

Nested repository of the wrapper layout: this repo owns its own code and
history; the wrapper around it owns coordination (issues, journals, rules,
evidence). Do not link to or depend on wrapper files from here.

**Stack rule:** compio runtime, tako (`tako-rs`, compio feature) transport,
GPUI interface — one owner per layer, channels across the boundary, no nested
executors. The GUI runs native on Wayland; X11 is a build feature only, no
product surface targets XWayland.

**Version control:** `jj` only — never `git add`/`commit`/`checkout`/`push`
(`git rev-parse` for build scripts is fine). `main` is the integration line.

**Verification:** `just gate` is the minimum before landing, and any
slice touching `ui/` lands only with an in-view compositor screenshot —
socket JSON never proves pixels (the M4 empty-pane regression hid for
four milestones behind green tests). Heavy commands
(> ~1–2 GiB) run in a task-owned transient systemd user unit and coordinate
through the shared laptop queue.

**Rust skills are mandatory — read before the first edit, never after:
`~/.agents/skills/rust-best-practices/SKILL.md` (BeautifulRust) and
`~/.agents/skills/rust-operator-style/SKILL.md` (personal policy) always;
the stack layer skill for the touched code (`gpui-apps` UI, `tako-apps`
transport, the compio runtime reference, `toml-config` for configuration,
`m04-zero-cost` for type/dispatch choices, `clippy-advanced` for lint
profile work); `rust-api-design` when shaping the rc surface,
`rust-code-quality-guide` for conversions and error boundaries,
`rust-port` when porting protocol logic; `rust-review` /
`rust-code-review` before reviewing a diff or module. This repository's
stack, lint and behavior contracts override generic style advice.**

**Required Rust skills — `~/.agents/skills/<name>/SKILL.md`, use by path:**

| Skill | When |
|---|---|
| `rust-best-practices` | always, before writing or refactoring any Rust code |
| `rust-operator-style` | operator policy: configuration contracts, style precedence |
| `rust-api-design` | designing public APIs, crates, traits |
| `rust-code-quality-guide` | conversions, required values, error boundaries, lint exceptions |
| `rust-code-review` | reviewing diffs and PRs |
| `rust-review` | whole-crate or module review |
| `rust-ffi` | C/C++ interop, bindgen, JNI, memory conversion |
| `rust-port` | porting protocol logic from other languages |
| `rust-unsafe` | unsafe code, raw pointers, FFI soundness |
| `rust-skills` | legacy 265-rule bundle — explicit request only |

**Required non-Rust skills — use with these exact paths:**

- Protocol reverse engineering (Rocket.Chat facts):
  `~/.config/opencode/skills/reverse-engineer-anything/SKILL.md`

**GPUI stack (bezel-gpui, zed snapshot f16f96):** gpui and its platform
backend are two crates — `bezel-gpui` (lib name `gpui`) plus
`bezel-gpui-platform` with the `wayland` feature (DECL0002). Bootstrap
ONLY through `Application::with_platform(gpui_platform::current_platform(
false))` — never `gpui_platform::application()`, which attaches a
Reqwest HTTP client that spawns a hidden Tokio runtime (DECL0001: one
owner per layer; all app I/O belongs to compio, and the UI loads no
network images through gpui). Keep `bezel-gpui` and
`bezel-gpui-platform` on the same `+zed.<rev>` version.
