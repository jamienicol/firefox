# Plan: Parallelise non-overlapping backdrop filters on a tile

## 1. Goal

Today, N backdrop filters on the same picture-cache tile are serialised: each filter's chain is gated on the previous filter's `BackdropRender` having been issued into the dynamic-content task. With the existing render-pass batcher, that means roughly N × (filter-chain length) passes, plus N resolve blits, executed strictly in series — the dominant cost on Mali/PowerVR/Apple-Silicon.

When two backdrop filters on the same tile don't visually interact, CSS doesn't require ordering between them — they could both read the same input and write disjoint output regions. We exploit this: group filters into **phases**, where every filter in a phase shares one resolve source. Chains in a phase become independent in the task graph; the existing pass batcher folds them together so per-tile passes scale with the *deepest* chain in the phase, not the total.

**Scope of the win — read this before §3.** A backdrop-filter has exactly **one** sub-graph, **one** resolve task, and **one** filter chain, regardless of how many tiles it covers. Its `ResolveOp.src_task_ids` carries one source *per covered tile*, and `pop_surface` adds a dependency from the single resolve task to *each* tile's chosen source. Phase assignment is per-tile, but the coalescing benefit is therefore **per-filter and global**: filter B's chain coalesces with filter A's only if B reads a pre-A source on **every** tile the two share. If B is same-phase as A on tile T but a *later* phase on tile U (B reads U's post-A task), then B's single resolve task transitively depends on A's output via U, and B's whole chain serialises after A's — *including on T*. This is still **correct** (each tile reads the right source), but it is **not strictly "no regression."** On T, B now reads the early source `D_0^T` while B's chain runs late (gated by U), so `D_0^T`'s lifetime is extended versus today (where B read `D_1^T`, freeable once A consumed it); and T's reused phase task `D_{P+1}^T` now also depends on `O_B`, which can delay T's combined task. So render-pass coalescing collapses back toward serial *and* task lifetimes / peak memory / scheduling can differ from today — usually marginally, but not provably zero. The fusion speedup only fully materialises for filters with **compatible phase assignments across all their source tiles**: single-tile filters (the dominant shape in the `bf-perf.html` pills) and multi-tile filters that happen to phase uniformly.

To recover a strict no-regression guarantee for the mixed case, the implementation can **force any non-uniform multi-tile filter to `ForceSerial` on all its tiles** — making it behave byte-for-byte like today (post-predecessor source everywhere, no early-source lifetime extension). This needs a per-filter reconciliation pass over its tile set after stamping, so it is listed as an optional refinement (§11) rather than baseline; without it, the mixed case is correct and close-to-today but not identical.

**"Non-uniform" means non-uniform *source class*, not unequal `Phase(p)` integers.** Per-tile phase IDs are independent local counters: an earlier `ForceSerial`/overlap barrier on tile T can bump T's counter so that `Phase(1)` on T means "fused follower reading A's pre-source" while `Phase(1)` on tile U means "fresh phase reading the post-A task." Same integer, opposite coalescing relationship. The property that actually governs global coalescing (and the only correct uniformity test) is the **relationship to predecessors / chosen resolve-source class** on each tile: classify the filter per tile as *fused* (it reuses an existing phase's pre-predecessor source — a §7.5 "subsequent of phase" / 3b outcome) versus *fresh* (it reads the current post-predecessor content — a 3a-with-predecessors or `ForceSerial` outcome). A filter is **uniform** iff that classification is the same across every tile it covers; comparing raw `Phase(p)` values would mis-classify the example above. This is exactly the definition the §12 `truly_fused_count` telemetry uses ("only pre-predecessor sources"), and reconciliation must use the same one.

## 2. When is it safe to share a resolve source?

Let `D_P` be the dynamic-content task that the resolves of all filters in phase P read from, and `D_{P+1}` be the task that those filters' `BackdropRender` quads (and any intermediate prims) draw into. Two filters A and B can sit in the same phase iff B's capture (as it would be observed today, i.e. *after* A) equals what's already in `D_P`. Equivalently:

- **(R1)** B's capture rect doesn't intersect any other phase-member's capture rect (their `BackdropRender` quads write into `D_{P+1}` and B must not see them).
- **(R2)** B's capture rect doesn't intersect the union of bounds of any *non-filter* primitive drawn between the start of the phase and B (those prims write into `D_{P+1}` and B must not see them).

Prims drawn **before the first filter of phase P** are baked into `D_P` and are exactly what we want B to read — they're a non-issue.

Because `clip_leaf_id` is shared between `BackdropCapture` and `BackdropRender` (`scene_building.rs:3600`), a filter's capture rect equals its render rect, so (R1) is symmetric in A and B.

When B violates (R1) or (R2), it starts a new phase. `D_{P+1}` then becomes phase P+1's source (same physical dynamic-content surface, same render task in fact — just like today's T_new naturally becomes the next filter's T_old).

## 3. Per-tile phasing

Phases are **per tile**, not per filter, because the same backdrop-filter picture may be on tile T1 with no interferers but on tile T2 with overlapping siblings. The existing splice already iterates parent tiles inside the `Tiled` arm of `pop_surface` (`surface.rs:649–721`); we extend that iteration to consult a per-tile phase mapping.

For a tile with M visible backdrop filters today, the new structure is:

```
D_0 ── all "background" prims up to first filter
  │   (forks)
  ├─► [resolve A: D_0 → R_A] ─► … A's chain … ─► O_A ─┐
  ├─► [resolve B: D_0 → R_B] ─► … B's chain … ─► O_B ─┤   (these are
  ├─► [resolve C: D_0 → R_C] ─► … C's chain … ─► O_C ─┤    siblings —
  ⋮                                                    │    same level)
D_1 ── BackdropRender(A,B,C…) + intermediate prims  ◄─┘
        (in display order, into D_1's command buffer)
  │
  ⋮ (next phase, same shape)
  ▼
TileComposite → static tile slot
```

The phase boundary creates the only inter-phase dependency. The `add_dependency(D_{P+1}, D_P)` edge stays (so writes are well-ordered against reads inside `D_P`); the resolves read `D_P`; chains feed into `D_{P+1}`. The chains in a phase are independent **on this tile** — but recall from §1 that each chain is a single global task whose resolve depends on *all* its tiles' sources. So a chain only actually coalesces if it sits in a same-or-earlier phase on *every* tile it touches; a filter that is later-phase on some other tile carries that tile's post-predecessor dependency back into this tile's diagram and serialises anyway. For single-tile filters (and uniformly-phased multi-tile ones) the diagram above is exactly what the task graph produces.

## 4. Findings from investigation (the "unknowns")

Before committing to the design, the following were verified by reading the code:

### 4.1 Pass batcher *does* fuse sibling chains (the load-bearing assumption)

`render_task_graph.rs:407` (`RenderTaskGraphBuilder::end_frame`) groups tasks into render passes by **topological depth**: at `:455-490` a single `TopologicalSort::pop_all()` per pass extracts all tasks with zero remaining dependencies and assigns them `render_on = PassId(pass_count)` (`:483`). Tasks at the same depth — A's blur-pass-1 and B's blur-pass-1 with no A→B edge — *automatically* share a `RenderPass`. **This is the load-bearing guarantee** we need.

Within a render pass, packing into one *physical* render target is a secondary win, gated by `lifetime_group` equality (`:178, :614`) plus size/shared-surface eligibility. `lifetime_group` is the task's `free_after`, computed by walking each task's children top-down through the passes (`:1029`). For two equal-length chains running side-by-side, A's pass-i and B's pass-i both have their last consumer at depth i+1 → identical `free_after` → same `lifetime_group` → they can pack into one target.

For **unequal-length chains** (e.g. A is `blur(8px)` with 3 downscale + V/H passes, B is `brightness(120%)` with 1 pass), depths differ — but the shorter chain finishes early. The render-pass count for the phase equals the **deepest** chain's length, not the sum. Lifetime constraints may force unequal-depth tasks into separate physical targets within a pass, but they still share the same render pass — which is the dominant Mali/PowerVR/Apple-Silicon win (one tile-flush/load cycle per pass, regardless of how many subpasses).

