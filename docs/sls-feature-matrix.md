# NestCraft — Ranked Feature Matrix & Roadmap

> **Naming & Scope Note:** Renamed to **NestCraft** (formerly *SolidCraft SLS*) to distinguish it from a slicer and support a broader nesting & build-preparation approach (project file format: **`.nestc`**). Target workspace crates: `crates/nestcraft-core` + `apps/nestcraft` (migrating from initial `crates/sls-core` + `apps/solidcraft-sls`).

Reference baseline: All 12 functional areas from the additive build-preparation reference manual, ranked for **powder-bed SLS manufacturing** (EOS Formiga P110 `160 × 210 × 330 mm`, Farsoon HT403P `400 × 400 × 450 mm`, PA12) in **NestCraft** (`crates/nestcraft-core` + `apps/nestcraft`).

Related docs: [`docs/sls-nesting-plan.md`](sls-nesting-plan.md) · [`docs/sls-nesting-migration.md`](sls-nesting-migration.md)

---

## Ranking Rubric

|       Rank       | Tier                                  | Criteria for SLS Powder-Bed Build Preparation                                                                                                                                                                            |
| :--------------: | :------------------------------------ | :----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`P0`** | **Critical SLS Core**           | Mandatory for daily SLS build prep: mesh ingestion, manifold validation, manual & 3D true-shape nesting, exact collision/interlock safety, chamber/metrics overview, and 3MF export.                                     |
| **`P1`** | **High-Value Production**       | Major workflow accelerators for batch SLS runs: Sinterboxes / model packages, anisotropic shrinkage scaling, Z-compensation, CSV serial labeling, automatic mesh repair gate, report sheets, and headless `--mcp` API. |
| **`P2`** | **Secondary / Quality-of-Life** | Useful engineering utilities: part hollowing with drain holes, plane/polygon cuts, boolean ops, interactive Z-slice hatching & EOS `.sli` export, and 3D measurement tools.                                            |
| **`P3`** | **Deferred / Non-SLS**          | Redundant or irrelevant to powder-bed SLS (e.g., SLA/DLP resin support structures, Kinect depth scanning, UV color/texture painting).                                                                                    |

---

## Complete Ranked Feature Matrix (§1–§12)

> **Status Legend:** `Done` = implemented on `sls-dev` · `Partial` = core present, enhancements pending · `Planned` = not yet implemented.

### §1. Mesh Validity & Diagnostics

| Ref          | NestCraft Feature                                   |       Rank       |   Status   | Implementation Target                     |
| :----------- | :-------------------------------------------------- | :--------------: | :---------: | :---------------------------------------- |
| `1.1–1.2` | Watertight / closedness check (open boundary edges) | **`P0`** |  `Done`   | `crates/nestcraft-core/src/manifold.rs` |
| `1.3`      | Consistent triangle orientation / winding check     | **`P0`** |  `Done`   | `crates/nestcraft-core/src/manifold.rs` |
| `1.4`      | Self-intersection detection & highlighting          | **`P1`** | `Planned` | `crates/nestcraft-core/src/manifold.rs` |

### §2. Workspace, Project Tree & Build Platform

