# NestCraft 3D Nesting — Migration Architecture & Landmine Audit

> **Naming & Project Format:** Renamed to **NestCraft** (formerly *SolidCraft SLS*) to separate it from a slicer and allow a broader nesting & build-preparation scope, with project file extension **`.nestc`** (v2 ZIP + SHA-256 content-addressed binary `.nmesh` entries, replacing `.scsls`). Target workspace members: `crates/nestcraft-core` + `apps/nestcraft`.
>
> **Related Docs:** [`sls-3d-nesting-plan.md`](sls-3d-nesting-plan.md) · [`sls-feature-matrix.md`](sls-feature-matrix.md) · [`sls-nesting-plan.md`](sls-nesting-plan.md)

- **Repository:** `yuvalabou/solidcraft` (tracking upstream `tangency-labs/solidcraft`)
- **Target Baseline:** SolidCraft `v0.46.2` (Rust 2024 edition, MSRV `1.95`)
- **Licensing Strategy:** **Internal / Upstream-Ready Fork (`LICENSE-BRAND` Clause 2a/2b)** — full `solidcraft-ui-kit`, `solidcraft-look`, and `solidcraft-theme` integration
- **Workspace Topology:** `crates/nestcraft-core` (`Class::Shared`, pure Rust) + `apps/nestcraft` (`Class::Exempt` binary & MCP host)
- **Target Machines:** EOS Formiga P110 / P110 Velocis / P100 (`200 × 250 × 330 mm`, Front-Left), Farsoon HT403P / SS403P (`400 × 400 × 450 mm`, Center), PA12 sintered parts

---

## 1. Executive Architecture

Instead of modifying SolidCraft's parametric B-Rep application (`apps/solidcraft`), this project lives as isolated workspace members inside the fork—making it trivial to stay synced with `upstream/main` and allowing `tangency-labs` to upstream `crates/nestcraft-core` or an additive workspace (`ws_additive`) cleanly if desired:

```
solidcraft/
├── crates/
│   ├── viewport/          # Shared (MIT/Apache-2.0) — camera, projection, picking rays, Scene
│   ├── viewport-gpu/      # Shared (MIT/Apache-2.0) — wgpu pipelines, InstancedMesh (INSTANCE_SIZE = 52)
│   ├── viewport-egui/     # Shared (MIT/Apache-2.0) — egui-wgpu paint callbacks
│   ├── ui-kit/            # Shared (LICENSE-BRAND Clause 2a/2b) — Container, Panel, Ribbon, IconSet
│   ├── look/              # Shared (LICENSE-BRAND Clause 2a/2b) — SVG icon masters, palette.toml, IBM Plex
│   ├── theme/             # Shared (LICENSE-BRAND Clause 2a/2b) — light/dark Tokens & semantic ramps
│   └── nestcraft-core/          # NEW (Class::Shared) — pure-Rust mesh I/O, BVH, 3D EDT voxelization, 2D/3D packing, interlocking, V-HACD settle, collision, Sinterbox
├── apps/
│   ├── solidcraft/        # Untouched upstream CAD app
│   └── nestcraft/    # NEW (Class::Exempt) — SolidCraft UI-kit SLS nesting app + `--mcp` headless server
├── attribution/pending/
│   └── nestcraft.md             # Pending asset attribution rows (avoids merge conflicts on ATTRIBUTION.md)
└── docs/
    ├── sls-3d-nesting-plan.md
    ├── sls-feature-matrix.md
    ├── sls-nesting-migration.md
    └── sls-nesting-plan.md
```

### Dependency Boundary Rules
1. **`crates/nestcraft-core` has ZERO UI/GPU dependencies.** No `egui`, `wgpu`, `eframe`, `rfd`, or `solidcraft-ui-kit`. Both `apps/nestcraft` (GUI) and `--mcp` (headless automation) drive the exact same `nestcraft-core` engine.
2. **Reuse `viewport*` and `ui-kit`/`look`/`theme`, NOT `solidcraft-io` or `solidcraft-engine`.** `crates/io` (`solidcraft-io`) pulls in `solidcraft-kernel`, `solidcraft-doc`, `solidcraft-sketch`, and `solidcore-ops` (the full B-Rep stack). By contrast, `solidcraft-viewport`, `solidcraft-viewport-gpu`, and `solidcraft-viewport-egui` depend only on `solidcore-record` (`Vec3`), `solidcore-tess` (`Mesh`), `solidcraft-color`, `wgpu` (`30`), and `egui` (`0.37`).
3. **Hardware Instancing is Already Built In:** `crates/viewport-gpu/src/view.rs` provides `InstancedMesh` (`INSTANCE_SIZE = 52`: 3×4 rigid transform rows + `PaletteIndex`). Hundreds of duplicate SLS parts share one `Arc<Mesh>` / GPU vertex buffer with per-instance transform matrices and red/gray clearance coloring.
4. **Native `Container` Compliance:** Routing all windows, panels, menus, and tooltips in `apps/nestcraft` through `solidcraft_ui_kit::Container` and `BackingTooltipExt` satisfies `cargo xtask ui-containers` out of the box—no AST guard exemptions required.