**Verdict**: removing the artificial A→B dep is sufficient. Same-pass coalescing is guaranteed; same-target coalescing is a bonus.

### 4.2 Visibility loop hook is `update_prim_dependencies` (`tile_cache/mod.rs:2176`)

This is the only caller from `visibility.rs:485`. It runs once per (prim_instance × root tile cache) for every visible leaf prim *and* every non-passthrough Picture (`visibility.rs:386–430`). The traversal walks display-list order via depth-first recursion through Picture clusters, so prims are observed in draw order.

`pic_coverage_rect` (line 2207–2256) is already mapped into the **root tile cache's picture space** — the exact space `sub_graphs.coverage_rect` lives in (`tile_cache/mod.rs:2662`). The mapping iterates the surface stack and applies each parent surface's `composite_mode.get_coverage(...)`, so filter inflation is already baked in. **No new space mappers** are needed for phase computation.

`on_picture_surface` (line 2206: `prim_surface_index == self.surface_index`) is the clean filter for "draws directly into the dynamic-content task":
- Top-level leaf prims of the tile cache: `true`.
- Children of a passthrough stacking-context Picture: `true` (their `prim_surface_index` is the tile cache's, because the passthrough adds no surface).
- Children of a non-passthrough sub-surface (e.g. inside a backdrop-filter `IntermediateSurface` chain — the filter Pictures, the `BackdropCapture` itself): `false`. These do not paint into the dynamic-content task and must be excluded from `phase_prim_union`.
- The non-passthrough sub-surface's own Picture prim (visited *after* its recursion): `true`. It DOES contribute to the dynamic-content task via its composited output, except when it's a backdrop-filter `IntermediateSurface`.

### 4.3 The IntermediateSurface picture must be filtered out

A hoisted `IntermediateSurface` (the backdrop-filter sub-graph root) appears as a direct child of the tile cache (`scene_building.rs:3674` — hoisted to the nearest non-`WRAPS_BACKDROP_FILTER` ancestor, which is the tile cache root in the mastodon test case for both header and tweet actions). Its `update_prim_dependencies` is called with `on_picture_surface == true`. But it doesn't actually paint into the dynamic-content task — its output is sampled by the `BackdropRender` quad later.

Identify it via `pictures[pic_index.0].flags.contains(PictureFlags::IS_SUB_GRAPH)` (`picture.rs:517–526`, flag set at `scene_building.rs:339`).

### 4.4 `register_resolve_source` Tiled-not-supported is about the sub-graph, not the parent

The panic in `surface.rs:477–478` triggers when the **top** of the builder stack is Tiled. Backdrop-filter sub-graphs are always pushed as `Simple` surfaces — the panic guards against ever pushing a *tiled* sub-graph, which isn't a thing. The *parent* of the sub-graph (one entry below on the stack) is the Tiled tile cache; that's handled in `pop_surface`'s Tiled arm (`surface.rs:649`).

`resolve_source` itself (`surface.rs:486`) stores the **destination** of the resolve blit (the `IS_RESOLVE_TARGET` task inside the sub-graph). The blit's **source** comes from iterating the parent's tile descriptors inside `pop_surface` and reading each `descriptor.current_task_id` (`:655, :672`). This is exactly the slot we redirect in the phase-aware version: instead of reading `current_task_id`, read the per-tile `phase_source_task`.

### 4.5 `RenderTaskLocation::Existing` and the dynamic-content storage model

`render_task.rs:108–113`: `Existing { parent_task_id, size }` means "same allocation as an existing task deeper in the dependency graph." The `duplicate(cmd_buffer_index)` call (`surface.rs:669`) plus this location is exactly the mechanism that lets `T_old` and `T_new` share one physical render target today; the same trick is what lets `D_0, D_1, ..., D_N` all alias one physical dynamic-content texture under phase fusion.

### 4.6 `SurfaceTileDescriptor` construction sites

`surface.rs:305–314`. Built in three places:
- `picture.rs:2015` — picture cache tile with sub-graphs (extra `TileComposite` task path).
- `picture.rs:2050` — picture cache tile without sub-graphs.
- `surface.rs:704` — the *new* descriptor cloned by `pop_surface` during the existing splice (replacement-task path).

The fields are simple: `current_task_id`, `composite_task_id`, `dirty_rect`. Phase state can be added here without rippling anywhere else. Per-tile filter→phase mapping is computed in visibility and lives in `CachedSurface`; the descriptor reads from there at `picture.rs:2015/:2050` to seed itself.

### 4.7 The mastodon perf case: single-rect `phase_prim_union` floods

Concrete walk-through on the mastodon clone (`bf-perf.html`): each tweet has 3 `.tweet-action` pills with `backdrop-filter: blur(8px)`. A 512px tile holds ~5 tweet rows.

In draw order: `[tweet1 avatar+text] [tweet1 TA1.BR][TA1 text] [TA2.BR][TA2 text] [TA3.BR][TA3 text] [tweet2 avatar+text] [tweet2 TA1.BR] ...`

With a **single** `phase_prim_union` rect:
- Phase 0 starts at tweet1.TA1. TA2, TA3 join (no overlap).
- tweet2's avatar prim accumulates into `phase_prim_union`. Now the union spans tweet1 row + tweet2 avatar.
- tweet2.TA1's bounding rect lies inside that union → forces new phase.

Result: a phase per tweet row (3 filters), not a phase for the whole tile (15). Still a 3× pass-count reduction, but the available win is more like 15×.

With a **list of disjoint rects** (cap K, merge by intersection on insert), tweet2 avatar's rect is a single small rect on the left of tweet2. tweet2.TA1 is on the right of tweet2. No intersection. Joins phase 0. We get the whole tile in one phase.

**Decision**: ship the list variant from the start. A small `SmallVec<[PictureRect; 8]>` with a "merge overlapping on insert" policy is cheap enough that the simple-rect variant has no advantage to land first.

## 5. Per-tile state

Three distinct cases need distinct semantics at `pop_surface` time. Rather than overload `None` / `PhaseId::MAX` for all of them, use an explicit enum:

```rust
type PhaseId = u16;            // wider than u8 — pathological grids can exceed 255 phases on a tile

#[derive(Copy, Clone)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub enum PhaseAssignment {
    /// Filter participates in normal phase fusion.
    Phase(PhaseId),
    /// Filter cannot fuse — must be serialised as its own phase.
    /// Sources: 3D plane-split context (§6.2), phase-id overflow (§6.1).
    ForceSerial,
}
```

**Serialisation note**: capture/replay derives are needed only for new types that end up inside a struct that itself carries the derives. Which is which:

- `SurfaceTileDescriptor` **is** derived (`surface.rs:303-304`). Its new fields — `phase_source_task: Option<RenderTaskId>`, `current_phase: Option<PhaseId>`, `phase_map: Vec<(PictureIndex, PhaseAssignment)>` — therefore require derived components. In practice this means **`PhaseAssignment`** must carry the `cfg_attr(feature = "capture"/"replay", derive(Serialize/Deserialize))` pair as shown above. `PhaseId` is just `type PhaseId = u16;` — a type alias can't and doesn't need to carry derives; the underlying `u16` already implements both traits. `RenderTaskId`, `PictureIndex`, `Vec`, and `Option` already round-trip. (`Vec` is deliberately chosen over `FastHashMap` here: per-tile filter counts are tiny, so a linear scan beats hashing and the descriptor stays cheap to serialise — see §11.)
- `CachedSurface` is **not** derived (`cached_surface.rs:25` has no `cfg_attr`; only the sibling `CachedSurfaceDescriptor` at `:421-423` is). So `PhaseBuildState`, the rect `SmallVec`s, and the `SubGraphEntry` struct (which lives inside `CachedSurface.sub_graphs`) do **not** need the derives. Leave them off to avoid implying they're capture-stable when they aren't.

Rule of thumb: add the `Serialize`/`Deserialize` derives only where the containing struct already has them. The compile error if you get it wrong is loud and immediate.

Add to `CachedSurface` (`invalidation/cached_surface.rs:25`):

```rust
struct PhaseBuildState {               // reset by pre_update each frame
    in_phase: bool,
    current_phase_id: PhaseId,
    current_phase_filter_rects: SmallVec<[PictureRect; 8]>,
    current_phase_prim_rects: SmallVec<[PictureRect; 8]>, // disjoint after merge-on-insert
    /// Set once any filter on this tile was forced serial — used by step-1
    /// instrumentation assertions and for tile-level diagnostics.
    saw_force_serial: bool,
    /// Sticky latch: once the PhaseId space is exhausted on this tile, every
    /// subsequent filter is forced serial regardless of overlap state.
    /// Without this, after an overflow event clears the rect lists, the next
    /// non-overlapping filter would short-circuit the bump path and silently
    /// reuse Phase(MAX). See §6.1.
    force_serial_only: bool,
}
pub phase_build: PhaseBuildState,
```

Extend the per-tile `sub_graphs` entry to carry `pic_index` and `phase: PhaseAssignment`:

```rust
pub struct SubGraphEntry {
    pub coverage_rect: PictureRect,
    pub surface_stack: Vec<(PictureCompositeMode, SurfaceIndex)>,
    pub pic_index: PictureIndex,
    pub phase: PhaseAssignment,
}
pub sub_graphs: Vec<SubGraphEntry>;
```

All existing consumers of the old tuple shape must be updated. The non-trivial one is `picture.rs:1934` (the dirty-content rect expansion loop in `prepare_for_render`) — currently `for (sub_graph_rect, surface_stack) in &tile.cached_surface.sub_graphs`. Mechanical rename to `for entry in &tile.cached_surface.sub_graphs { let sub_graph_rect = entry.coverage_rect; let surface_stack = &entry.surface_stack; … }`. Semantics unchanged. The other touches (`picture.rs:1929` is_empty, `cached_surface.rs:35/50/76` defn/new/clear, `tile_cache/mod.rs:2662` push) are trivial.

**Initialisation & reset checklist**: `CachedSurface::new` (`cached_surface.rs:38`) must construct `phase_build` with `in_phase: false`, `current_phase_id: 0`, both rect SmallVecs empty, `saw_force_serial: false`, `force_serial_only: false`. `CachedSurface::pre_update` (`cached_surface.rs:60`) already calls `self.sub_graphs.clear()` at `:76`; add a `self.phase_build.reset()` call (or inline-reset the same six fields) at the same site. Missing the reset would carry per-frame state across frames and silently corrupt fusion decisions.

Add to `SurfaceTileDescriptor` (`surface.rs:305`):

```rust
pub phase_source_task: Option<RenderTaskId>, // D_P for the current normal phase
pub current_phase: Option<PhaseId>,          // None before the first non-serial filter pops on this tile
pub phase_map: Vec<(PictureIndex, PhaseAssignment)>, // populated from cached_surface.sub_graphs
```

`phase_map` is per-tile, cloned at `picture.rs:2015`/`:2050` from the `sub_graphs` we computed in visibility. Only entries for filters that *touch this tile* are present, and a tile carries a handful of filters at most, so a flat `Vec` with linear lookup is both cheaper to clone/serialise than a `FastHashMap` and avoids implying per-tile hashing cost (see the memory note in §11). `pop_surface` distinguishes three cases at lookup time (see §7.5):
- **not found** — filter does not touch this tile (no `sub_graphs` entry was inserted).
- **`ForceSerial`** — filter touches this tile but cannot fuse.
- **`Phase(p)`** — normal phase fusion candidate.

### 5.1 Simple (non-tiled) parent surfaces — out of scope, and why phantom assignments are safe

Phase fusion as designed applies **only when the parent surface in `pop_surface` is `Tiled`** (the picture-cache case, where the perf problem lives). The `Simple` and `Chained` arms (`surface.rs:723-782`) have no `CachedSurface`, no `sub_graphs`, no `phase_map` — and no obvious place to put them, since the parent isn't a tile cache. For the initial implementation:

- The `Simple` / `Chained` arms run today's per-filter splice unconditionally.
- A backdrop-filter whose hoisting destination is a Simple non-tiled parent (e.g. inside a nested stacking context that establishes its own surface via `opacity`, a transform, etc.) gets no fusion benefit but remains correct.
- No additions to `CommandBufferBuilderKind::Simple` for this phase of the work.

**The "phantom phase assignment" subtlety (verified, harmless).** The phase computation in §6 runs in visibility against the *root tile cache* and is **independent of where each filter's `IntermediateSurface` is hoisted**. The `sub_graphs.push` at `tile_cache/mod.rs:2658-2664` is gated *only* on `!pic_coverage_rect.is_empty()` — **not** on `on_picture_surface`. So a backdrop-filter nested inside a Simple-surface SC still produces a `sub_graphs` entry (and therefore a `Phase(p)` or `ForceSerial` assignment) on the root tile cache's tiles, with its coverage mapped into tile-cache picture space through the surface stack (`tile_cache/mod.rs:2217-2255`).

That filter then pops against its Simple parent in `pop_surface`, hits the §5.1 fall-through, and **ignores `phase_map`**. The assignment it received is a phantom — it never honours it. This is harmless to correctness for three independent reasons:

1. The Simple SC's *own* picture prim (the one that composites the SC's result, including the nested backdrop-filter's output, into the tile's dynamic content) is visited with `on_picture_surface == true` and is **not** `IS_SUB_GRAPH`, so its coverage is accumulated into `phase_prim_rects` (the `_ if on_picture_surface && s.in_phase` arm of §6). Any *fusing* sibling on the tile that would actually see the nested filter's composited output therefore fails the R2 interference test and splits.
2. The phantom filter's own `capture_rect` is pushed into `current_phase_filter_rects` when its `BackdropRender` is observed, so later filters' R1 test still accounts for it.
3. It simply forfeits its own fusion speedup; it does not corrupt any other filter's `phase_source_task` snapshot, which is per-tile descriptor state mutated only by filters that actually pop against the Tiled parent.