| Ref               | NestCraft Feature                                                                                             |       Rank       |   Status   | Implementation Target                                                                |
| :---------------- | :------------------------------------------------------------------------------------------------------------ | :--------------: | :---------: | :----------------------------------------------------------------------------------- |
| `2.1, 5.7`      | Compact Project Tree & Split Sidebars (left: `Build Parts Browser` with 20×20 3D thumbnails, groups, hide/show, transparency, color picker, context menu + `Selected Part` config/info; right: `Build Summary`, `Build Chamber`, `No-Build Zone`; draggable pop-up modals) | **`P0`** |  `Done`   | `apps/nestcraft` |
| `2.2, 4.1–4.3` | 3D Viewport (`MovingLayer` fast multi-part drag & rotation, `Z=0` floor grid, bed envelope, Front-Left default view with `+X` right / `+Y` far, Perspective/Isometric toggle, 360° orbit, middle-pan, marquee select, face hover highlight, centerpoint drag handle, toggleable world-axis `X/Y/Z` ring rotation gizmo, bottom-right Mesh Repair Gate badge, model context menu, 3D View Compass) | **`P0`** |  `Done`  | `apps/nestcraft/src/{viewport,ui_viewport}.rs` |
| `2.4, 5.9`      | Machine Platform presets (Formiga P110 `200×250×330`, Farsoon HT403P `400×400×450`, Custom) with configurable size, origin, wall distance, Min Z & per-machine No-Build Zone | **`P0`** |  `Done`   | `crates/nestcraft-core/src/model.rs` + `apps/nestcraft`                              |
| `2.5`           | File formats: Binary/ASCII STL (read/write), Production 3MF (read/write) & `.nestc` project archive | **`P0`** |  `Done`  | `crates/nestcraft-core/src/io/{stl,threemf,nestc}.rs`                              |
| `2.6`           | Keyboard shortcuts (`Ctrl+Z`/`Ctrl+Y` unlimited undo/redo, `X`/`Y`/`Z` ±45° rotate, `Ctrl+Down` Z-snap, `R` +90°, `Space` view cycle, `M` move, `Ctrl+D` dup, `Ctrl+S` save) | **`P1`** | `Partial` | `apps/nestcraft` (undo/redo/rotate/snap/view/move/nudge/dup/del/hide/lock/save done; pack & collision pending) |

### §3. Project Management, Reporting & Export

| Ref              | NestCraft Feature                                                                        |       Rank       |   Status   | Implementation Target                                                                             |
| :--------------- | :--------------------------------------------------------------------------------------- | :--------------: | :---------: | :------------------------------------------------------------------------------------------------ |
| `3.1–3.2.2`   | Project lifecycle (`.nestc` save/open), parallel multi-threaded STL/3MF import, model-location import with below-`Z=0` prompt | **`P0`** |  `Done`   | `crates/nestcraft-core/src/io/nestc.rs` + `apps/nestcraft`                                        |
| `3.2.3`        | File Preview Browser (dimensions, triangle count, thumbnail before import)               | **`P2`** | `Planned` | `apps/nestcraft`                                                                                |
| `3.2.4, 6.2.4` | Split multi-shell STLs into separate parts (`Shells to Parts` with Keep/Delete Original prompt) | **`P1`** |  `Done`   | `crates/nestcraft-core/src/manifold.rs` + `apps/nestcraft` |
| `3.2.5`        | Build Report Sheets (`.pdf` / `.csv` / HTML build summary with metrics & thumbnails; `Generate Report` placeholder in top bar) | **`P1`** | `Planned` | `crates/nestcraft-core` + `apps/nestcraft`                                                    |
| `3.3`          | Export build job to 3MF & export individual/selected parts to STL (with context menu)    | **`P0`** |  `Done`   | `crates/nestcraft-core/src/io/{threemf,stl}.rs` + `apps/nestcraft`                                |
| `3.4`          | High-resolution viewport screenshot capture                                              | **`P2`** | `Planned` | `apps/nestcraft`                                                                                |

### §4. Viewing & Visualization Options

| Ref          | NestCraft Feature                                                                              |       Rank       |    Status    | Implementation Target                                                                           |
| :----------- | :--------------------------------------------------------------------------------------------- | :--------------: | :----------: | :---------------------------------------------------------------------------------------------- |
| `4.1–4.2` | Standard views (`Space` cycle), View banner (`Zoom to All Parts`, `Zoom to Selected`, `Zoom to Platform`, `Zoom to All`, `Zoom to Selection` right-drag rect) | **`P0`** | `Done` | `apps/nestcraft/src/viewport.rs` + `apps/nestcraft/src/main.rs` |
| `4.3`      | Display modes (shaded, 50% per-part transparency, per-part RGB color picker, outbox brackets, coplanar face highlight, Z-clipping plane) | **`P1`** | `Partial` | `apps/nestcraft/src/viewport.rs` (50% transparency, RGB color, outbox & face highlight done; edges/Z-clip pending) |
| `4.4`      | Mesh vertex color & UV texture viewer                                                          | **`P3`** | `Deferred` | —                                                                                              |

