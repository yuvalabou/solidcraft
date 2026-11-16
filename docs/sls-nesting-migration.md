# SolidCraft SLS 3D Nesting — Migration Architecture & Landmine Audit

- **Repository:** `yuvalabou/solidcraft` (tracking upstream `tangency-labs/solidcraft`)
- **Target Baseline:** SolidCraft `v0.43.0` (Rust 2024 edition, MSRV `1.95`)
- **Licensing Strategy:** **Internal / Upstream-Ready Fork (`LICENSE-BRAND` Clause 2a/2b)** — full `solidcraft-ui-kit`, `solidcraft-look`, and `solidcraft-theme` integration
- **Workspace Topology:** `crates/sls-core` (`Class::Shared`, pure Rust) + `apps/solidcraft-sls` (`Class::Exempt` binary & MCP host)
- **Target Machines:** EOS Formiga P110 (`160 × 210 × 330 mm`), Farsoon HT403P (`400 × 400 × 450 mm`), PA12 sintered parts

---

## 1. Executive Architecture

Instead of modifying SolidCraft's parametric B-Rep application (`apps/solidcraft`), this project lives as isolated workspace members inside the fork—making it trivial to stay synced with `upstream/main` and allowing `tangency-labs` to upstream `crates/sls-core` or an additive workspace (`ws_additive`) cleanly if desired:

```
solidcraft/
├── crates/
│   ├── viewport/          # Shared (MIT/Apache-2.0) — camera, projection, picking rays, Scene
│   ├── viewport-gpu/      # Shared (MIT/Apache-2.0) — wgpu pipelines, InstancedMesh (INSTANCE_SIZE = 52)
│   ├── viewport-egui/     # Shared (MIT/Apache-2.0) — egui-wgpu paint callbacks
│   ├── ui-kit/            # Shared (LICENSE-BRAND Clause 2a/2b) — Container, Panel, Ribbon, IconSet
│   ├── look/              # Shared (LICENSE-BRAND Clause 2a/2b) — SVG icon masters, palette.toml, IBM Plex
│   ├── theme/             # Shared (LICENSE-BRAND Clause 2a/2b) — light/dark Tokens & semantic ramps
│   └── sls-core/          # NEW (Class::Shared) — pure-Rust mesh I/O, BVH, 3D EDT voxelization, 3D packer, Sinterbox
├── apps/
│   ├── solidcraft/        # Untouched upstream CAD app
│   └── solidcraft-sls/    # NEW (Class::Exempt) — SolidCraft UI-kit SLS nesting app + `--mcp` headless server
├── attribution/pending/
│   └── sls.md             # Pending asset attribution rows (avoids merge conflicts on ATTRIBUTION.md)
└── docs/
    └── sls-nesting-migration.md
```

### Dependency Boundary Rules
1. **`crates/sls-core` has ZERO UI/GPU dependencies.** No `egui`, `wgpu`, `eframe`, `rfd`, or `solidcraft-ui-kit`. Both `apps/solidcraft-sls` (GUI) and `--mcp` (headless automation) drive the exact same `sls-core` engine.
2. **Reuse `viewport*` and `ui-kit`/`look`/`theme`, NOT `solidcraft-io` or `solidcraft-engine`.** `crates/io` (`solidcraft-io`) pulls in `solidcraft-kernel`, `solidcraft-doc`, `solidcraft-sketch`, and `solidcore-ops` (the full B-Rep stack). By contrast, `solidcraft-viewport`, `solidcraft-viewport-gpu`, and `solidcraft-viewport-egui` depend only on `solidcore-record` (`Vec3`), `solidcore-tess` (`Mesh`), `solidcraft-color`, `wgpu` (`30`), and `egui` (`0.37`).
3. **Hardware Instancing is Already Built In:** `crates/viewport-gpu/src/view.rs` provides `InstancedMesh` (`INSTANCE_SIZE = 52`: 3×4 rigid transform rows + `PaletteIndex`). Hundreds of duplicate SLS parts share one `Arc<Mesh>` / GPU vertex buffer with per-instance transform matrices and red/gray clearance coloring.
4. **Native `Container` Compliance:** Routing all windows, panels, menus, and tooltips in `apps/solidcraft-sls` through `solidcraft_ui_kit::Container` and `BackingTooltipExt` satisfies `cargo xtask ui-containers` out of the box—no AST guard exemptions required.

---

## 2. UI & Icon Strategy (`ui-kit` + `IconSet::extended`)

Because this fork operates internally under `LICENSE-BRAND` Clause 2a (personal/internal fork builds) and Clause 2b (potential upstream contribution), `apps/solidcraft-sls` inherits SolidCraft's entire visual system without copying or modifying `crates/look`:

### Zero-Touch Icon Extension
`crates/ui-kit/src/icons/svg.rs` provides `IconSet::extended(data, extension)` and `crates/look/icons/design/palette.toml` already defines the **`mesh`** (yellow-gold), **`constraint`** (red), and **`surface`** (orange) accent ramps. Furthermore, `crates/look/icons/design/svg/model/` already includes CAD/mesh SVG masters covering almost every SLS operation:

| Semantic SLS Action (`apps/solidcraft-sls`) | Existing Master (`crates/look/icons/design/svg/model/`) | Accent (`palette.toml`) |
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

Register in `apps/solidcraft-sls` without touching `crates/look`:

```rust
use solidcraft_ui_kit::icons::{self, IconExtension, IconSet};

static SLS_ICONS: IconSet = IconSet::extended(
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
// icons::install(&SLS_ICONS);
// solidcraft_look::fonts::install(&ctx);
```

*(Note: `solidcraft_look::install_tables()` calls `icons::install(&SET)` first, and `OnceLock` ignores any subsequent `icons::install` call—so call `icons::install(&SLS_ICONS)` **before** or **instead of** `solidcraft_look::icons::install()`.)*

---

## 3. Codebase Landmines & Mitigations

| Landmine | Source in Repo | Why It Breaks & Mitigation |
| :--- | :--- | :--- |
| **`LICENSE-BRAND` Scope** | `LICENSE-BRAND` | Clause 2a allows running/modifying `ui-kit`, `look`, and `theme` inside your own SolidCraft fork; Clause 2b covers upstream contributions; **Clause 3a/3c prohibits external standalone redistribution** under SolidCraft's brand/look without written permission. Keep builds internal unless contributed upstream. |
| **`OnceLock` Icon Ordering** | `crates/ui-kit/src/icons/mod.rs` | `solidcraft_look::install()` locks `solidcraft_look::icons::SET` into `INSTALLED: OnceLock<&'static IconSet>`, causing later `icons::install(&SLS_ICONS)` calls to be ignored. Install `&SLS_ICONS` explicitly alongside `glyphs::install()`, `themes::install()`, and `fonts::install(ctx)`. |
| **AST Container Guard** | `xtask/src/ui_containers.rs` | `syn` parses every `.rs` file in `crates/**` and `apps/**` (including macros) and fails if any code outside `crates/ui-kit/src/container.rs` calls `egui::Window`, `Area`, `Panel`, `SidePanel`, `TopBottomPanel`, `CentralPanel`, `Popup`, `SubMenu`, or `ui.menu_button`. Use `solidcraft_ui_kit::Container` exclusively in `apps/solidcraft-sls`. |
| **Layering Registry** | `xtask/src/layers.rs` | Root `Cargo.toml` globs `crates/*` and `apps/*`. Unregistered crates fail `cargo xtask layers` (`Violation::Unregistered`). `UI_MIN_LAYER = 5` forbids `egui`/`wgpu`/`rfd` below Layer 5. Register `("sls-core", Class::Shared)` and `("solidcraft-sls", Class::Exempt)` in `xtask/src/layers.rs::TABLE`. |
| **Asset Provenance Gate** | `xtask/src/assets.rs` | Any committed `.png`, `.svg`, `.ico`, `.ttf`, `.stl`, `.step`, `.obj` outside `solidcore/`/`vendor/` fails `cargo xtask assets` unless listed in `ATTRIBUTION.md` or `attribution/pending/*.md`. Log any new SLS assets in `attribution/pending/sls.md` (prevents merge conflicts on `ATTRIBUTION.md`). |
| **Strict Lints** | `Cargo.toml`, `crates/viewport/src/lib.rs` | Workspace sets `unsafe_code = "forbid"`, and core crates apply `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]`. Use `thiserror = "2"` + `?` everywhere. |
| **`crates/io` B-Rep Coupling** | `crates/io/Cargo.toml` | `solidcraft-io` pulls in `solidcraft-kernel`, `solidcraft-doc`, `solidcraft-sketch`, and `solidcore-ops`. Do not import `solidcraft-io` into `sls-core`; write a lean pure-mesh STL/3MF module using workspace `zip = "9"` + `quick-xml = "0.41"` (modeling build-item transforms on `crates/io/src/threemf.rs`). |
| **`jagua-rs` is 2D Only** | External ecosystem | `jagua-rs` and `sparrow` handle 2D irregular polygons only. Build the 3D clearance/packing kernel over BVH + 3D Euclidean Distance Fields from `pyroid-core`. |

---

## 4. 3D Nesting Engine Architecture (`sls-core`)

Rather than writing geometry math from scratch or importing `jagua-rs` (2D), reuse proven Rust primitives already validated in our ecosystem (`pyroid-core` + `solidcraft`):

1. **Mesh Ingestion & Repair Gate:**
   - **Binary/ASCII STL & 3MF:** Pure-mesh parser in `sls-core` using workspace `zip = "9"` + `quick-xml = "0.41"` (mirroring the Production Extension `p:path` build-item logic in `crates/io/src/threemf.rs` without pulling in `solidcraft-doc`).
   - **Manifold & Winding Check:** Reuse the Farsoon BuildStar manifold/winding checks from `pyroid-core::manifold` (`raw/projects/GitHub/pyroid/crates/pyroid-core/src/manifold.rs`) to reject/flag non-manifold customer STLs up front (keeping Netfabb as an upstream repair pre-pass for broken meshes during transition).
