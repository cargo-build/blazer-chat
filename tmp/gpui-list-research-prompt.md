# Research request: gpui 0.2.2 `list` element paints nothing (crates.io snapshot of zed's gpui)

You are researching a UI-framework bug on our behalf. We need facts from
upstream (zed-industries/zed, GitHub issues/PRs/commits, the gpui source
history) — not generic advice. Answer in English, cite concrete
issue/PR/commit numbers and code locations.

## Context

- Crate: `gpui = "0.2.2"` from crates.io — a published snapshot of
  zed-industries/zed's gpui (the repo's `crates/gpui`). Its
  `src/elements/list.rs` is ~1300 lines. Current upstream `main` has a
  heavily rewritten `list.rs` (~3600 lines, scroll-anchor based).
- Platform: Linux, Wayland (niri compositor), Rust 2024 edition, wgpu
  (Vulkan via NVIDIA). The app is a normal gpui desktop client.
- In the SAME window: `uniform_list` paints fine (our room list), and a
  plain `div().overflow_y_scroll()` pane paints fine (our messages).
  Only the `list` element fails.

## Symptom

gpui's `list(state, render_item)` builds rows but paints NOTHING — the
pane stays empty/transparent while sibling elements render. The
`render_item` closure IS invoked by layout (we logged row construction),
so the element lays items out but they never appear on screen.

## Experiments (independent, both failed identically)

### Experiment 1 — `ListAlignment::Bottom` (chat-log style)

```rust
messages_list: ListState::new(count, ListAlignment::Bottom, px(500.))
    .measure_all(),
// every rebuild:
self.messages_list.reset(self.views.len());
self.messages_list.scroll_to_reveal_item(last_index);
// element:
list(self.messages_list.clone(), move |index, _window, cx| {
    // reads an Entity<ChatRoot>, builds the row div (logs confirm
    // this runs), returns .into_any_element()
})
```

Result: rows materialized (closure ran, count correct), pane painted
empty. `ListSizingBehavior` left default (`Auto`).

### Experiment 2 — `ListAlignment::Top`

Same as above but `ListState::new(count, ListAlignment::Top, px(500.))`,
plus `reset(count)` per rebuild and `scroll_to_reveal_item(index)` for
anchoring. Result: identical — empty pane; uniform_list in the same
window kept painting.

### Experiment 3 — minimal repro attempt (did NOT complete)

We tried replacing the whole window render with just
`list(ListState::new(100, Top, px(500.)), |ix,_,_| div().h(px(40.)).child(...)).size_full()`
but the experiment build was botched (macro/dup-symbol error) and
rolled back before running. So the minimal standalone repro is NOT yet
confirmed either way — if you can spot a mistake in how we used the API
below, tell us.

## Source-level observations (gpui 0.2.2 as published)

- `List::paint` iterates `prepaint.layout.item_layouts` and paints each
  inside a `ContentMask{bounds}` — nothing obviously skipping.
- `prepaint` gates item generation on
  `bounds.size.height > padding.top + padding.bottom`.
- `logical_scroll_top()` defaults for `Bottom` to
  `ListOffset { item_ix: items.count, offset_in_item: 0 }` (i.e. past
  the end) until an anchor is set.
- `ListSizingBehavior::Auto` is the default; we never set
  `with_sizing_behavior(Infer)`, and the list was given a real
  height (flex_1/min_h_0 inside panels; the window-level experiment
  used `.size_full()`).
- 0.2.2's `list.rs` uses the OLD layout engine fields
  (`logical_scroll_top: Option<ListOffset>` directly in StateInner),
  while upstream main uses `scroll_anchor: Option<ScrollAnchor>` — the
  whole scroll model was rewritten after 0.2.2.

## Upstream research we already did (verify/extend, don't repeat)

- No issue/PR found matching "list builds rows but paints nothing".
- Adjacent known issues: #51198 (bottom-aligned scrollbar thumb
  disappears; fixed by #51223), #42768 (`scroll_to_reveal_item` fails
  to scroll when layout sizes aren't cached; closed as *not planned*),
  #65623 (autoscroll retry duplicates a11y node ids; open).
- crates.io has NO 0.2.2-successor under the name `gpui`. Newer zed
  gpui ships only in republished lineages: `gpui-pre 0.3.11`
  ("snapshot of zed@f816e1f"), `gpui-unofficial 1.26.x`,
  `gpui-standalone`. `gpui-ce` is a separate fork family
  (longbridge/gpui-component ecosystem).

## Questions (answer these specifically)

1. Which zed commit does the published `gpui 0.2.2` correspond to?
   Was its `list` known-broken at that point in history? Find the
   upstream commit(s)/PR(s) between that snapshot and the scroll-anchor
   rewrite that FIX list painting (e.g. anything touching
   `List::prepaint`/`paint`, `definite_height`, item culling).
2. Is there a known minimal fix if we were to fork/patch 0.2.2's
   `list.rs` locally? Point at the exact function(s) to change.
3. Does `gpui-pre 0.3.11` (or `gpui-unofficial`) contain the fixed
   list? Any known regressions/issues with that lineage on Wayland?
   What is the lib target name inside `gpui-pre` (`gpui` or
   `gpui_pre`)? Does it still expose the same public API surface we
   use: `list`, `ListState::new/reset/splice/scroll_to_reveal_item`,
   `uniform_list`, `actions!`, `KeyBinding::new`,
   `ElementInputHandler`/`EntityInputHandler` (text input internals:
   `ShapedLine`, `TextRun`, `UTF16Selection`, `UnderlineStyle`),
   `Application::new()`, `Window::set_rem_size`, `tooltip`,
   `ClipboardItem`?
4. Any hypothesis for why rows lay out but never paint in 0.2.2 while
   `uniform_list` (separate element file) paints — e.g. a known
   interaction with `ListSizingBehavior::Auto` + `min_h_0` + flex
   parents giving the list a zero-height definite space, or a content
   mask bug? If it's a sizing/height issue, tell us the exact
   diagnostic to run (e.g. wrap in a fixed-height div of N px and
   retry) so we can falsify quickly.

## Deliverable

- A short verdict: known bug + fixing commit, or our misuse of the API
  (with the corrected usage), or "needs the newer lineage".
- Concrete next step for us: minimal patch for 0.2.2, or the smallest
  viable migration target (`gpui-pre` vs `gpui-ce`) with the specific
  API breaks we listed above.