### §5. Part Management, Alignment, Collision & 3D Packing

| Ref              | NestCraft Feature                                                                                                                               |       Rank       |   Status   | Implementation Target                                                                                       |
| :--------------- | :---------------------------------------------------------------------------------------------------------------------------------------------- | :--------------: | :---------: | :---------------------------------------------------------------------------------------------------------- |
| `5.1–5.2`     | Add/Remove parts, Multi-select (`Ctrl+Click`, `Shift+Click` tree range, marquee drag) & Metric Filter Selection | **`P1`** | `Partial` | `apps/nestcraft` (add/remove & multi-select done; metric filter selection pending)                          |
| `5.3`          | Standard parametric test library (cubes, cylinders, tensile bars, benchmark shapes)                                                             | **`P2`** | `Partial` | `crates/nestcraft-core` / `apps/nestcraft` (`Sample Part` bracket done)                                    |
| `5.4`          | Duplicate parts (`×N` grid `Nx × Ny × Nz` with configurable X/Y/Z gap & compact distribution)                                                 | **`P0`** |  `Done`   | `crates/nestcraft-core/src/model.rs` + `Duplicate Parts` popup in `apps/nestcraft`                          |
| `5.5.1–5.5.3` | Move (`Relative`/`Absolute`, multi-part `MovingLayer` drag), Rotate (centroid-preserving `±45°`/`±90°`/Euler), Align (`Align Face to -Z`, `Drop Min Z`, `Center XY`) | **`P0`** | `Done` | `crates/nestcraft-core/src/model.rs` + `apps/nestcraft` |
| `5.5.4`        | **Anisotropic X/Y/Z Scaling** (`Ctrl+S` popup: Uniform/Per-Axis, Percent/Absolute/Factor, `in ↔ mm` unit scale, Keep Open & Reset) | **`P0`** | `Done` | `crates/nestcraft-core/src/model.rs` (`scale_instance`) + `apps/nestcraft` |
| `5.5.5`        | Outbox 2D/3D Arrange Parts                                                                                                                      | **`P1`** | `Planned` | `crates/nestcraft-core/src/packer.rs`                                                                     |
| `5.6`          | 2D Planar True-Shape Placement (single-layer Z=0 nesting)                                                                                       | **`P1`** | `Planned` | `crates/nestcraft-core/src/packer.rs`                                                                     |
| `5.8`          | **Platform Overview Table (`F5`)** (ID, status, L×W×H, volume, area, tris, density % + CSV export)                                    | **`P0`** | `Planned` | `apps/nestcraft`                                                                                          |
| `5.10`         | **Collision Detection (`Ctrl+K`) & Live Drag Collision Highlight** (`1.0 mm` / `2.5 mm` clearance)                                  | **`P0`** | `Planned` | `crates/nestcraft-core/src/verify.rs` + viewport highlight                                                |
| `5.11.1`       | **3D True-Shape Packing** (min part gap, voxel accuracy, Z-step angles, 3D flip, selected-only, height limit `max_Z`, XY border margin) | **`P0`** | `Planned` | `crates/nestcraft-core/src/voxel.rs`, `edt.rs`, `packer.rs`                                           |
| `5.11.1`       | **Interlocking Test** (detect non-touching parts trapped inside cavities/loops)                                                           | **`P0`** | `Planned` | `crates/nestcraft-core/src/interlock.rs`                                                                  |
| `5.11.2`       | **Model Packages / Sinterboxes** (`Box` & `Tight Fit` stepped cages, auto window sizing from min XYZ, adaptive `0.8–1.0 mm` bars, largest-panel 3D label, auto-grouping & single-part multiply suggestion) | **`P1`** |  `Done`   | `crates/nestcraft-core/src/sinterbox.rs` + `apps/nestcraft`                                               |
| `5.11.2`       | Headless Data-API / ERP batch automation (`--mcp` server)                                                                                     | **`P1`** | `Planned` | `apps/nestcraft` + `solidcraft-mcp-server`                                                              |

