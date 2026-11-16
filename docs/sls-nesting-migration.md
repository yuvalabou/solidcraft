# SolidCraft SLS 3D Nesting — Migration Architecture & Landmine Audit

- **Repository:** `yuvalabou/solidcraft` (tracking upstream `tangency-labs/solidcraft`)
- **Target Baseline:** SolidCraft `v0.43.0` (Rust 2024 edition, MSRV `1.95`)
- **Workspace Topology:** `crates/sls-core` (`Class::Shared` / pure Rust) + `apps/solidcraft-sls` (`Class::Exempt` binary & MCP host)
- **Target Machines:** EOS Formiga P110 (`160 × 210 × 330 mm`), Farsoon HT403P (`400 × 400 × 450 mm`), PA12 sintered parts

---

## 1. Executive Architecture

Instead of modifying SolidCraft's parametric B-Rep application (`apps/solidcraft`), this project lives as isolated workspace members inside the fork:

```
solidcraft/
├── crates/
│   ├── viewport/          # Shared (MIT/Apache-2.0) — camera, projection, picking rays, Scene
│   ├── viewport-gpu/      # Shared (MIT/Apache-2.0) — wgpu pipelines, InstancedMesh (INSTANCE_SIZE = 52)
│   ├── viewport-egui/     # Shared (MIT/Apache-2.0) — egui-wgpu paint callbacks
│   └── sls-core/          # NEW (Class::Shared) — pure-Rust mesh I/O, BVH, 3D EDT voxelization, 3D packer, Sinterbox
├── apps/
│   ├── solidcraft/        # Untouched upstream CAD app
│   └── solidcraft-sls/    # NEW (Class::Exempt) — standalone egui/wgpu SLS nesting GUI + `--mcp` headless server
├── attribution/pending/
│   └── sls.md             # Pending asset attribution rows (avoids merge conflicts on ATTRIBUTION.md)
└── docs/
    └── sls-nesting-migration.md
```

### Dependency Boundary Rules
1. **`crates/sls-core` has ZERO UI/GPU dependencies.** No `egui`, `wgpu`, `eframe`, `rfd`, or `solidcraft-ui-kit`. Both `apps/solidcraft-sls` (GUI) and `--mcp` (headless automation) drive the exact same `sls-core` engine.
2. **Reuse `viewport*` only, NOT `solidcraft-io` or `solidcraft-engine`.** `crates/io` (`solidcraft-io`) pulls in `solidcraft-kernel`, `solidcraft-doc`, `solidcraft-sketch`, and `solidcore-ops` (the full B-Rep stack). By contrast, `solidcraft-viewport`, `solidcraft-viewport-gpu`, and `solidcraft-viewport-egui` depend only on `solidcore-record` (`Vec3`), `solidcore-tess` (`Mesh`), `solidcraft-color`, `wgpu` (`30`), and `egui` (`0.37`), and are dual-licensed `MIT OR Apache-2.0`.
3. **Hardware Instancing is Already Built In:** `crates/viewport-gpu/src/view.rs` provides `InstancedMesh` (`INSTANCE_SIZE = 52`: 3×4 rigid transform rows + `PaletteIndex`). Hundreds of duplicate SLS parts share one `Arc<Mesh>` / GPU vertex buffer with per-instance transform matrices and red/gray clearance coloring.

---

## 2. Codebase Landmines & Mitigations

The following traps exist in SolidCraft `v0.43.0` and will break builds, CI, or licensing if ignored:

### Landmine 1: `LICENSE-BRAND` Restrictions (`ui-kit`, `look`, `theme`)
- **Fact:** While SolidCraft's geometry, viewport, and engine crates are `MIT OR Apache-2.0`, `LICENSE-BRAND` explicitly restricts:
  - `crates/ui-kit` (`solidcraft-ui-kit`)
  - `crates/look` (`solidcraft-look`: SVG icon masters, `palette.toml`, `aliases.toml`, `glyphs.rs`, built-in themes)
  - `crates/theme` (`solidcraft-theme`)
  - SolidCraft name, logo, and app icons (`assets/branding`, `assets/icons/app`)