---

## 2. UI & Icon Strategy (`ui-kit` + `IconSet::extended`)

Because this fork operates internally under `LICENSE-BRAND` Clause 2a (personal/internal fork builds) and Clause 2b (potential upstream contribution), `apps/nestcraft` inherits SolidCraft's entire visual system without copying or modifying `crates/look`:

### Zero-Touch Icon Extension
`crates/ui-kit/src/icons/svg.rs` provides `IconSet::extended(data, extension)` and `crates/look/icons/design/palette.toml` already defines the **`mesh`** (yellow-gold), **`constraint`** (red), and **`surface`** (orange) accent ramps. Furthermore, `crates/look/icons/design/svg/model/` already includes CAD/mesh SVG masters covering almost every SLS operation:

| Semantic SLS Action (`apps/nestcraft`) | Existing Master (`crates/look/icons/design/svg/model/`) | Accent (`palette.toml`) |
| :--- | :--- | :--- |
| `sls_import_mesh` | `mesh_body` | `mesh` |
| `sls_auto_nest` | `bounding_solid` | `mesh` |
| `sls_sinterbox` | `perforate` | `mesh` |
| `sls_orient_lock` | `print_face` | `mesh` |
| `sls_duplicate` | `pattern_rect` | `mesh` |
| `sls_emboss_id` | `emboss` | `mesh` |
| `sls_hollow` | `shell` | `mesh` |
| `sls_slice_z` | `section` | `mesh` |
| `sls_collision` | `interference` | `constraint` |
| `sls_thermal_map` | `curvature_map` | `surface` |

Register in `apps/nestcraft` without touching `crates/look`:

```rust
use solidcraft_ui_kit::icons::{self, IconExtension, IconSet};

static NESTCRAFT_ICONS: IconSet = IconSet::extended(
    solidcraft_look::icons::DATA,
    IconExtension {
        masters: &[], // add SLS-specific SVGs later only if needed
        aliases: &[
            ("sls_import_mesh", "mesh_body", "mesh"),
            ("sls_auto_nest", "bounding_solid", "mesh"),
            ("sls_sinterbox", "perforate", "mesh"),
            ("sls_orient_lock", "print_face", "mesh"),
            ("sls_duplicate", "pattern_rect", "mesh"),
            ("sls_emboss_id", "emboss", "mesh"),
            ("sls_hollow", "shell", "mesh"),
            ("sls_slice_z", "section", "mesh"),
            ("sls_collision", "interference", "constraint"),
            ("sls_thermal_map", "curvature_map", "surface"),
        ],
        icon_accents: &[],
    },
);

// Startup initialization (replaces solidcraft_look::install(&ctx)):
// solidcraft_look::glyphs::install();
// solidcraft_look::themes::install();
// icons::install(&NESTCRAFT_ICONS);
// solidcraft_look::fonts::install(&ctx);
```

*(Note: `solidcraft_look::install_tables()` calls `icons::install(&SET)` first, and `OnceLock` ignores any subsequent `icons::install` call—so call `icons::install(&NESTCRAFT_ICONS)` **before** or **instead of** `solidcraft_look::icons::install()`.)*

---

## 3. Codebase Landmines & Mitigations