The only observable cost is telemetry: such filters inflate `would_fuse_count` in §12 without delivering a real fusion. The step-1 instrumentation should subtract them (a phantom is detectable as a filter whose `BackdropRender` was observed with `prim_surface_index != tile_cache.surface_index`).

A follow-up could extend real fusion to the Simple parent case by attaching the same phase state to `CommandBufferBuilder` itself (next to `establishes_sub_graph`) and computing it during visibility against the parent surface's prim list. Not in initial scope.

## 6. Phase-computation algorithm (now precise)

**Scope clarifier**: this logic lives inside `update_prim_dependencies` and is **additive** — it runs alongside the function's existing per-tile dependency / invalidation book-keeping. Nothing here replaces or short-circuits any of that work. The pseudo-code below describes only the new "update `phase_build` and stamp the phase assignment onto `sub_graphs` entries" logic; existing per-tile loops continue unchanged.

**Two distinct code sites, one unified state machine.** The pseudo-code presents a single `match prim.kind` for readability, but the two arms land in *different* places in the function:
- The `BackdropRender` arm goes in the existing `PrimitiveKind::BackdropRender { pic_index, .. }` block at `tile_cache/mod.rs:2637-2664`, right where `required_sub_graphs.insert` and `sub_graphs.push` already happen (its own per-tile `for y { for x }` loop at `:2658-2664`).
- The "normal prim" accumulation arm goes in the general per-tile dependency loop at `tile_cache/mod.rs:2859-2871`.

This is sound because `phase_build` is **per-tile state on `cached_surface`** and `update_prim_dependencies` is invoked **once per prim in draw order**. The state machine therefore advances correctly across calls regardless of which of the two sites touches it — each call mutates only the tiles its prim covers. The "single loop" framing in the pseudo-code is a simplification; do not expect to find one literal loop handling both.