- **Clause 2a vs. Clause 3a/3c:** Clause 2a allows building/modifying Brand Material inside a personal SolidCraft fork for internal development, testing, and learning. Clause 3a/3c prohibits linking `ui-kit`, `look`, or `theme` in another external program/product or distributing builds under SolidCraft's look/name without written maintainer permission.
- **Exception (Fonts):** `LICENSE-BRAND` explicitly states that **IBM Plex Sans** and **IBM Plex Mono** (`crates/look/fonts/`) remain under **SIL Open Font License 1.1**.

### Landmine 2: `xtask/src/ui_containers.rs` AST Container Guard
- **Trap:** `cargo xtask ui-containers` (and unit test `all_workspace_hosts_use_the_container`) parses **every `.rs` file under `crates/` and `apps/`** using `syn` (including macro bodies). Calling `egui::Window`, `egui::Area`, `egui::Modal`, `egui::Panel`, `egui::SidePanel`, `egui::TopBottomPanel`, `egui::CentralPanel`, `egui::Popup`, `egui::SubMenu`, or `ui.menu_button` anywhere outside `crates/ui-kit/src/container.rs` **fails the check**.
- **Mitigation:** Either exempt `apps/solidcraft-sls/` in `xtask/src/ui_containers.rs` (for a clean-room raw `egui` UI) or route containers through `solidcraft_ui_kit::Container` (only valid for internal non-distributed Clause 2a builds).

### Landmine 3: `xtask/src/layers.rs` Workspace Crate Registration
- **Trap:** Root `Cargo.toml` uses glob `members = ["crates/*", "apps/*", "xtask"]`. Adding `crates/sls-core` or `apps/solidcraft-sls` without registering them in `xtask/src/layers.rs::TABLE` fails `cargo xtask layers` with `Violation::Unregistered`. Furthermore, `UI_MIN_LAYER = 5` forbids `egui`/`wgpu`/`rfd` in any crate below Layer 5.
- **Mitigation:** Register `("sls-core", Class::Shared)` (or `Class::Layer(1)`) and `("solidcraft-sls", Class::Exempt)` in `xtask/src/layers.rs::TABLE`.

### Landmine 4: `xtask/src/assets.rs` Attribution Gate
- **Trap:** Any non-code asset (`.png`, `.svg`, `.ico`, `.ttf`, `.otf`, `.stl`, `.step`, `.obj`, etc.) outside `solidcore/`, `vendor/`, and `plan/` fails `cargo xtask assets` unless registered in `ATTRIBUTION.md` or `attribution/pending/*.md`. (Markdown `.md` files are exempt.)
- **Mitigation:** Record all custom SLS icons, fonts, or sample test meshes in `attribution/pending/sls.md` instead of editing `ATTRIBUTION.md` directly (prevents upstream merge conflicts).

### Landmine 5: Strict Workspace & Crate Lints
- **Trap:** Root `Cargo.toml` sets `unsafe_code = "forbid"`, and core crates apply `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]`.
- **Mitigation:** Use `?`, `Result`, and explicit error types (`thiserror = "2"`, already in workspace) everywhere in `sls-core` and `solidcraft-sls`.

### Landmine 6: `jagua-rs` is Strictly 2D
- **Trap:** `jagua-rs` (and `sparrow`) is a 2D irregular polygon collision/nesting engine for laser/sheet-metal cutting. It does **not** solve 3D volumetric nesting for SLS powder beds.
- **Mitigation:** Build the 3D collision/clearance engine on BVH + voxelized 3D Euclidean Distance Fields (see Section 4).

---

## 3. UI & Icon Strategy