### §6. Part Analysis & Mesh Editing

| Ref              | NestCraft Feature                                                                                    |       Rank       |    Status    | Implementation Target                             |
| :--------------- | :--------------------------------------------------------------------------------------------------- | :--------------: | :----------: | :------------------------------------------------ |
| `6.1.1`        | Standard Analysis & `Analyze` banner (volume, surface area, dimensions, watertightness, shell count, bottom-right Mesh Repair Gate badge; `Analyze` banner placeholders: `Stair Highlight`, `Measure`, `Wall Thickness`, `Up / Downskin`, `Shadow Area`, `Collision Check`, `Thermal Map`) | **`P0`** | `Partial` | `crates/nestcraft-core/src/{model,manifold}.rs` + `apps/nestcraft/src/{ui_bars,ui_viewport}.rs` |
| `6.1.4–6.1.5` | Wall Thickness check & Sintered Thermal/Shadow Area Density Heatmap                                  | **`P1`** | `Planned` | `crates/nestcraft-core/src/thermal.rs`          |
| `6.1.2–6.1.3` | Upskin / Downskin surface analysis & Center of Gravity                                               | **`P2`** | `Planned` | `crates/nestcraft-core/src/analysis.rs`         |
| `6.2.1–6.2.5` | Simple Editing (`Edit` banner): Invert normals, Mirror, Convert Units (`mm`↔`in`), `Shells to Parts`, `Merge Parts` (with Keep/Delete Original prompt), plus placeholder buttons (`Shell`, `Labels`, `Repair Mesh`, `Boolean`, `Cut / Split`, `Offset`) | **`P1`** | `Partial` | `crates/nestcraft-core/src/model.rs` + `apps/nestcraft` (`Shells to Parts` & `Merge Parts` done; invert/mirror/units & edit placeholders pending) |
| `6.3`          | Create Shell / Hollowing (inner wall offset + powder drain holes)                                    | **`P2`** | `Planned` | `crates/nestcraft-core/src/hollow.rs`           |
| `6.4–6.5`     | Simple Plane Cut (`X/Y/Z` with alignment pins) & Free Polygon Cut                                  | **`P2`** | `Planned` | `crates/nestcraft-core/src/cut.rs`              |
| `6.6`          | Mesh Boolean Operations (Union, Difference, Intersection)                                            | **`P2`** | `Planned` | `crates/nestcraft-core/src/boolean.rs`          |
| `6.7.1, 6.7.3` | **Text Labeling & CSV List Labeling** (emboss/deboss part IDs or serial BOM lists onto parts)  | **`P1`** | `Planned` | `crates/nestcraft-core/src/label.rs`            |
| `6.7.2`        | Image-to-relief labeling                                                                             | **`P3`** | `Deferred` | —                                                |
| `6.8–6.11`    | Triangle Reduction, Mesh Smoothing, Remesh, Mesh Comparison                                          | **`P2`** | `Planned` | `crates/nestcraft-core/src/mesh_ops.rs`         |
| `6.12–6.13`   | Kinect depth scan import & color/texture imposing                                                    | **`P3`** | `Deferred` | —                                                |

### §7. Part Repair & SLS Compensation

| Ref              | NestCraft Feature                                                                                                                            |       Rank       |    Status    | Implementation Target                       |
| :--------------- | :------------------------------------------------------------------------------------------------------------------------------------------- | :--------------: | :----------: | :------------------------------------------ |
| `7.1–7.6`     | Mesh Repair Pipeline (fix flipped normals, stitch open edges, remove degenerate/double faces, close trivial/all holes, wrap internal shells) | **`P1`** | `Planned` | `crates/nestcraft-core/src/repair.rs`     |
| `7.7.3`        | **Z-Compensation** (trim bottom-layer laser over-cure growth on down-facing surfaces)                                                  | **`P1`** | `Planned` | `crates/nestcraft-core/src/compensate.rs` |
| `7.7.6`        | Offset Hole Edges (horizontal hole shrinkage compensation)                                                                                   | **`P2`** | `Planned` | `crates/nestcraft-core/src/compensate.rs` |
| `7.7.7–7.7.8` | Surface extrusion & 3D-extrude modeling                                                                                                      | **`P3`** | `Deferred` | —                                          |