**Tile-clipped interference rects.** `pic_coverage_rect` and `capture_rect` reach the per-tile loop as the *full* primitive coverage in tile-cache picture space — the same full rect is pushed to every covered tile (`:2662`). For the phase/interference tests (R1, R2) we instead clip to the **current tile's** picture-space rect before pushing or testing: `let r = capture_rect.intersection(&tile_pic_rect)` (and likewise for prim rects). This makes phasing per-tile-precise: two filters that overlap *outside* tile T but are disjoint *inside* T then correctly fuse on T. The unclipped `pic_coverage_rect` is still what gets stored in the `SubGraphEntry` for dirty-rect expansion — only the `phase_build` rect lists use the clipped form. The tile's picture-space rect is derivable from the tile key and tile size already in scope in the `for y { for x }` loop.

For each visible primitive, inside the same `for y in p0.y..p1.y { for x in p0.x..p1.x { ... } }` per-tile loop the existing code already uses (clipping `capture_rect`/`pic_coverage_rect` to `tile_pic_rect` first, per above):

```
let s = &mut tile.cached_surface.phase_build;

// Phase-state update only — does NOT replace the function's normal flow
match prim.kind:
    BackdropRender(pic_index, capture_rect):   // capture_rect already clipped to tile_pic_rect
        let assignment;
        if s.force_serial_only || frame_state.in_3d_context_count > 0 {
            // Sticky overflow (§6.1) or 3D plane-split context (§6.2) —
            // fusion unsafe; emit barrier. advance_phase_barrier itself
            // latches force_serial_only if its internal bump fails.
            advance_phase_barrier(s);
            assignment = PhaseAssignment::ForceSerial;
        } else {
            let needs_new_phase = s.in_phase && (
                s.current_phase_filter_rects.iter().any(|r| r.intersects(&capture_rect))
                || s.current_phase_prim_rects.iter().any(|r| r.intersects(&capture_rect))
            );
            if needs_new_phase && !try_bump_phase_id(s) {
                // First overflow event on this tile. Latch and barrier; do not
                // fall through to the Phase(s.current_phase_id) push below.
                s.force_serial_only = true;
                advance_phase_barrier(s);
                assignment = PhaseAssignment::ForceSerial;
            } else {
                if needs_new_phase {
                    s.current_phase_filter_rects.clear();
                    s.current_phase_prim_rects.clear();
                }
                s.current_phase_filter_rects.push(capture_rect);
                s.in_phase = true;
                assignment = PhaseAssignment::Phase(s.current_phase_id);
            }
        }
        // existing sub_graphs.push call now builds a SubGraphEntry with `phase: assignment`

    BackdropCapture:
        // no phase-state update (marker, doesn't paint into the dynamic-content task)

    Picture { pic_index } if pictures[pic_index.0].flags.contains(PictureFlags::IS_SUB_GRAPH):
        // no phase-state update — hoisted IntermediateSurface, doesn't paint into the dynamic-content task

    _ if on_picture_surface && s.in_phase:
        // prim.pic_coverage_rect clipped to tile_pic_rect; skip if the clip is empty
        insert_with_merge(&mut s.current_phase_prim_rects, clipped_prim_rect)

    _:
        // no phase-state update — prim is inside a non-passthrough sub-surface and
        // doesn't paint into the dynamic-content task
```

`insert_with_merge` keeps the list disjoint and bounded:
- If the new rect intersects any existing rect, replace them with their union (re-check transitively — a merged rect can intersect another existing entry).
- If the list would exceed K (say 16), merge the two closest rects.

Prims observed *before* any filter on this tile (`in_phase == false`) are not accumulated — they're already baked into `D_0` and irrelevant to phase decisions.

`advance_phase_barrier(s)` is the helper that makes ForceSerial a hard divider so later filters can't fuse across it:

```
fn advance_phase_barrier(s: &mut PhaseBuildState) {
    s.saw_force_serial = true;
    s.in_phase = false;                 // any next filter starts fresh
    s.current_phase_filter_rects.clear();
    s.current_phase_prim_rects.clear();
    // The ForceSerial filter isn't in any Phase(p) bucket — but we still
    // advance current_phase_id so a later normal filter on this tile can
    // claim a fresh phase id distinct from any used before the barrier.
    // If the bump fails (we're already at MAX), latch force_serial_only
    // so subsequent filters on this tile can't silently reuse Phase(MAX).
    // Without this latch, the next non-overlapping non-3D filter would
    // skip the needs_new_phase branch entirely, never call try_bump,
    // and silently get Phase(MAX) again — corrupting fusion semantics.
    if !try_bump_phase_id(s) {
        s.force_serial_only = true;
    }
}

fn try_bump_phase_id(s: &mut PhaseBuildState) -> bool {
    if s.current_phase_id == PhaseId::MAX { return false; }
    s.current_phase_id += 1;
    true
}
```

### 6.1 Overflow guard

`current_phase_id` is `u16`, but a runaway should not panic on overflow in release. The first overflow event on a tile is what `try_bump_phase_id` returning `false` indicates. At that point we:

1. Set the sticky latch `s.force_serial_only = true`.
2. Emit `PhaseAssignment::ForceSerial` for the current filter.
3. Call `advance_phase_barrier(s)`.

From that frame onwards, every later filter on the tile hits the `s.force_serial_only ||` branch at the top of the match and is also `ForceSerial`. Without the latch, the overflow filter would clear `current_phase_filter_rects`, the next non-overlapping filter would skip `needs_new_phase`, never call `try_bump_phase_id`, and silently get `Phase(MAX)` — silently reusing a phase id when we'd already lost track of distinctness. The latch closes that hole.

The threshold is only reached on pathological scenes (≥65k filters in one tile) — this is correctness insurance, not a hot path.

### 6.2 3D / perspective contexts: explicit plumbing

Pictures participating in a preserve-3d context route through a plane splitter that sorts polygons by depth at frame-build time, meaning **draw order ≠ display order** within a 3D context. Our phase computation assumes display order (the order `update_prim_dependencies` observes prims). Two backdrop-filters inside the same 3D context could be reordered by the splitter such that B (display-after A) is drawn before A, breaking the phase invariant if fused. So: any `BackdropRender` observed while inside a 3D context is forced serial.

**Detect *any* `In`, not just the root (verified).** `Picture3DContext` (`picture.rs:274-290`) has two participating shapes:
- `In { root_data: Some(..), .. }` — the **root** establishing the context. It does *not* add split planes itself; it collects its child participants into `root_data` (`scene_building.rs:3564-3579`) and resolves them via the splitter at `picture.rs:914-924`.
- `In { root_data: None, .. }` — a **child participant**. It *does* add a split plane (`prepare.rs:1748-1758`).

Scene building collects participant instances into the root's `root_data` vector during the stacking-context body (`add_primitive_instance_to_3d_root`, `scene_building.rs:3564-3579`), then **drains them back into the 3D root picture's `prim_list`** at stacking-context close (`scene_building.rs:2463-2480`, `prim_list.add_prim(...)`). So at visibility time the participants *are* descendants of the root and `update_prim_visibility` *does* recurse through them. **The guard must fire for any `Picture3DContext::In { .. }`, both variants** — counting all `In` variants is conservative and directly covers every participant picture (each is itself `In`, and each is visited). It is also precise at the right boundary: when `preserve-3d` has no real effect, scene building converts the whole group to `Picture3DContext::Out` and disables the off-screen surface (`scene_building.rs:2488-2498`), so those (where draw order *does* equal display order) are correctly *not* counted and remain free to fuse. (Codex flagged the earlier inaccurate rationale; confirmed against the code.)

**Detection plumbing**: `surface_stack` is unreliable — only non-passthrough pictures push onto it (`visibility.rs:267-303`), so a passthrough `Picture3DContext::In` picture is invisible there. We add an explicit counter to `FrameVisibilityState` (`visibility.rs:48`):

```rust
pub in_3d_context_count: u32,   // depth of nested 3D plane-split contexts
```

