# How WebRender renders `backdrop-filter`

This is a tour of the code path with the render-task graph topology spelled out and every extra copy accounted for. References are to `gfx/wr/webrender/src/`.

## 1. The big idea

`backdrop-filter` is implemented as a **picture sub-graph** that splices itself into its parent surface (typically a picture cache tile). The mechanism has three movers:

- **`BackdropCapture`** primitive — a marker that, during `prepare`, registers the *current* parent task as a "resolve source."
- A chain of filter `Picture`s wrapped in an `IntermediateSurface` — the sub-graph that actually runs the filters. For CSS filter functions this is one picture per filter op; for an SVG filter graph (`filter: url(#…)`) the whole graph is a single `Picture` with `PictureCompositeMode::SVGFEGraph(..)`.
- **`BackdropRender`** primitive — the placeholder that, at the right point in the parent surface's draw order, samples the sub-graph's output and composites it back into the tile.

The capture is *not* a framebuffer readback. It is an explicit resolve (a `device.blit_render_target`) from one render-task target into another, scheduled into the task graph rather than emerging at draw time.

A second consequence of putting backdrop filters on a picture cache tile, which is easy to miss: the tile stops drawing directly into its persistent slot and is instead rendered into a **dynamic content** picture task that is then blitted into the slot via a final `TileComposite` task. See §3 and §6.

## 2. Scene building

### Display list entry

`webrender_api/src/display_list.rs:1662` — `push_backdrop_filter` records a `DisplayItem::BackdropFilter` with the filter ops attached.

### `add_backdrop_filter` (the workhorse)

`scene_building.rs:3582` (called from `scene_building.rs:1759`). It builds the following in order:

1. A **shared `clip_leaf_id`** (`scene_building.rs:3600`) used by *both* the `BackdropCapture` and the `BackdropRender` primitives. This is deliberate — if the two sides could be culled differently, one would expect output the other never produced.

2. A `BackdropCapture` primitive (`prim_store/backdrop.rs`). It's an empty struct — purely a marker whose only job is to fire `register_resolve_source()` later.

3. A `PictureChainBuilder` rooted at that capture, constructed with `is_sub_graph = true`. That flag has two follow-on effects (`scene_building.rs:230–339`):
   - the **first** picture added to the chain is tagged `PictureFlags::IS_RESOLVE_TARGET` (this is where the resolve blit will land);
   - the **last** picture in the chain is tagged `PictureFlags::IS_SUB_GRAPH` (this is what `SurfaceBuilder` recognises on pop to perform the splice in §3).

4. The requested filters are applied by `wrap_prim_with_filters`. For CSS filter functions, each non-noop filter op wraps the current source in another `Picture` with `PictureCompositeMode::Filter(..)` or `ComponentTransferFilter(..)`. For an SVG filter graph the whole graph becomes a single `Picture` with `PictureCompositeMode::SVGFEGraph(..)`. For backdrop filters there's a special case (`scene_building.rs:4275`): a `Filter::Blur` gets `should_inflate = false` and `edge_mode = BlurEdgeMode::Mirror`. The bounds aren't inflated because the capture rect already defines what's available; mirroring is the closest analogue to "what's actually behind the element" when the blur kernel walks off the captured region.

5. An outer `PictureCompositeMode::IntermediateSurface` wrapping the whole filter chain (`scene_building.rs:3649`). This is the picture whose render-task output `BackdropRender` will sample.

6. The filter chain is attached to the nearest stacking context on the stack whose flags do **not** include `WRAPS_BACKDROP_FILTER` (`scene_building.rs:3674`, via `rposition`); if no such stacking context exists it is added to the root tile cache instead. The exact rule is just that: *nearest non-`WRAPS_BACKDROP_FILTER` SC, or tile cache root*. Depending on what's on the stack, the landing site can be the current stacking context, can skip past several wrapper contexts, or can fall through to the tile cache root.

7. A `BackdropRender` primitive is pushed at the original (un-hoisted) location with `pic_index` pointing to the IntermediateSurface picture and the same `clip_leaf_id` as the capture. This is the only thing inserted into the *draw order* of the element's own stacking context.

So at the end of scene building you have, conceptually:

```
parent_sc  (no WRAPS_BACKDROP_FILTER flag)
├── … earlier prims …
├── IntermediateSurface           (IS_SUB_GRAPH at the chain root)
│   └── Filter(Brightness)
│       └── Filter(Blur, Mirror edge, no inflate, IS_RESOLVE_TARGET)
│           └── BackdropCapture
├── element's own stacking context (WRAPS_BACKDROP_FILTER)
│   ├── BackdropRender → picks up output of outer IntermediateSurface
│   └── … element's painted content …
└── … later prims …
```

## 3. Visibility, tile marking, and the dynamic-content split

Before frame building, two book-keeping steps happen during tile-cache visibility (`tile_cache/mod.rs`):

- **`scratch.frame.required_sub_graphs.insert(pic_index)`** (`tile_cache/mod.rs:2646`, set defined at `prim_store/mod.rs:788`). Hidden `IS_SUB_GRAPH` pictures are skipped during `prepare` unless their `BackdropRender` is visible and has placed them in this set. No visible render → no sub-graph built.
- **Per-tile `sub_graphs` list** on `CachedSurface` (`invalidation/cached_surface.rs:35`), populated at `tile_cache/mod.rs:2662`:
  ```rust
  pub sub_graphs: Vec<(PictureRect, Vec<(PictureCompositeMode, SurfaceIndex)>)>;
  ```
  Each entry is `(coverage_rect, surface_stack)`. The presence of any entry forces the tile into the dynamic-content shape (below); the `surface_stack` is replayed to expand the dynamic content rect so e.g. a blur has enough source pixels outside the dirty rect.

**Tile shape with no backdrop filters:** primitives draw straight into the static picture-cache tile via `PictureCacheTargetKind::Draw`.

**Tile shape with ≥1 backdrop filter:** the tile becomes a two-task pipeline:

```
dynamic content picture task  ─►  static TileComposite task  ─►  persistent tile slot
```

The dynamic content task is what gets split by the sub-graph mechanism (next section). The `TileComposite` task is realised at render time as `PictureCacheTargetKind::Blit` and blits the dirty rect from dynamic-content → static tile with `TextureFilter::Nearest` (`renderer/mod.rs:2992`).

This `TileComposite` blit is **in addition to** the resolve blits and is per affected tile, not per filter.

## 4. Frame building — the sub-graph splice

The interesting bits live in `SurfaceBuilder` (`surface.rs`) and `prepare.rs`.

### `BackdropCapture` during `prepare`

`prepare.rs:1761`:

```rust
PrimitiveKind::BackdropCapture { .. } => {
    frame_state.surface_builder.register_resolve_source();
}
```

`register_resolve_source` stores the *current* parent surface's render task id into `SurfaceBuilder::resolve_source` for the sub-graph that's on top of the surface-builder stack. There is an assert that only one resolve source can be set per sub-graph — that matters in a moment.

### `pop_surface` — the actual splice

When the surface builder pops the IntermediateSurface that wraps the filter chain (`surface.rs:~450–830`), the four-step block comment summarises the wiring:

```
(a) Set up new task(s) on parent surface that write to the same location
(b) Set up a resolve target to copy from parent surface task(s) to the resolve target
(c) Make the old parent surface tasks input dependencies of the resolve target
(d) Make the sub-graph output an input dependency of the new task(s).
```

Concretely:

1. **Snapshot the existing parent task id** — call it `T_old`. (This is the *dynamic content* task on a tile-cached parent, not the static tile slot.)
2. **Create a brand-new picture task** `T_new` writing to the same destination as `T_old` (the same dynamic-content target). `T_new` gets a fresh command buffer.
3. **Attach a `ResolveOp`** to the resolve-target picture task inside the sub-graph (the `IS_RESOLVE_TARGET` one). `src_task_ids` includes `T_old`; `dest_task_id` is the resolve target. For a tiled parent, the loop iterates every tile descriptor in the parent tiled builder (`surface.rs:648`) and appends each tile's old task to `src_task_ids`, so a single backdrop-filter on a multi-tile parent produces **one** `ResolveOp` with multiple sources. The renderer's `handle_resolve` will only issue an actual `blit_render_target` for a source whose wanted destination rect intersects the source's available rect, so the realised blit count is "non-empty source intersections," which can be ≤ the length of `src_task_ids`.
4. **`rg_builder.add_dependency(T_new, T_old)`** (`surface.rs:696`/`:762`) — explicit scheduling edge so `T_new` runs after `T_old`.
5. **Make the sub-graph's output task an input dependency of `T_new`.**
6. **Overwrite `*parent_task_id = T_new`** so every primitive prepared *after* the sub-graph appends to `T_new`'s command buffer.
7. **Record `sub_graph_output_map[pic_index] = output_task_id`** so the matching `BackdropRender` can find it.