| Track | Scope / Compliance | Fonts | Icons & Widgets | Tradeoff |
| :--- | :--- | :--- | :--- | :--- |
| **Track A: Clean-Room `egui` (Recommended)** | 100% `MIT OR Apache-2.0`, safe for any distribution | Embed `IBMPlexSans-Variable.ttf` & `IBMPlexMono-Regular.ttf` (SIL OFL 1.1) directly | `egui_phosphor` / Lucide icon font (MIT) **OR** Lucide/Tabler MIT SVGs rasterized via workspace `tiny-skia = "0.12"` | Requires exempting `apps/solidcraft-sls` in `xtask/src/ui_containers.rs`; zero `LICENSE-BRAND` exposure |
| **Track B: Internal Clause 2a Fork** | Internal dev/shop builds only (do not distribute externally as a separate product) | `solidcraft_look::fonts` | `solidcraft_ui_kit::icons::IconSet::extended(solidcraft_look::icons::DATA, sls_ext)` + `Container` | Inherits SolidCraft ribbon/panel styling immediately, but locked under `LICENSE-BRAND` Clause 3 |

### Clean-Room `tiny-skia` Icon Note
`tiny-skia = "0.12"` (BSD-3-Clause) is already compiled in `[workspace.dependencies]`. If crisp multi-color or custom CAD/SLS icons (e.g., *Sinterbox*, *Z-lock*, *Thermal Map*, *Part Clearance*) are preferred over monochrome font glyphs, load MIT-licensed Lucide/Tabler SVGs from `apps/solidcraft-sls/assets/icons/` (attributed in `attribution/pending/sls.md`) and rasterize them to `egui::TextureHandle` at startup.

---

## 4. 3D Nesting Engine Architecture (`sls-core`)

Rather than writing geometry math from scratch or importing `jagua-rs` (2D), reuse proven Rust primitives already validated in our ecosystem (`pyroid-core` + `solidcraft`):

1. **Mesh Ingestion & Repair Gate:**
   - **Binary/ASCII STL & 3MF:** Pure-mesh parser in `sls-core` using workspace `zip = "9"` + `quick-xml = "0.41"` (mirroring the Production Extension `p:path` build-item logic in `crates/io/src/threemf.rs` without pulling in `solidcraft-doc`).
   - **Manifold & Winding Check:** Reuse the Farsoon BuildStar manifold/winding checks from `pyroid-core::manifold` (`raw/projects/GitHub/pyroid/crates/pyroid-core/src/manifold.rs`) to reject/flag non-manifold customer STLs up front (or invoke `manifold3d` / keep Netfabb as an upstream repair pre-pass for broken meshes).
2. **Exact 3D Sintering Clearance via 3D EDT (from `pyroid-core`):**
   - Naive AABB padding wastes powder bed volume; triangle-to-triangle distance queries (`parry3d`) on 500 × 50k-triangle meshes are too slow inside an optimization loop.
   - **Voxel + 3D EDT Pipeline:**
     1. Voxelize each unique part mesh at target grid pitch (e.g., `1.0 mm` or `0.5 mm`) using `pyroid-core::bvh` (Rayon-parallel BVH ray-cast containment).
     2. Compute the **Felzenszwalb-Huttenlocher 3D Euclidean Distance Transform** (`pyroid-core::edt`) once per unique part / quantized orientation.
     3. Generate two binary `u64` bitpacked voxel masks per orientation:
        - `solid_mask` (`EDT <= 0`) — actual part occupancy (for thermal density & Sinterbox checks)
        - `clearance_mask` (`EDT <= clearance_mm / 2.0`, e.g., `1.25 mm` half-clearance for `2.5 mm` part-to-part gap) — dilated shell for `O(1)` bitwise `&` collision detection (`(a_word & b_word) != 0`).