**No RAII guard over `FrameVisibilityState`.** The originally-sketched `struct Push3D<'a> { state: &'a mut FrameVisibilityState<'a>, .. }` does **not compile**: the self-referential `&'a mut FrameVisibilityState<'a>` would hold the whole frame-state borrowed for the entire `update_prim_visibility` body, conflicting with every existing `frame_state.<field>` mutation inside it. (Both Codex and the review caught this.) Any guard that borrows into `frame_state` has the same problem.

Instead, use a plain by-value increment/decrement bracketing the recursion, placed **after** the function's existing early-return guards (visited check, culling at `visibility.rs:247-256`), so the increment only happens once we're committed to walking the subtree:

```rust
let is_3d = matches!(pic.context_3d, Picture3DContext::In { .. });
if is_3d { frame_state.in_3d_context_count += 1; }
// ... existing body, including the recursive update_prim_visibility calls ...
if is_3d { frame_state.in_3d_context_count -= 1; }
```

There must be **no early return between the increment and the decrement**. The function currently has its only early returns *before* this point; the invariant to preserve is "every increment site is dominated by the matching decrement on all paths." A debug-assert that `in_3d_context_count == 0` at the end of every top-level call is the backstop against a future edit introducing an unbalanced early return (it is a backstop, not the primary correctness mechanism).

`update_prim_dependencies` reads `frame_state.in_3d_context_count` (passed in alongside the existing args, or via the visibility frame-state pointer). When non-zero, any `BackdropRender` observed is emitted with `PhaseAssignment::ForceSerial` and `advance_phase_barrier` runs.

**This counter is a correctness invariant, not a hint.** Because participants are visited (above), their `BackdropRender`s reach `update_prim_dependencies` and *will* be assigned a real `Phase(p)` unless `in_3d_context_count > 0` at that moment. So a **false negative** in the counter — reading 0 while inside a plane-split context — does not merely "miss a fusion opportunity"; it lets a 3D-reordered filter fuse, which can be visually wrong. The invariant to uphold is: *every `BackdropRender` observed inside any `Picture3DContext::In` must see `in_3d_context_count > 0`.* The balance debug-assert above guards against unbalanced edits; the matched `is_3d` increment fires for both `In` variants so there is no participant path that escapes it. There is no safe-by-default fallback here — get the counter right.

A future optimisation could defer phase computation to prepare-time, after the plane splitter has produced the final draw order. Out of scope here.

## 7. Implementation walk-through (where the code changes)

### 7.1 `invalidation/cached_surface.rs:25`

Replace the tuple `sub_graphs: Vec<(PictureRect, Vec<...>)>` with `Vec<SubGraphEntry>`. Add `phase_build` field. Clear both in `pre_update` (`:76`).

### 7.2 `tile_cache/mod.rs:2176` (`update_prim_dependencies`)

In the per-tile loop where `sub_graphs.push(...)` is called for `BackdropRender` (around `:2658-2664`), apply the join-or-bump rule against `phase_build` and store the resulting `phase` in the new `SubGraphEntry`. For all other paths through the function, in the affected-tiles loop, when `on_picture_surface && s.in_phase`, call `insert_with_merge` on `phase_build.current_phase_prim_rects`. Skip Pictures whose `pic_index` has `PictureFlags::IS_SUB_GRAPH`.

### 7.3 `picture.rs:2015` and `picture.rs:2050`

When seeding `SurfaceTileDescriptor`, also populate its `phase_map` by iterating `tile.cached_surface.sub_graphs` and pushing `(entry.pic_index, entry.phase)` into the `Vec`. Initialise `phase_source_task: None, current_phase: None`. Lookups in §7.5 are a linear `iter().find(|(p, _)| *p == pic_index)` — fine for the handful of filters a tile carries.

### 7.4 `surface.rs:473` (`register_resolve_source`)

No longer reads the parent task — it just walks back to the `establishes_sub_graph` builder and sets `resolve_source = surface_task_id` as today. That part stays identical; the meaning of `resolve_source` (= destination of the blit) was always already what we need. **No change.**

### 7.5 `surface.rs:615` (`pop_surface`, sub-graph branch)

This applies only to the **Tiled** parent arm (`:648-721`). The Simple / Chained arms run today's per-filter splice unconditionally — see §5.1.

The current Tiled arm iterates every parent tile and **first switches on `parent_task.location`** (`surface.rs:658`):