### `BackdropRender` during `prepare`

`prepare.rs:1777`:

```rust
PrimitiveKind::BackdropRender { pic_index, .. } => {
    let sub_graph_output_id = frame_state.surface_builder
        .sub_graph_output_map.get(pic_index).cloned();
    frame_state.surface_builder.add_child_render_task(sub_graph_output_id, ..);
    // build UV mapping for the captured rect and emit a quad
    let pattern = BackdropPattern { src_task_id: sub_graph_output_id, uvs };
    quad::prepare_quad(&pattern, ..);
}
```

This emits a normal textured quad drawn into `T_new` (the current parent task). The shader is `gfx/wr/webrender/res/ps_quad_backdrop.glsl`, which bilinearly interpolates four homogeneous screen-space UV corners across the primitive, maps them into the source task's texture-cache UV rect, clamps, and samples through `sColor0`. If the capture's clip rect was off-screen and the sub-graph wasn't built, `sub_graph_output_map.get` returns `None` and the draw is dropped.

## 5. The render-task graph for one backdrop-filter on a tile

For `blur(8px)` on one tile, with the tile-shape change in §3 included:

```
                ┌──────────────────────────────┐
                │  T_old: dynamic-content task │
                │  (prims before the filter)   │
                └──────────────┬───────────────┘
                               │ src of ResolveOp,
                               │ + explicit add_dependency(T_new, T_old)
                               ▼
                ┌──────────────────────────────┐
                │  resolve target picture task │
                │  (IS_RESOLVE_TARGET)         │
                │  ResolveOp: blit T_old → me  │   ← resolve blit (Linear)
                └──────────────┬───────────────┘
                               ▼
                ┌──────────────────────────────┐
                │  blur passes (Mirror edge)   │
                └──────────────┬───────────────┘
                               ▼
                ┌──────────────────────────────┐
                │  outer IntermediateSurface   │
                │  (IS_SUB_GRAPH) output       │
                └──────────────┬───────────────┘
                               │ input dependency of T_new
                               ▼
                ┌──────────────────────────────┐
                │  T_new: dynamic-content task │
                │  – BackdropRender quad here  │
                │  – prims after the filter    │
                └──────────────┬───────────────┘
                               ▼
                ┌──────────────────────────────┐
                │  TileComposite task          │
                │  Blit dyn → tile (Nearest)   │   ← TileComposite blit
                └──────────────┬───────────────┘
                               ▼
                       static picture-cache tile slot
```

`T_old` and `T_new` both target the same *dynamic-content* surface; the *tile* texture is updated exactly once at the end by `TileComposite`.

## 6. Multiple backdrop-filters on the same tile

Each pair of `BackdropCapture` / `BackdropRender` triggers its own `pop_surface` splice, and the splice **overwrites `parent_task_id` each time**. The recursion bottoms out at:

```
T_0  ── prims before filter A
  │
  └─► [resolve A: T_0 → R_A] ─► … filter A chain … ─► O_A
  │
T_1  ── continues into same dynamic-content surface
  │     ── BackdropRender(A) draws O_A
  │     ── prims between A and B
  │
  └─► [resolve B: T_1 → R_B] ─► … filter B chain … ─► O_B
  │
T_2  ── continues into same dynamic-content surface
  │     ── BackdropRender(B) draws O_B
  │     ── trailing prims
  ▼
TileComposite: T_2 → tile slot
```

When `BackdropCapture` for filter B fires `register_resolve_source()`, the surface's current task is `T_1`, **not `T_0`**. So filter B's `ResolveOp` reads from `T_1` — which already contains filter A's composited result. This is the CSS-correct semantics: B sees A. The `assert_eq!(builder.resolve_source, None)` in `register_resolve_source` is per sub-graph (reset on each push), so successive backdrop-filters in the same tile don't collide.

## 7. Accounting for every additional resolve / blit

For a single picture-cache tile with **N** backdrop-filters on it:

| Operation | Count | Filter mode | Where it lives |
|---|---|---|---|
| Dynamic-content picture tasks (segments) | N + 1 | — | `surface.rs` `pop_surface` |
| **Resolve blits** (`ResolveOp` → `device.blit_render_target`) | **one per non-empty source intersection** across `src_task_ids` (`src_task_ids` itself lists every parent tile descriptor; the blit only fires where the wanted dest rect intersects the source's available rect) | **`Linear`** | `renderer/mod.rs:2778, 2860` |
| Filter chain internal tasks | per-filter (e.g. blur = zero or more downscale/scaling tasks, then V + H — `RenderTask::new_blur` at `render_task.rs:1008` only inserts downscales while the std deviation is still large enough) | — | `render_task.rs`, `picture_composite_mode.rs` |
| `BackdropRender` quad draws | N | — (`ps_quad_backdrop.glsl`) | `prepare.rs:1777` |
| **`TileComposite` blit** (dyn-content → static tile) | **1 per affected tile**, not per filter | **`Nearest`** | `renderer/mod.rs:2992` |
| MSAA resolves | 0 | — | picture cache targets are not MSAA |
| Framebuffer readbacks | 0 for `backdrop-filter` | — | `RenderTaskKind::Readback` / `handle_readback_composite` is the mix-blend fallback only |

So the "extra" cost per backdrop-filter, beyond the filter passes themselves, is:

- **One resolve blit per overlapping parent tile** (linear). A backdrop-filter spanning M parent tiles produces M blits from a single `ResolveOp`.
- **One additional dynamic-content task split** in the parent (no copy — `T_new` writes to the same target as `T_old`, the explicit `add_dependency` just orders them).

And per affected tile, regardless of N:

- **One `TileComposite` blit** (nearest) from the dynamic-content surface to the static tile slot.

There is no MSAA resolve in this path, and no framebuffer-to-texture readback. The `Readback` task kind / `handle_readback_composite` exists only for the mix-blend-mode fallback when the GPU can't do the blend directly; it is not part of the backdrop-filter pipeline. (If a mix-blend appears *inside* a backdrop sub-graph it would still take that path, independently of the resolve mechanism here.)

## 8. Key files / lines

- Display item entry: `webrender_api/src/display_list.rs:1662`
- Scene-building dispatch: `scene_building.rs:1759`
- `add_backdrop_filter`: `scene_building.rs:3582`
  - Shared clip leaf: `:3600`; capture prim: `:3611`; filter chain wrap: `:3636`; IntermediateSurface: `:3649`; hoist to non-`WRAPS_BACKDROP_FILTER` SC: `:3674`; render prim: `:3708`.
- `PictureChainBuilder` sub-graph plumbing (`is_sub_graph`, `IS_RESOLVE_TARGET`, `IS_SUB_GRAPH`): `scene_building.rs:230–339`.
- Backdrop blur special case (Mirror edge, no inflate): `scene_building.rs:4275`.
- Primitives: `prim_store/backdrop.rs`.
- Visibility / tile marking:
  - `required_sub_graphs` defined at `prim_store/mod.rs:788`, inserted at `tile_cache/mod.rs:2646`.
  - Per-tile `sub_graphs` list: `invalidation/cached_surface.rs:35`, populated at `tile_cache/mod.rs:2662`.
- Capture in prepare: `prepare.rs:1761`; render in prepare: `prepare.rs:1777`.
- Sub-graph splice (the heart of it): `surface.rs` — `register_resolve_source`, `pop_surface` (~450–830). The four-step `(a)…(d)` block comment is the design spec. Explicit `add_dependency` at `:696` (simple surface) and `:762` (tiled).
- `ResolveOp` struct (`src_task_ids: Vec<RenderTaskId>`): `render_target.rs:580`.
- Resolve execution: `renderer/mod.rs` `handle_resolves` / `handle_resolve` (~2772–2864), `blit_render_target(..., TextureFilter::Linear)` at `:2860`.
- `TileComposite` execution: `renderer/mod.rs` `draw_picture_cache_target` (~2866), `PictureCacheTargetKind::Blit` branch, `blit_render_target(..., TextureFilter::Nearest)` at `:2992`. Task struct at `render_task.rs:215`.
- Filter task creation: `picture_composite_mode.rs:564` (`Filter` arm of `prepare_composite_mode`).
- Shader: `gfx/wr/webrender/res/ps_quad_backdrop.glsl`.

If you want to step further, the most rewarding single read is `SurfaceBuilder::pop_surface` — every other piece exists to feed into the (a)–(d) splice it performs.