| Landmine | Source in Repo | Why It Breaks & Mitigation |
| :--- | :--- | :--- |
| **`LICENSE-BRAND` Scope** | `LICENSE-BRAND` | Clause 2a allows running/modifying `ui-kit`, `look`, and `theme` inside your own SolidCraft fork; Clause 2b covers upstream contributions; **Clause 3a/3c prohibits external standalone redistribution** under SolidCraft's brand/look without written permission. Keep builds internal unless contributed upstream. |
| **`OnceLock` Icon Ordering** | `crates/ui-kit/src/icons/mod.rs` | `solidcraft_look::install()` locks `solidcraft_look::icons::SET` into `INSTALLED: OnceLock<&'static IconSet>`, causing later `icons::install(&NESTCRAFT_ICONS)` calls to be ignored. Install `&NESTCRAFT_ICONS` explicitly alongside `glyphs::install()`, `themes::install()`, and `fonts::install(ctx)`. |
| **AST Container Guard** | `xtask/src/ui_containers.rs` | `syn` parses every `.rs` file in `crates/**` and `apps/**` (including macros) and fails if any code outside `crates/ui-kit/src/container.rs` calls `egui::Window`, `Area`, `Panel`, `SidePanel`, `TopBottomPanel`, `CentralPanel`, `Popup`, `SubMenu`, or `ui.menu_button`. Use `solidcraft_ui_kit::Container` exclusively in `apps/nestcraft`. |
| **Layering Registry** | `xtask/src/layers.rs` | Root `Cargo.toml` globs `crates/*` and `apps/*`. Unregistered crates fail `cargo xtask layers` (`Violation::Unregistered`). `UI_MIN_LAYER = 5` forbids `egui`/`wgpu`/`rfd` below Layer 5. Register `("nestcraft-core", Class::Shared)` and `("nestcraft", Class::Exempt)` in `xtask/src/layers.rs::TABLE`. |
| **Asset Provenance Gate** | `xtask/src/assets.rs` | Any committed `.png`, `.svg`, `.ico`, `.ttf`, `.stl`, `.step`, `.obj` outside `solidcore/`/`vendor/` fails `cargo xtask assets` unless listed in `ATTRIBUTION.md` or `attribution/pending/*.md`. Log any new SLS assets in `attribution/pending/nestcraft.md` (prevents merge conflicts on `ATTRIBUTION.md`). |
| **Strict Lints** | `Cargo.toml`, `crates/viewport/src/lib.rs` | Workspace sets `unsafe_code = "forbid"`, and core crates apply `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]`. Use `thiserror = "2"` + `?` everywhere. |
| **`crates/io` B-Rep Coupling** | `crates/io/Cargo.toml` | `solidcraft-io` pulls in `solidcraft-kernel`, `solidcraft-doc`, `solidcraft-sketch`, and `solidcore-ops`. Do not import `solidcraft-io` into `nestcraft-core`; write a lean pure-mesh STL/3MF module using workspace `zip = "9"` + `quick-xml = "0.41"` (modeling build-item transforms on `crates/io/src/threemf.rs`). |
| **`jagua-rs` is 2D Only** | External ecosystem | `jagua-rs` and `sparrow` handle 2D irregular polygons only. Build the 3D clearance/packing kernel over BVH + 3D Euclidean Distance Fields from `ferroform` (pure Rust). |

---

## 4. 3D Nesting Engine Architecture (`nestcraft-core`)

> See [`docs/sls-3d-nesting-plan.md`](sls-3d-nesting-plan.md) for the complete 3-stage hybrid specification, memory/CPU guardrails, and UI controls.

Rather than writing geometry math from scratch or importing `jagua-rs` (2D), `nestcraft-core` implements pure-Rust primitives aligned with SolidCraft's license and crash-safety rules:

1. **Mesh Ingestion & Repair Gate (`io/`, `manifold.rs` — `Done`):**
   - **Binary/ASCII STL, 3MF & `.nestc` v2:** Pure-mesh parsers/writers using workspace `zip = "9"` + `quick-xml = "0.41"` + `sha2` (content-addressed binary `.nmesh` blobs in `.nestc` v2) without pulling in `solidcraft-doc`.
   - **Manifold & Winding Check (`manifold.rs`):** Edge-incidence, winding, shell counting, and degenerate-triangle checks to flag non-manifold customer STLs and split multi-shell parts.
2. **Exact 3D Sintering Clearance via 3D EDT (`voxel.rs` — `Done`):**
   - **Conservative Flood-Fill & 3D EDT Pipeline:**
     1. Voxelize each unique `(PartPrototype, orientation)` at configurable pitch (`2.0 ↔ 0.5 mm`) using conservative triangle-AABB surface rasterization + exterior 3D flood-fill (open cavities stay open unless filled; non-manifold-safe).
     2. Compute the **Felzenszwalb-Huttenlocher 3D Euclidean Distance Transform** (separable 1D lower-envelope passes) with optional **anisotropic XY/Z spacing** and **adaptive part-mass (`r_A + r_B`)** half-clearance.
     3. Generate two `u64` bitpacked `BitGrid3D` masks per symmetry-pruned orientation:
        - `solid_mask` (`EDT == 0`) — exact occupancy (interlocking escape, cavity checks, thermal density).
        - `clearance_mask` (`EDT <= r_P`) — tight dilated shell for `O(1)` bitwise `&` collision probing, paired with a `2D XY HeightMap` and `CpuBudget` / `try_reserve` RAM guardrails.