### §8. Support Structures

| Ref          | NestCraft Feature                |       Rank       |      Status      | Implementation Target                                    |
| :----------- | :------------------------------- | :--------------: | :--------------: | :------------------------------------------------------- |
| `8.1–8.4` | SLA/DLP/Metal Support Structures | **`P3`** | `Out of Scope` | Powder-bed PA12 SLS is self-supporting in unfused powder |

### §9. Measuring & Quality Assurance

| Ref     | NestCraft Feature                                                                         |       Rank       |    Status    | Implementation Target                          |
| :------ | :---------------------------------------------------------------------------------------- | :--------------: | :----------: | :--------------------------------------------- |
| `9.1` | 3D Measuring Tool (point-to-point distance, angles, hole radius/diameter, anchored notes) | **`P2`** | `Planned` | `apps/nestcraft` + `crates/nestcraft-core` |
| `9.2` | Predefined QA inspection test suites                                                      | **`P3`** | `Deferred` | —                                             |

### §10. Slice Commander

| Ref            | NestCraft Feature                                                                           |       Rank       |    Status    | Implementation Target                                        |
| :------------- | :------------------------------------------------------------------------------------------ | :--------------: | :----------: | :----------------------------------------------------------- |
| `10.1–10.4` | Interactive Z-Slice Inspector (cross-section preview at layer height `Z`, slice playback) | **`P1`** | `Planned` | `crates/nestcraft-core/src/slice.rs` + viewport clip plane |
| `10.5–10.7` | 2D slice boolean/offset/hatch filling & **EOS `.sli` / CLI slice export**           | **`P3`** | `Deffered` | `crates/nestcraft-core/src/slice.rs`                       |

### §11–§12. Settings & Report Templates

| Ref            | NestCraft Feature                                                                           |       Rank       |   Status   | Implementation Target     |
| :------------- | :------------------------------------------------------------------------------------------ | :--------------: | :---------: | :------------------------ |
| `11.1–11.3` | Persistent preferences (default machine, clearance, wall margin, PA12 scale factors, theme) | **`P1`** | `Planned` | `apps/nestcraft`        |
| `12.1–12.2` | Customizable report template parameters (text/image placeholders)                           | **`P2`** | `Planned` | `crates/nestcraft-core` |

---

## Ranked Execution Plan (Stages 2–5)

```mermaid
flowchart LR
    S1["Stage 1 (Done): Mesh I/O, Option C UI, 3D Viewport & Compass"] --> S2["Stage 2 (P0): Core Build-Prep, 3D EDT Packer, Collision & Interlock"]
    S2 --> S3["Stage 3 (P1): Sinterbox, Z-Comp, CSV Labeling, Repair Gate, Reports & --mcp"]
    S3 --> S4["Stage 4 (P2): Hollowing, Plane Cut, Measure Tool & EOS .sli Export"]
```

### Stage 2 (`P0`) — Core Build-Prep, 3D EDT Packer, Collision & Interlock Engine

1. **Mesh Diagnostics (`crates/nestcraft-core/src/manifold.rs`):** Watertightness, consistent winding, shell counting, degenerate triangle count.
2. **3D EDT Voxelizer & Packer (`edt.rs`, `voxel.rs`, `packer.rs`):**
   - `u64`-bitpacked `solid_mask` & Felzenszwalb-Huttenlocher 3D EDT `clearance_mask`.
   - Bottom-Left-Back-Fill + multi-start metaheuristic supporting `Fixed`, `ZOnly`, and `Free` (with 3D flip toggle), `max_Z` ceiling limit, selected-only packing, and non-fitting overflow placement outside the chamber.
   - 2D Planar nesting mode (`§5.6`) & 3D grid duplicate (`Nx × Ny × Nz` with X/Y/Z spacing — `Done`).