2. **Exact 3D Sintering Clearance via 3D EDT (from `pyroid-core`):**
   - **Voxel + 3D EDT Pipeline:**
     1. Voxelize each unique part mesh at target pitch (`1.0 mm` / `0.5 mm`) using `pyroid-core::bvh` (Rayon-parallel BVH ray-cast containment).
     2. Compute the **Felzenszwalb-Huttenlocher 3D Euclidean Distance Transform** (`pyroid-core::edt`) once per unique part / quantized orientation.
     3. Generate two binary `u64` bitpacked voxel masks per orientation:
        - `solid_mask` (`EDT <= 0`) — actual part occupancy (thermal density & Sinterbox checks)
        - `clearance_mask` (`EDT <= clearance_mm / 2.0`, e.g., `1.25 mm` half-clearance for `2.5 mm` part-to-part gap) — dilated shell for `O(1)` bitwise `&` collision detection (`(a_word & b_word) != 0`).
3. **3D Placement & Optimization:**
   - **Stage 1 (Constructive):** Gravity / Bottom-Left-Back-Fill (`Z`-first) sorted by bounding volume / height, supporting per-part rotation constraints (`Free`, `ZOnly` / XY-strength lock, `Fixed`).
   - **Stage 2 (Metaheuristic):** Simulated Annealing / Tabu local search swapping/nudging parts to minimize `max_Z` (recoater time/cost driver) and penalizing excessive local voxel density (thermal hotspot / "egg effect" mitigation).
   - **Stage 3 (Exact Verification):** Final `parry3d` exact mesh-to-mesh clearance check on placed neighbors before committing or exporting 3MF.

---

## 5. Phased Implementation Roadmap

### Phase 0: Git & Workspace Hygiene
- Keep `main` tracking upstream-compatible docs/mirrors; all SLS implementation lives on `sls-dev`.
- Register `("sls-core", Class::Shared)` and `("solidcraft-sls", Class::Exempt)` in `xtask/src/layers.rs::TABLE`.

### Phase 1: Headless `sls-core` + Packer Benchmark (Zero UI)
- Scaffold `crates/sls-core`: STL + 3MF Production build-item I/O, Formiga P110 (`160×210×330 mm`) & Farsoon HT403P (`400×400×450 mm`) build volumes.
- Port/depend on `pyroid-core` (`bvh.rs`, `edt.rs`, `manifold.rs`) for `u64` bitpacked clearance masks.
- Implement Bottom-Left-Back-Fill + Simulated Annealing packer and benchmark packing density (%), `max_Z` (mm), and runtime (s) on a real 50–150 part job against Netfabb **before** investing in GUI panels.

### Phase 2: `solidcraft-ui-kit` + `solidcraft-viewport-gpu` App
- Scaffold `apps/solidcraft-sls` using `solidcraft_ui_kit::Container`, `Panel`, `ribbon`, `solidcraft_look` themes/fonts/glyphs, and `&SLS_ICONS` (`IconSet::extended`).
- Render build volume wireframe (`SceneLine` cube + `Z = 0` floor grid) and instanced parts via `InstancedMesh` (`Arc<Mesh>` + 3×4 affine matrix + normal/red collision palette).
- Add `solidcraft_viewport::Camera` ray + `parry3d` picking + manual translation/rotation Gizmo with live `parry3d` clearance checks.

### Phase 3: SLS Production Tooling & `--mcp` Automation (Upstreamable)
- **Sinterbox Generator:** Parametric perforated cage wrapper around tiny parts (`< X mm³` or `< Y mm` min dim).
- **Thermal Density Heatmap:** Sliding 3D box filter over `solid_mask` voxels highlighting PA12 curl/over-sintering zones.
- **Headless MCP Server (`solidcraft-sls --mcp`):** Expose `load_job` (folder + CSV/JSON BOM), `nest_job`, `split_builds`, and `export_3mf` over `solidcraft-mcp-server`.
- **Upstream Path (Optional):** If `tangency-labs` adopts additive manufacturing, `crates/sls-core` (`Class::Shared`) integrates cleanly into `apps/solidcraft` as a `ws_additive` workspace tab alongside `ws_design`, `ws_drawing`, `ws_render`, and `ws_animate`.

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

- **Rule:** Never edit `apps/solidcraft/`, `crates/engine/`, or `crates/look/` on `sls-dev`. All new code lives inside `crates/sls-core/` and `apps/solidcraft-sls/` (plus 2 lines in `xtask/src/layers.rs`).
- **Nightly CI:** Add `.github/workflows/sls-nightly.yml` targeting `sls-dev` running `cargo build --release -p solidcraft-sls`.
