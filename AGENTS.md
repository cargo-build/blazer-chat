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

**Verification:** `just gate` is the minimum before landing. Heavy commands
(> ~1–2 GiB) run in a task-owned transient systemd user unit and coordinate
through the shared laptop queue.

**Rust skills:** before writing or refactoring any Rust code, read the
Rust authoring skills first — `~/.agents/skills/rust-best-practices/SKILL.md`
(BeautifulRust) — and keep this repository's stack, lint and behavior
contracts over generic style advice.

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

**GPUI pin:** the published `gpui` crate starts with `Application::new()`;
Zed `main` splits `gpui`/`gpui_platform` and starts differently. Never mix
the two.