3. **Exact Collision & Interlocking Tests (`verify.rs`, `interlock.rs`):**
   - Exact triangle-to-triangle clearance verification (`1.0 mm` / `2.5 mm` configurable) + live collision highlight in the viewport.
   - Topological interlocking detector (ray-escape / translational separability check on cavity-nesting pairs).
4. **Build-Prep Workspace Upgrades (`apps/nestcraft`):**
   - **Platform Overview (`F5`)** modal/table with all part stats + CSV export.
   - **Anisotropic X/Y/Z Scaling** in the Inspector (for PA12 shrinkage compensation) + Custom Machine Platform dialog + Zoom to Selected.

### Stage 3 (`P1`) — SLS Production Automation & Compensation

1. **Sinterbox / Model Packages (`sinterbox.rs`):** Auto-group small parts (`< vol / min_dim`) inside parametric perforated cages for depowdering.
2. **Automatic Mesh Repair Gate (`repair.rs`):** Fix flipped normals, stitch open edges, remove degenerate/duplicate triangles, close holes, and split multi-shell STLs (`Shells to Parts` / `Merge Parts`).
3. **SLS Geometric Compensation & Labeling (`compensate.rs`, `label.rs`):**
   - First-layer **Z-Compensation** (`§7.7.3`), Invert/Mirror/Unit conversion.
   - **CSV List Labeling** & text embossing/debossing for serialized production parts.
4. **Thermal Density Heatmap & Interactive Z-Slice (`thermal.rs`, `slice.rs`):** 3D sliding-box sintered density map (`20×20×20 mm`) + real-time Z-plane cross-section viewer.
5. **Report Sheets & Headless `--mcp` (`apps/nestcraft`):** Exportable build report sheets (`.html` / `.pdf` / `.csv`) + `--mcp` JSON-RPC automation (`load_job`, `nest_job`, `split_builds`, `export_3mf`).
   - *Build Report Plan Note (pricing & material specs in `C:\Users\Degamim-I9\Desktop\SLS_wiki\raw\tools\quoting_editor`):*
     1. Only account for placed parts that are **inside the machine** build volume.
     2. Generate report metrics, pricing, and powder/build calculations based on the **selected machine** (`data/machines.js`: `bedX`, `bedY`, `pillowZ`, `machineCost`, `defaultBuildRate`).
     3. Add **"Select Material"** to the **Build Chamber** configuration (`data/materials.js`: `PA 2200`, `FS3300PA`, `PA2210FR`, `FS3401GB`, `PBT-01`, `PA3200GB` + `MATERIAL_PROFILES` with `bulkDensity`, `sinteredDensity`, `powderPrice`, `recovery`, `setupHrs`, `margin`, `layerThickness`, `recoatTime`, `laserSpeed`, `hatchDistance`) and support Netfabb-style XML metrics (`netfabb618891.xml`).

### Stage 4 (`P2`) — Mesh Editing, Measurement & Direct Slicing

1. **Hollowing / Create Shell (`hollow.rs`)** with inner wall offset and powder drain holes + **Wall Thickness** analysis.
2. **Plane & Free Polygon Cutting (`cut.rs`)** with optional alignment dowel pins + **Mesh Booleans / Decimation / Smoothing**.
3. **3D Measuring Tool (`§9.1`):** Point-to-point distance, angle, radius/diameter, and viewport annotations.
4. **2D Slice Offsets, Hatching & EOS `.sli` Export (`§10.5–10.7`).**

### Stage 5 (`P3`) — Deferred / Out-of-Scope

- SLA/DLP support structures (`§8`), Kinect scan capture (`§6.12`), UV texture/image imposing (`§4.4, §6.7.2, §6.13`), and standalone 3D surface modeling (`§7.7.7–7.7.8`).