3. **3D Placement & Optimization:**
   - **Stage 1 (Constructive):** Gravity / Bottom-Left-Back-Fill (`Z`-first) sorted by bounding volume / height, supporting per-part rotation constraints (`Free`, `ZOnly` / XY-strength lock, `Fixed`).
   - **Stage 2 (Metaheuristic):** Simulated Annealing / Tabu local search swapping/nudging parts to minimize `max_Z` (since Z-height dictates SLS recoater time and print cost) and penalizing excessive local voxel density (thermal hotspot / "egg effect" mitigation).
   - **Stage 3 (Exact Verification):** Final `parry3d` exact mesh-to-mesh clearance check on placed neighbors before committing or exporting 3MF.

---

## 5. Phased Implementation Roadmap

### Phase 0: Git & Workspace Hygiene (Immediate)
- Keep `main` as a clean 1:1 mirror of `upstream/main` (only docs/config or fast-forwardable commits if tracking upstream; keep all SLS code on `sls-dev`).
- Register `sls-core` and `solidcraft-sls` in `xtask/src/layers.rs::TABLE`.
- If choosing Track A UI, add `apps/solidcraft-sls/` exemption in `xtask/src/ui_containers.rs`.

### Phase 1: Headless `sls-core` + Packer Benchmark (No UI)
- Scaffold `crates/sls-core` (`stl` / `3mf` mesh reader & 3MF build-item writer, printer build volumes for Formiga P110 & Farsoon HT403P).
- Port or depend on `pyroid-core`'s BVH + 3D EDT (`bvh.rs`, `edt.rs`, `manifold.rs`) to generate bitpacked `u64` clearance masks.
- Implement Bottom-Left-Back-Fill + Simulated Annealing packer and run a **benchmark CLI** against a real 50–150 part job comparing packing density (%), Z-height (mm), and runtime (s) against Netfabb/Magics.
- **Go/No-Go Gate:** Do not build GUI widgets until `sls-core` achieves acceptable density (target ≥ 10–12% without collisions) on real production jobs.

### Phase 2: `solidcraft-viewport-gpu` Canvas + Interactive Layout
- Scaffold `apps/solidcraft-sls` (`eframe` `0.37` + `solidcraft-viewport-egui` + `solidcraft-viewport-gpu`).
- Render build volume wireframe (`SceneLine` cube + `Z = 0` floor grid) and instanced parts via `InstancedMesh` (`Arc<Mesh>` + 3×4 affine matrix + palette color: normal gray vs. red clearance violation).
- Implement mouse ray-picking via `solidcraft_viewport::Camera` ray + `parry3d` raycast + manual translation/rotation Gizmo with live `parry3d` clearance checks.

### Phase 3: SLS Production Tooling & `--mcp` Automation
- **Sinterbox Generator:** Parametric perforated cage wrapper around small parts (`< X mm³` or `< Y mm` min dim) so parts don't vanish during bead-blasting/depowdering.
- **Thermal Density Heatmap:** Sliding 3D box filter over `solid_mask` voxels coloring dense local clusters (`PA12` curl/over-sintering risk) in viewport.
- **Headless MCP Server (`solidcraft-sls --mcp`):** Expose `load_job` (folder + CSV/JSON manifest of `part, qty, orientation_lock`), `nest_job`, `split_builds`, and `export_3mf` using workspace `solidcraft-mcp-server` (`Class::Shared`).

---

## 6. Git & Upstream Sync Workflow

```powershell
# 1. Sync main with upstream SolidCraft
git checkout main
git fetch upstream
git pull upstream main
git push origin main

# 2. Rebase or merge updates into sls-dev
git checkout sls-dev
git merge main   # or: git rebase main
```

- **Rule:** Never touch `apps/solidcraft/`, `crates/engine/`, or `crates/ui-kit/` on `sls-dev`. All new code lives strictly inside `crates/sls-core/` and `apps/solidcraft-sls/`.
- **Nightly CI:** Add `.github/workflows/sls-nightly.yml` triggering on `sls-dev` (or cron) running `cargo build --release -p solidcraft-sls`.