3. **3D Placement, Interlocking, Physics Compaction & Exact Verification:**
   - **Stage 1 (`nest3d.rs` — `Done`):** Parallel 6-portfolio constructive 3D Bottom-Left-Back-Fill (`Z`-first) respecting `RotationConstraint`, `max_z_limit_mm`, `selected_only`, `keep_in_bounds_fixed`, `NoBuildZone`, and open-cavity-safe topological interlocking rejection (`interlock.rs` — `Done`).
   - **Stage 2 (`nest3d.rs` — `Done`, Phase 4):** Anytime parallel islands combining Ruin-and-Recreate + GA crossover with live viewport streaming, `Per-Part` / `Override All` orientation mode with `XY Rotation` & `Z Rotation` step sliders (`0° (Locked)`) + conditional `PCA / OBB Pre-Alignment`, and optional `20×20×20 mm` thermal density and slice-smoothness penalties.
   - **Stage 3 (`compact.rs` — `Done`):** `rapier3d` `0.33` + `parry3d` `0.31` V-HACD compound convex-hull `-Z` gravity compaction (with optional micro-rotations), also available standalone as **Gravity Settle**.
   - **Exact Final Gate (`collision.rs`, `voxel.rs::MeshBvh` — `Done`):** Incremental `CollisionCache` + exact triangle-level clearance verification (including anisotropic/adaptive rules and `NoBuildZone`) before committing or exporting 3MF.

---

## 5. Phased Implementation Roadmap

### Phase 0: Git & Workspace Hygiene (`Done`)
- Keep `main` tracking upstream-compatible docs/mirrors; all SLS implementation lives on `sls-dev`.
- Registered `("nestcraft-core", Class::Shared)` and `("nestcraft", Class::Exempt)` in `xtask/src/layers.rs::TABLE`.

### Phase 1: `nestcraft-core` Engine & 3D Nesting Core (`Done` — Stage 1–3 Packing & Refinement Complete; Automated Benchmarks `Planned`)
- Implemented STL/3MF/`.nestc` v2 I/O, 5 predefined SLS machines + Custom, `manifold.rs`, `collision.rs`, `nest2d.rs`, `voxel.rs` (flood-fill + 3D EDT), `interlock.rs`, `nest3d.rs` (Stage 1 multi-order BLBF, Stage 2 parallel island GA/Ruin-and-Recreate with thermal/slice refinement & live viewport streaming, + exact gate), and `compact.rs` (`rapier3d`/`parry3d` V-HACD).
- Remaining: automated 30–150 part Netfabb benchmarks (`Phase 6` in [`sls-3d-nesting-plan.md`](sls-3d-nesting-plan.md)).

### Phase 2: `solidcraft-ui-kit` + `solidcraft-viewport-gpu` App (`Done`)
- Scaffolded `apps/nestcraft` using `solidcraft_ui_kit::Container`, `solidcraft_look` themes/fonts/glyphs, and `&NESTCRAFT_ICONS` (`IconSet::extended`).
- Rendered build volume wireframe, `Z = 0` floor grid, instanced parts via `InstancedMesh` (`Arc<Mesh>` + 3×4 affine matrix + selection/out-of-bounds/thermal/collision palettes), `MovingLayer` drag/rotate gizmo, 3D View Compass, compact fixed-width scrollable `3D Auto-Nest` (with `XY`/`Z` rotation step sliders including `0° (Locked)`) & `Gravity Settle` modals, and non-blocking bottom progress bar with `Stop & Apply Best` / `Cancel`.

### Phase 3: SLS Production Tooling & `--mcp` Automation (`Partial` — Sinterbox, Thermal & Slice `Done`; `--mcp` & Nightly CI `Planned`)
- **Sinterbox Generator (`sinterbox.rs` — `Done`):** Parametric perforated cage wrapper around grouped small parts (`Box` / `Tight Fit`, adaptive bars, 3D label).
- **Thermal Density Heatmap & Slice Distribution (`thermal.rs`, `slice.rs` — `Done`):** `20×20×20 mm` sliding-box thermal map + `0.1–5.0 mm` interactive Z-slice sidebar (`Z`-clip plane pending).
- **Headless MCP Server (`nestcraft --mcp` — `Planned`):** Expose `load_job`, `nest_job`, `split_builds`, and `export_3mf` over `solidcraft-mcp-server`.
- **Upstream Path (Optional):** Keep `crates/nestcraft-core` (`Class::Shared`) UI-agnostic so it can integrate cleanly into `apps/solidcraft` as a `ws_additive` workspace tab alongside `ws_design`, `ws_drawing`, `ws_render`, and `ws_animate`.

---

## 6. Git & Upstream Sync Workflow

```powershell
# 1. Sync main with upstream SolidCraft
git checkout main
git fetch upstream
git pull upstream main
git push origin main

# 2. Merge or rebase updates into sls-dev
git checkout sls-dev
git merge main   # or: git rebase main
```

- **Rule:** Never edit `apps/solidcraft/`, `crates/engine/`, or `crates/look/` on `sls-dev`. All new code lives inside `crates/nestcraft-core/` and `apps/nestcraft/` (plus 2 lines in `xtask/src/layers.rs`).
- **Nightly CI:** Add `.github/workflows/sls-nightly.yml` targeting `sls-dev` running `cargo build --release -p nestcraft`.