- **`RenderTaskLocation::Static { .. }`** (`:710-716`): re-insert the descriptor unchanged. **No** `src_task_ids` push and **no** dependencies added *inside this branch*. (The belt-and-braces `add_dependency` at `:810-828` still fires for every tile in the parent map, Static included — that's a separate, downstream loop.) This branch handles tiles built without any sub-graph at construction time (`picture.rs:2050` path — tiles whose `cached_surface.sub_graphs.is_empty()` was true when the descriptor was created). These tiles are not part of the dynamic-content pipeline and the current backdrop-filter doesn't touch them.
- **`RenderTaskLocation::Unallocated { .. } | RenderTaskLocation::Existing { .. }`** (`:659-709`): full splice — duplicate the parent task with a fresh command buffer, push `parent_task_id` into `src_task_ids`, create the replacement task with `Location::Existing { parent_task_id, size }`, add `add_dependency(resolve_task_id, parent_task_id)` and `add_dependency(new_task_id, parent_task_id)`, re-insert the descriptor with `current_task_id = new_task_id`.
- **`_`** (`:717-719`): `panic!`. Should never trigger.

Today the dynamic-content branch runs for **every** non-Static parent tile in the `Tiled` map, including tiles the filter doesn't touch — those tiles get a "fake" replacement task whose resolve blit later no-ops at `handle_resolve` because of empty intersection. Wasteful but harmless.

**Phase-aware version**:

- The `Static` branch is **unchanged** — keep today's bare re-insert with no `src_task_ids` push and no inner-branch dependency adds. By construction these tiles have empty `phase_map` and no filter touches them; treating "filter on Static tile" as `phase_map.get() == None` would over-trigger the full splice that the Static branch is explicitly designed to skip.
- The `Unallocated | Existing` branch consults `descriptor.phase_map.get(&pic_index)`. The cases differ in **two independent decisions** which the code should make in this order:
  1. **What's the resolve source for this tile?** Compute a single `resolve_src_task_id` and push it once into `src_task_ids`, with one matching `add_dependency(resolve_task_id, resolve_src_task_id)`.
  2. **Do we splice (allocate `D_{P+1}`) on this tile?** If yes, run the existing duplicate / `Existing`-location code and `add_dependency(new_task_id, parent_task_id)`. If no, leave `descriptor.current_task_id` alone.

  The four cases are then a compact decision table over those two choices plus the descriptor-state updates:

| Case | `phase_map.get(&pic_index)` | `resolve_src_task_id` | Allocate `D_{P+1}`? | Descriptor state updates |
|---|---|---|---|---|
| 1 | `None` | `parent_task_id` | Yes (today's full splice) | None — leave `current_phase`/`phase_source_task` untouched so a later filter sees state as if this one hadn't existed |
| 2 | `Some(ForceSerial)` | `parent_task_id` | Yes (today's full splice) | Reset `current_phase = None`, `phase_source_task = None` — hard barrier (see "Why ForceSerial resets" below) |
| 3a | `Some(Phase(p))`, `current_phase != Some(p)` (first of phase) | `descriptor.current_task_id` *before* splice (snapshotted to `phase_source_task`) | Yes — this allocates `D_{P+1}` | Set `phase_source_task = Some(<the snapshotted task>)`, `current_phase = Some(p)`; `current_task_id` becomes `D_{P+1}` as part of the splice |
| 3b | `Some(Phase(p))`, `current_phase == Some(p)` (subsequent of phase) | `descriptor.phase_source_task.unwrap()` (= `D_P` saved during 3a) | **No** — `descriptor.current_task_id` (= `D_{P+1}`) is reused | None — `phase_source_task` and `current_phase` unchanged |

  Notes on the table:
  - `src_task_ids.push(resolve_src_task_id)` and `add_dependency(resolve_task_id, resolve_src_task_id)` happen **exactly once per non-Static tile**, with the value chosen from the row. They are not repeated.
  - "Allocate `D_{P+1}`" is the existing code at `surface.rs:666-708` — `pic_task.duplicate(cmd_buffer_index)`, `RenderTaskLocation::Existing { parent_task_id, size }`, `add_dependency(new_task_id, parent_task_id)`, and updating `descriptor.current_task_id = new_task_id`. Skipping it in case 3b is the whole optimisation.
  - In case 3a, the snapshot must happen *before* `current_task_id` is overwritten. The natural code shape: `let phase_source = descriptor.current_task_id; … allocate new_task_id … descriptor.phase_source_task = Some(phase_source); descriptor.current_task_id = new_task_id; descriptor.current_phase = Some(p);`.

**Load-bearing ordering coupling (make it explicit).** Phases are *assigned* in visibility (display order) but `phase_source_task` is *snapshotted* in prepare (case 3a). For the snapshot to be the correct `D_P`, two things must hold:
  1. `pop_surface` must process the filters of a given phase in the same relative order visibility observed them, and
  2. the **first-of-phase** filter (the one whose `current_phase != Some(p)` triggers 3a) must pop *before* any 3b member of that phase, so its snapshot captures the true pre-phase tile state.

Both follow from the hoisted `IntermediateSurface`s being prepared in display order, which holds as long as hoisting to the landing SC (tile-cache root in the perf case) *appends* in display order. This is currently true but is an implicit dependency of the whole scheme.

Note the table classifies 3a vs 3b purely by whether `current_phase == Some(p)`, **not** by display position — so the *first* phase-`p` filter to pop becomes the 3a snapshot regardless of order. That self-classification has a useful safety property (a phase-`p` member can never read a *stale* `phase_source_task` left over from an earlier phase: a mismatch always re-snapshots), but it does **not** by itself guarantee the snapshot is the intended `D_P` if filters pop out of display order — an out-of-order first-pop would snapshot a tile state that already contains some phase-`p` content. That is exactly the silent-incorrectness case, and the assertion below is the real guard, not the self-classification.

Guard it: in step 1 (instrumentation), debug-assert per tile that the first `pop_surface` lookup producing a given `Phase(p)` corresponds to the display-earliest phase-`p` filter (equivalently: a 3b reuse of `Some(p)` is never observed before that phase's 3a snapshot). If it ever trips, force the whole tile to `ForceSerial` for that frame in step 2 rather than fuse wrongly.

The belt-and-braces `add_dependency` to every tile at `:810-828` stays unchanged — it fires for all tiles (Static, splice, no-splice) regardless of which row above applied.

**Implementation note — preserve new descriptor fields across re-insertion.** The current Tiled splice at `surface.rs:702-708` re-inserts the descriptor with functional-update syntax:

```rust
tiles.insert(
    key,
    SurfaceTileDescriptor {
        current_task_id: new_task_id,
        ..descriptor
    },
);
```

The `..descriptor` is what carries `composite_task_id` and `dirty_rect` (the original fields) plus the three new ones (`phase_source_task`, `current_phase`, `phase_map`) across the re-insertion. The compiler *does* enforce that every field is initialised in an explicit construction — but it won't catch the actual hazard: someone editing the splice rewrites the explicit form and adds default resets for the phase fields (`phase_source_task: None, current_phase: None, phase_map: FastHashMap::default()`), thinking these are "new-tile" defaults. That silently wipes phase state every splice and manifests as fusion never engaging (lookups always return `None`), which is hard to diagnose.

Two safer patterns: keep the `..descriptor` shorthand and mutate `descriptor` *in place* before re-inserting (e.g. `descriptor.current_phase = Some(p); descriptor.phase_source_task = Some(...);` then `tiles.insert(key, descriptor);` after `descriptor.current_task_id` is set); or do `let mut descriptor = tiles.remove(&key).unwrap(); … mutate … tiles.insert(key, descriptor);` throughout. The §7.5 case bodies above are described in mutate-in-place style for exactly this reason.

**Why ForceSerial resets `current_phase` to `None`**: it would be tempting to leave the existing `current_phase` alone, so that a Phase(p) → ForceSerial → Phase(p) sequence on the same tile would still let the second Phase(p) fuse with the first. But that would be wrong: any prims drawn between the ForceSerial filter's splice and the next Phase(p) filter (including the ForceSerial filter's own BackdropRender) are committed into a task that *is not* the first Phase(p)'s `phase_source_task`. Resetting forces a fresh first-of-phase splice and a new `phase_source_task` snapshot.

The `Simple` and `Chained` arms (`:723-782`) are out of scope for this work — see §5.1. They retain today's per-filter splice unconditionally; no `phase_map` lookup or per-phase state is added to `CommandBufferBuilderKind::Simple` in this round.

### 7.6 `prepare.rs:1761` (`BackdropCapture`)

No code change. `register_resolve_source` is called as today.

### 7.7 `prepare.rs:1777` (`BackdropRender`)

No code change. It still reads from `sub_graph_output_map[pic_index]` and adds it as a child render task on the descriptor's current task at the time the `BackdropRender` is prepared. For normal same-phase members this is `D_{P+1}` and is shared across the phase. If a `None`-case (filter doesn't touch this tile) fake splice happens between two same-phase filters, the descriptor's current task may have advanced to a further-duplicated task (e.g. `D_{P+1}'`) — but that duplicate uses `RenderTaskLocation::Existing` against the same parent allocation, so all the same-phase quads still land in the same physical render target. The fusion property is preserved; only the task-id chain has more links.

### 7.8 No changes to: render-task graph, pass batcher, batching, shaders, scene building.

## 8. Worked example: 3 non-overlapping filters in one phase

Today (3 filters, chain length L):
```
D_0 → resolve_A → A_chain(L) → D_1 → resolve_B → B_chain(L) → D_2 → resolve_C → C_chain(L) → D_3 → TileComposite
```
Render-pass depth ≈ 3·L + bookkeeping. All passes are serialised.

After (phase 0 = {A, B, C}):
```
                   ┌─► resolve_A → A_chain(L) ──┐
D_0  ──────────────┼─► resolve_B → B_chain(L) ──┼──► D_1 → TileComposite
                   └─► resolve_C → C_chain(L) ──┘
```
Render-pass depth ≈ L + ~3 (resolves at depth 0, chains span depths 1..L, D_1 at L+1, TileComposite at L+2). Independent of phase size.

Filter chains of differing lengths still all share the same passes for as long as they overlap, then the shorter chains just finish early.

This diagram assumes A, B, C are each single-tile (or uniformly phased across their tiles). If, say, B also covered a second tile where it landed in a later phase, B's resolve would depend on that tile's post-A task and B's chain would drop below A's depth here too — collapsing back toward the serial shape (§1, §9.1). The fan-out is the best case, realised exactly when every filter in the phase phases uniformly across all its tiles.

## 9. Edge cases (what I checked, with verdicts)

1. **Multi-tile filters.** Phase is keyed by `(pic_index, tile_key)`. `phase_map` per tile descriptor handles it naturally for *correctness* — the resolve op's `src_task_ids` already gathers per-tile sources, just sourced from `phase_source_task` instead of `current_task_id`, so each tile reads the right backdrop. **But** the filter has a single resolve task and single chain (§1): if its per-tile phase assignments disagree (same-phase as a predecessor on one tile, later-phase on another), the resolve task depends on the later tile's post-predecessor source and the whole chain serialises after that predecessor on *all* tiles. Result: correct output, no coalescing benefit, and *close to* — but not provably identical to — today (B reads the early source on the same-phase tile, which extends that source's lifetime and adds a dependency to that tile's combined task; see §1). The full win requires a filter to phase uniformly across all its tiles. This is the main case where per-tile phasing buys correctness but not speed; the optional §11 reconciliation (force non-uniform multi-tile filters serial everywhere) restores strict no-regression if measurements show it matters.

2. **Filter culled on some tiles** (already gated by `required_sub_graphs` at `tile_cache/mod.rs:2643`). The phase walk only runs inside the `if !pic_coverage_rect.is_empty()` branch, matching today's behaviour.

3. **SVG filter graphs (`PictureCompositeMode::SVGFEGraph`).** A single sub-graph picture. Phasing is agnostic to chain shape — same input, same output requirements.

4. **Mix-blend-mode inside a backdrop sub-graph.** Uses the readback fallback path independently; unaffected.

5. **Single-filter tiles.** Path collapses to the existing single-splice behaviour: one filter, one phase, no extra work.

6. **All filters overlapping.** Each filter starts a new phase. Output identical to today.

7. **Coordinate space.** `pic_coverage_rect` is already in the tile cache's picture space (`tile_cache/mod.rs:2207-2256`). `phase_filter_rects` and `phase_prim_rects` live in the same space. Intersections are direct.

8. **Tile invalidation.** `sub_graphs.clear()` and `phase_build` reset happen in `pre_update` (`cached_surface.rs:76`). Phase IDs are rebuilt from scratch every frame. Tile invalidation walks `sub_graphs[i].surface_stack` for dirty-rect expansion — unchanged.

9. **`add_dependency` belt-and-braces** at `surface.rs:810-828`. For every normal same-phase member, lands on the same `D_{P+1}` — fine. ForceSerial and `None`-case tiles each get their own per-filter `current_task_id` and the dependency lands there, matching today's behaviour. Static tiles also receive the dependency (they're in the parent's tile map); this matches today.

10. **Simple / Chained parent surfaces** (`surface.rs:723-782`, including the `Chained` `root_task_id` hazard guard at `:773`). Out of initial scope per §5.1 — these arms continue today's per-filter splice unconditionally. No phase-aware path runs through them in this round; the existing `add_dependency(root_task_id, new_task_id)` guard is unchanged. A backdrop-filter hoisted to a Simple parent still receives a (phantom) phase assignment in visibility but ignores it here — verified harmless, see §5.1. A follow-up could extend phase fusion to Simple parents.

13. **Filters overlapping only outside a tile.** Two filters whose full coverage rects intersect, but whose tile-clipped rects on tile T are disjoint, are assigned the **same phase on T** — so on T, B reads the earlier (pre-A) source rather than the post-A one. This is the §6 tile-clipped interference test giving correct, tighter per-tile *source selection*; using the full rect would have forced B to read post-A on T unnecessarily. Note this is a statement about **source selection on T**, *not* about render-pass coalescing: if A and B overlap on the *other* tile U, they split there, B's single chain depends on U's post-A source, and the chains do not coalesce globally (§1, §9.1). So the precise benefit is "B reads the earlier backdrop on T," not "B's chain becomes independent." Verified `pic_coverage_rect` is the full rect pushed identically to every covered tile (`tile_cache/mod.rs:2658-2664`), so the clip is a real precision win for source selection.

14. **Visibility/prepare ordering.** The `phase_source_task` snapshot in `pop_surface` (case 3a) assumes same-phase filters pop in display order with the first-of-phase first. Holds via display-ordered hoisting; guarded by a step-1 debug-assert and a defensive 3b→3a fallback (§7.5).

11. **`IntermediateSurface` Picture visible on tile**. Has `IS_SUB_GRAPH` flag → excluded from `phase_prim_rects`. The actual painting happens via the `BackdropRender` quad which we handle separately.

12. **Non-passthrough `WRAPS_BACKDROP_FILTER` SC** (rare — the SC itself has effects like opacity). Its Picture prim is visited with `on_picture_surface == true` and is *not* sub-graph flagged, so it accumulates. Its coverage closely matches the `BackdropRender`'s coverage, so it doesn't add new restrictiveness in practice.

## 10. Testing

- **Reftests** (visual identity vs today):
  - 2 non-overlapping filters, no interfering prims → must fuse phases.
  - 2 non-overlapping filters with a banner prim between them overlapping the 2nd's capture → must split.
  - 2 overlapping filters → must split.
  - SVG filter graph alongside CSS filter, non-overlapping.
  - Multi-tile filter mixed with single-tile filters (covers the `phase_map.get() == None` case on non-touched tiles).
  - **Global-chain caveat (§1 / §9.1)**: a multi-tile backdrop filter (ideally a blur whose kernel straddles a tile boundary) that is **same-phase** as a predecessor on one tile but **split-phase** on the adjacent tile. Verify visual identity to today (each tile reads the correct per-tile source), and — via the step-1 instrumentation — that this filter is counted in `would_fuse_count` but **not** in `truly_fused_count`, confirming the per-tile-vs-global distinction is measured rather than assumed.
  - Non-passthrough `WRAPS_BACKDROP_FILTER` SC (e.g. parent has `opacity: 0.5`).
  - **ForceSerial barrier**: three filters A, B, C on one tile where A and C *would* fuse but B is `ForceSerial` (e.g. B is inside a `preserve-3d` rotateY). Verify visual identity and that C does *not* fuse with A across B.
  - **Backdrop-filter inside `transform-style: preserve-3d`** (`backdrop-filter-preserve-3d.html`): backdrop-filter siblings under a 3D parent → must hit the §6.2 path, all filters get `ForceSerial`, output visually identical to today.
  - **Passthrough 3D-context root**: ensures the `in_3d_context_count` plumbing is correct independent of `raster_config` (covers the `visibility.rs:267-303` push gap).
  - **Simple (non-tiled) parent**: backdrop-filter hoisted to a Simple-parent surface — exercises §5.1 fall-through (today's behaviour preserved).
- **Mochitest**: `HIGHLIGHT_BACKDROP_FILTERS` debug overlay still highlights every capture.
- **Perf**: `bf-perf.html` mastodon clone on Mali / PowerVR / Apple-Silicon. Capture profile and check render-pass count drops from ≈N·L to ≈L per phase. Compare to a hand-crafted control case with `K` identical disjoint filters in one tile.
- **WPT**: existing backdrop-filter WPTs unchanged.

## 11. Risks and confidence

- **Pass-batcher coalescing**: verified by code reading. High confidence — no scheduling hints needed. (4.1)
- **`phase_prim_rects` heuristic**: list-of-disjoint-rects-with-merge avoids the mastodon flooding pathology. Cap K=16 with a "merge two closest" overflow rule. Cheap, bounded. (4.7)
- **Peak render-target memory (elevated to initial scope)**: today exactly one resolve target + one filter chain is live at a time; under fusion a phase of K filters has K resolve targets *plus* K filter chains live across the *same* passes. Worse, unequal-length chains get different `free_after` → different `lifetime_group` → they will **not** pack into a shared physical target (`render_task_graph.rs:178`), so peak allocation grows roughly linearly in K. On the memory-constrained Mali/PowerVR GPUs this work targets, that can regress or fail allocation outright — the opposite of the intended win. **Therefore: ship a phase-size cap (force-split after K members, start K≈8–16) in the initial implementation, not as a stretch**, and measure peak GPU render-target memory in §10, not just render-pass count. The cap is a trivial addition to `advance_phase_barrier` (force a barrier once `current_phase_filter_rects.len()` reaches the cap).
- **Non-uniform multi-tile filters — optional strict-no-regression reconciliation**: a multi-tile filter that is same-phase as a predecessor on one tile but later-phase on another reads the early source on the same-phase tile while its single global chain still serialises late (§1, §9.1). This is correct but extends that source's lifetime and adds a dependency to the same-phase tile's combined task, so it is *close to* today rather than provably identical. If step-1/step-3 measurements show this hurts (extra peak memory or a frame-time bump on mixed scenes), add a per-filter reconciliation: when a filter is non-uniform across its tiles, make it `ForceSerial` on every tile so it behaves exactly like today. **"Non-uniform" here is the source-class definition from §1, not raw `Phase(p)` equality** — per-tile phase IDs are independent counters, so the test is whether the filter is *fused* (reuses an existing phase's pre-predecessor source — §7.5 "subsequent of phase"/3b) versus *fresh* (reads post-predecessor content — 3a-with-predecessors or `ForceSerial`) consistently across all its tiles. Mixed fused/fresh ⇒ reconcile.

**Reconciliation must update `phase_build`, not just the stored entries (Codex caught this).** A filter's `BackdropRender` touches all its tiles within a single `update_prim_dependencies` call, so non-uniformity is detectable at the end of that call — but by then the per-tile `phase_build` state machines have *already* mutated for the fused interpretation (on the same-phase tile the filter was pushed into `current_phase_filter_rects` with `in_phase` still true and the phase id un-bumped). Rewriting only the `sub_graphs` / `phase_map` `PhaseAssignment` to `ForceSerial` while leaving `phase_build` as-is would let *later* filters be classified into the phase across the intended barrier, and would desync the §12 monotonicity / bounded-growth assertions. Two correct shapes:
  - **Decide-before-commit (preferred, no rollback):** compute the tentative per-tile decision from each tile's current (pre-filter) state first, classify each by **source class** (does the chosen resolve source carry a predecessor-*filter* dependency?), check the class is uniform across tiles, then commit — either the normal per-tile fused assignments (uniform) or `advance_phase_barrier` on every touched tile (non-uniform). Because per-tile states are independent, the tentative decisions can be gathered before any mutation. A precise classifier asks whether the source transitively depends on a predecessor filter's output; a cheaper **conservative proxy** is "is this tile a non-leader phase follower (3b)?" — uniform-3b fuses, anything mixed reconciles. The proxy can over-reconcile (e.g. pristine-leader on one tile, follower on another, where both are actually predecessor-free) but only ever errs toward today's serial behaviour, never toward unsafe fusion, which is the right bias for an optional safety knob. Do **not** classify by raw `Phase(p)` integer (per §1).
  - **Rewrite-with-reset:** if done post-hoc, save each touched tile's pre-filter `phase_build` and, on a non-uniform verdict, restore it and re-apply the `ForceSerial` path (`advance_phase_barrier`) on every touched tile — equivalently, a conservative barrier+reset everywhere the filter landed.

Cheap either way (one pass over the filter's tile set) but needs the per-filter tile decisions grouped, hence a refinement rather than baseline. Interacts with the phase-size cap above — both are knobs that trade fusion for predictability.
- **`phase_map` allocation**: a `Vec<(PictureIndex, PhaseAssignment)>` per `SurfaceTileDescriptor` means one small heap allocation per *dirty* tile per frame, and it is capture/replay-serialised. Per-tile filter counts are tiny so this is acceptable for v1 (and `Vec` was chosen over `FastHashMap` precisely to keep it cheap — §5), but track it: if profiling shows allocation churn, a `SmallVec<[_; 4]>` (if it can be made capture-serialisable) or a frame-arena-backed slice would remove the per-tile alloc.
- **Order of `BackdropRender` quads within a phase**: same display order as today. They land in the descriptor's current task at quad-prepare time (typically `D_{P+1}`, possibly a further `Existing`-chained duplicate if `None`-case fake splices intervened — all sharing the same physical target). By construction non-overlapping within a phase, so order is irrelevant for correctness.
- **`should_inflate=false, BlurEdgeMode::Mirror`** for backdrop blurs (`scene_building.rs:4275`): unchanged. Each phase member owns its own blur chain with the same edge mode.
- **`PictureFlags::IS_SUB_GRAPH` filter coverage**: verified flag is set on exactly the IntermediateSurface (last in chain, `scene_building.rs:339`). Intermediate filter Pictures in the chain don't have the flag but are below `on_picture_surface == false` anyway.

## 12. Rollout

1. **Step 1, instrumentation-only**: land the visibility-time phase computation and the data extensions to `SubGraphEntry` / `SurfaceTileDescriptor` / `FrameVisibilityState`. Do not change `pop_surface` behaviour (it ignores the new fields). Rendering is byte-identical to today; what we're validating is the phase algorithm itself. The assertions are:
   - **Monotonicity**: in each tile's `sub_graphs`, restricted to entries with `PhaseAssignment::Phase(p)`, the `p` values are monotonically non-decreasing in insertion order.
   - **Bounded growth (barrier-aware)**: between two consecutive `Phase(p_i)` and `Phase(p_j)` entries on the same tile with `k` `ForceSerial` entries between them, `p_j - p_i ≤ max(1, k)`.
     - `k = 0`: a single `needs_new_phase` bump (if the new filter overlaps the current phase) can advance `p` by 1. Otherwise `p_j = p_i`.
     - `k ≥ 1`: each `ForceSerial` traverses `advance_phase_barrier`, bumping `current_phase_id` by exactly 1. The next `Phase(p_j)` does **not** re-bump on entry: `advance_phase_barrier` sets `in_phase = false`, so `needs_new_phase = s.in_phase && (overlap)` short-circuits to false, and `p_j` lands at exactly the post-barrier value. Hence `p_j - p_i = k`.
     This bound is tight in both regimes.
   - **Coverage**: every visible `BackdropRender` that goes through `pic_coverage_rect.is_empty() == false` produces a `sub_graphs` entry on every parent tile its rect intersects (no missing entries on intersecting tiles, no spurious entries on non-intersecting tiles).
   - **3D-fallback exercised**: the `backdrop-filter-preserve-3d.html` test produces only `ForceSerial` entries for filters under the 3D parent. The control case (the same filters *not* under preserve-3d) produces `Phase(p)` entries.
   - **`in_3d_context_count` balance**: increment/decrement are paired — counter returns to 0 at end of every top-level `update_prim_visibility` call (debug-assert). Covers both `Picture3DContext::In` variants (§6.2).
   - **Snapshot ordering**: per tile, a 3b reuse of `Some(p)` is never observed in `pop_surface` before the corresponding 3a snapshot of `p` (the load-bearing ordering coupling, §7.5). If this trips, fusion would be silently wrong — treat as a hard barrier in step 2.
   - **ForceSerial-barrier behaviour**: in the A–B(ForceSerial)–C reftest, C's `Phase(p)` id is strictly greater than A's `Phase(p)` id on every tile they share.
   - **Telemetry counter**: log `would_fuse_count = (total Phase(p) filter–tile pairs) - (total distinct (tile, p) pairs)`. ForceSerial entries are excluded from both terms; **phantom assignments are also excluded** — a filter whose `BackdropRender` was observed with `prim_surface_index != tile_cache.surface_index` will pop against a Simple parent and never fuse (§5.1), so counting it would overstate the available win. **Caveat (per §1):** this is a *per-tile* count and over-states real pass coalescing for multi-tile filters with split phase assignments — those read the right per-tile source but their single chain still serialises globally. Log a second, stricter `truly_fused_count` that counts a filter as fused only when it is single-tile, or multi-tile with an *identical* phase relationship to its predecessors on every covered tile (i.e. its resolve task gathers only pre-predecessor sources). The gap between the two numbers is the multi-tile degeneracy. A non-zero `truly_fused_count` on the mastodon case is the real evidence the algorithm helps; the goal is to maximise *that* before flipping the pref.
   
   These assertions validate the algorithm against current code without changing render output. Note that "phase IDs match the strict-serial baseline" is **not** an assertion we want — it would *fail* in the success case.
2. **Step 2, gated activation**: add a pref (e.g. `gfx.webrender.parallel-backdrop-filters`). When on, `pop_surface` consults `phase_map` and skips the splice for subsequent same-phase filters. Off by default.
3. **Step 3, validation**: run try push (`mach try fuzzy`), full WPT, reftests, and `bf-perf.html` on a Mali device. Compare render-pass count and frame time.
4. **Step 4, flip on**: once green and perf-confirmed on Android, default the pref on.

The pref gives a single-knob rollback if a corner-case visual regression surfaces, and lets us A/B perf without rebuilds.
