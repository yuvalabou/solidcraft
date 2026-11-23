//! Parametric Sinterbox / Model Package generator (`Box` and `Tight Fit` stepped cages)
//! with automatic window sizing, adaptive bar thickness, largest-panel 3D label embossing,
//! and automatic `PartGroup` creation.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::model::{
    Aabb, BuildJob, GroupId, IDENTITY_TRANSFORM, InstanceId, PartId, TriMesh, transform_point,
};
use crate::{NestCraftError, Result};

/// Maximum coarse grid cells along any single axis to keep mesh sizes bounded.
const MAX_GRID_CELLS_PER_AXIS: usize = 48;

/// Outer geometry style for a generated Sinterbox cage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CageShape {
    /// Rectangular AABB perforated box around the selected parts.
    #[default]
    Box,
    /// Stepped 3D voxel-hull cage conforming to irregular/concave/uneven-height layouts.
    TightFit,
}

/// Which face of the Sinterbox receives the 3D embossed label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelPanel {
    /// Automatically pick the largest contiguous flat panel on the cage.
    #[default]
    AutoLargest,
    TopPosZ,
    FrontNegY,
    RightPosX,
    BackPosY,
    LeftNegX,
    BottomNegZ,
}

impl LabelPanel {
    /// Human-readable display label for UI dropdowns and summaries.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::AutoLargest => "Auto (Largest)",
            Self::TopPosZ => "Top (+Z)",
            Self::FrontNegY => "Front (-Y)",
            Self::RightPosX => "Right (+X)",
            Self::BackPosY => "Back (+Y)",
            Self::LeftNegX => "Left (-X)",
            Self::BottomNegZ => "Bottom (-Z)",
        }
    }

    /// All manual face options (excluding `AutoLargest`), in tie-break preference order.
    pub const FACES: [Self; 6] = [
        Self::TopPosZ,
        Self::FrontNegY,
        Self::RightPosX,
        Self::BackPosY,
        Self::LeftNegX,
        Self::BottomNegZ,
    ];
}

/// Parameters controlling Sinterbox generation.
#[derive(Debug, Clone)]
pub struct SinterboxConfig {
    /// Cage outer shape (`Box` or `TightFit`).
    pub shape: CageShape,
    /// Inner gap (mm) between the enclosed part surfaces/bounds and the inner cage wall.
    pub inner_clearance_mm: f32,
    /// Maximum window opening width/height (mm).
    pub window_size_mm: f32,
    /// Cage strut / wall thickness (mm), typically `0.80–1.00 mm`.
    pub bar_thickness_mm: f32,
    /// Whether to emboss a 3D label on the chosen panel.
    pub label_enabled: bool,
    /// Text to emboss (defaults to the primary part name).
    pub label_text: String,
    /// Target panel for the label (`AutoLargest` or explicit face).
    pub label_panel: LabelPanel,
    /// Emboss protrusion height (mm) outward from the backing wall.
    pub label_depth_mm: f32,
    /// Name for the auto-created `PartGroup`.
    pub group_name: String,
    /// Render the cage 50% transparent so enclosed parts remain visible.
    pub cage_transparent: bool,
}

impl Default for SinterboxConfig {
    fn default() -> Self {
        Self {
            shape: CageShape::Box,
            inner_clearance_mm: 2.5,
            window_size_mm: 4.0,
            bar_thickness_mm: 0.80,
            label_enabled: true,
            label_text: String::new(),
            label_panel: LabelPanel::AutoLargest,
            label_depth_mm: 0.8,
            group_name: String::new(),
            cage_transparent: true,
        }
    }
}

/// Preview and diagnostic metrics for a Sinterbox configuration.
#[derive(Debug, Clone)]
pub struct SinterboxMetrics {
    /// Smallest individual bounding-box axis across all enclosed parts (mm).
    pub min_part_dim_mm: f32,
    /// Auto-computed diagonal-safe window opening (mm).
    pub recommended_window_mm: f32,
    /// Auto-computed bar thickness (`0.80–1.00 mm`) for the active window size.
    pub recommended_bar_mm: f32,
    /// Outer bounding dimensions `[dx, dy, dz]` (mm) of the cage.
    pub outer_size_mm: [f32; 3],
    /// Selected label panel face (resolved when `AutoLargest`).
    pub chosen_panel: LabelPanel,
    /// Physical `[width, height]` (mm) of the chosen contiguous flat panel.
    pub chosen_panel_size_mm: [f32; 2],
    /// `true` when `window_size_mm * sqrt(2) >= min_part_dim_mm` (part might slip diagonally).
    pub window_exceeds_safe_limit: bool,
}

/// Output produced by [`create_sinterbox`].
#[derive(Debug, Clone)]
pub struct SinterboxResult {
    /// Newly created `PartGroup` containing the enclosed parts and cage.
    pub group_id: GroupId,
    /// Prototype ID of the generated cage mesh.
    pub cage_part_id: PartId,
    /// Placed instance ID of the generated cage.
    pub cage_instance_id: InstanceId,
    /// Vertical lift (mm) applied to keep the cage bottom at or above `job.min_z_mm`.
    pub lifted_z_mm: f32,
    /// Final geometry metrics.
    pub metrics: SinterboxMetrics,
}

/// Compute diagonal-safe window opening (mm) from the smallest single-part dimension.
///
/// A square opening of side `w` has diagonal `w * sqrt(2)`. Using `0.70 * d_min` rounded down to
/// `0.5 mm` increments guarantees `w * sqrt(2) < d_min` so no part escapes during blasting.
pub fn recommended_window_size_mm(min_part_dim_mm: f32) -> f32 {
    if !min_part_dim_mm.is_finite() || min_part_dim_mm <= 0.0 {
        return 1.5;
    }
    let safe = (min_part_dim_mm * 0.70 * 2.0).floor() * 0.5;
    safe.clamp(1.5, 25.0)
}

/// Compute adaptive cage bar/wall thickness (`0.80–1.00 mm`) from `window_size_mm`.
///
/// Smaller windows are more prone to sintering shut from PA12 thermal bleed and therefore use
/// thinner `0.80 mm` bars; larger windows (`>= 8 mm`) step up to `1.00 mm`.
pub fn recommended_bar_thickness_mm(window_size_mm: f32) -> f32 {
    if !window_size_mm.is_finite() || window_size_mm <= 4.0 {
        return 0.80;
    }
    if window_size_mm >= 8.0 {
        return 1.00;
    }
    let t = (window_size_mm - 4.0) / 4.0;
    let raw = 0.80 + 0.20 * t;
    ((raw * 20.0).round() / 20.0).clamp(0.80, 1.00)
}

/// Return `true` when `window_size_mm` could allow a part of `min_part_dim_mm` to escape diagonally.
pub fn window_exceeds_safe_limit(window_size_mm: f32, min_part_dim_mm: f32) -> bool {
    window_size_mm * std::f32::consts::SQRT_2 >= min_part_dim_mm - 1e-4
}

/// Inspect `instance_ids` in `job` and return `(combined_world_aabb, min_part_dim_mm, default_label)`.
pub fn analyze_sinterbox_inputs(
    job: &BuildJob,
    instance_ids: &[InstanceId],
) -> Result<(Aabb, f32, String)> {
    if instance_ids.is_empty() {
        return Err(NestCraftError::InvalidInput(
            "Select at least 1 part to create a Sinterbox".to_owned(),
        ));
    }

    let mut comb_min = [f32::INFINITY; 3];
    let mut comb_max = [f32::NEG_INFINITY; 3];
    let mut min_dim = f32::INFINITY;
    let mut default_label: Option<String> = None;
    let mut found = 0usize;

    for &iid in instance_ids {
        let inst = job
            .instances
            .iter()
            .find(|i| i.id == iid)
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown instance id {}", iid.0)))?;
        let proto = job
            .part(inst.part_id)
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown part id {}", inst.part_id.0)))?;
        if default_label.is_none() {
            default_label = Some(proto.name.clone());
        }
        let wb = inst.world_bounds(proto.mesh.bounds);
        for k in 0..3 {
            comb_min[k] = comb_min[k].min(wb[0][k]);
            comb_max[k] = comb_max[k].max(wb[1][k]);
            let d = (wb[1][k] - wb[0][k]).abs();
            if d.is_finite() {
                min_dim = min_dim.min(d);
            }
        }
        found += 1;
    }

    if found == 0 || !min_dim.is_finite() {
        return Err(NestCraftError::InvalidInput(
            "No valid part instances found for Sinterbox".to_owned(),
        ));
    }

    Ok(([comb_min, comb_max], min_dim.max(0.1), default_label.unwrap_or_else(|| "Sinterbox".to_owned())))
}

/// Fast preview of Sinterbox metrics (without generating the full mesh) for live UI updates.
///
/// When `simulated_single_grid` is `Some((counts, gap_mm))` and `instance_ids.len() == 1`,
/// estimates the expanded combined AABB as if the part were duplicated in that compact grid.
pub fn preview_sinterbox_metrics(
    job: &BuildJob,
    instance_ids: &[InstanceId],
    cfg: &SinterboxConfig,
    simulated_single_grid: Option<([usize; 3], f32)>,
) -> Result<SinterboxMetrics> {
    let (mut bounds, min_dim, _) = analyze_sinterbox_inputs(job, instance_ids)?;
    if instance_ids.len() == 1
        && let Some(([nx, ny, nz], gap_mm)) = simulated_single_grid
    {
        let gap = gap_mm.max(0.0);
        let dx = (bounds[1][0] - bounds[0][0]).max(0.1);
        let dy = (bounds[1][1] - bounds[0][1]).max(0.1);
        let dz = (bounds[1][2] - bounds[0][2]).max(0.1);
        let gx = nx.max(1) as f32;
        let gy = ny.max(1) as f32;
        let gz = nz.max(1) as f32;
        bounds[1][0] = bounds[0][0] + dx * gx + gap * (gx - 1.0);
        bounds[1][1] = bounds[0][1] + dy * gy + gap * (gy - 1.0);
        bounds[1][2] = bounds[0][2] + dz * gz + gap * (gz - 1.0);
    }

    let params = validate_params(cfg)?;
    let plan = build_grid_plan(job, instance_ids, bounds, &params, false)?;
    let outer = [
        plan.coords[0].last().copied().unwrap_or(0.0) - plan.coords[0].first().copied().unwrap_or(0.0),
        plan.coords[1].last().copied().unwrap_or(0.0) - plan.coords[1].first().copied().unwrap_or(0.0),
        plan.coords[2].last().copied().unwrap_or(0.0) - plan.coords[2].first().copied().unwrap_or(0.0),
    ];
    let rec_w = recommended_window_size_mm(min_dim);
    let rec_b = recommended_bar_thickness_mm(params.window_mm);

    Ok(SinterboxMetrics {
        min_part_dim_mm: min_dim,
        recommended_window_mm: rec_w,
        recommended_bar_mm: rec_b,
        outer_size_mm: outer,
        chosen_panel: plan.panel.face,
        chosen_panel_size_mm: [plan.panel.long_span_mm(), plan.panel.short_span_mm()],
        window_exceeds_safe_limit: window_exceeds_safe_limit(params.window_mm, min_dim),
    })
}

/// Generate a watertight perforated Sinterbox [`TriMesh`] and its [`SinterboxMetrics`] without modifying `job`.
pub fn generate_sinterbox_mesh(
    job: &BuildJob,
    instance_ids: &[InstanceId],
    cfg: &SinterboxConfig,
    font_bytes: Option<&[u8]>,
) -> Result<(TriMesh, SinterboxMetrics)> {
    let (bounds, min_dim, default_name) = analyze_sinterbox_inputs(job, instance_ids)?;
    let params = validate_params(cfg)?;
    let plan = build_grid_plan(job, instance_ids, bounds, &params, true)?;

    let label_str = if cfg.label_enabled {
        let trimmed = cfg.label_text.trim();
        if trimmed.is_empty() {
            default_name
        } else {
            trimmed.to_owned()
        }
    } else {
        String::new()
    };

    let layout = if cfg.label_enabled && !label_str.is_empty() {
        Some(layout_label_2d(&label_str, &plan.panel, font_bytes))
    } else {
        None
    };

    let mut solid = build_solid_subgrid(&plan);
    if let Some(ref lbl) = layout {
        apply_label_backing(&plan, lbl, &mut solid);
    }

    let (mut positions, mut indices) = emit_subgrid_mesh(&plan.coords, &solid, plan.sub_dims);

    if let Some(ref lbl) = layout {
        append_extruded_label(
            &plan.panel,
            lbl,
            params.label_depth_mm,
            &mut positions,
            &mut indices,
        );
    }

    let mesh = TriMesh::from_indexed(positions, indices)?;
    let outer = mesh.dimensions();
    let rec_w = recommended_window_size_mm(min_dim);
    let rec_b = recommended_bar_thickness_mm(params.window_mm);

    let metrics = SinterboxMetrics {
        min_part_dim_mm: min_dim,
        recommended_window_mm: rec_w,
        recommended_bar_mm: rec_b,
        outer_size_mm: outer,
        chosen_panel: plan.panel.face,
        chosen_panel_size_mm: [plan.panel.long_span_mm(), plan.panel.short_span_mm()],
        window_exceeds_safe_limit: window_exceeds_safe_limit(params.window_mm, min_dim),
    };

    Ok((mesh, metrics))
}

/// Generate a Sinterbox around `instance_ids`, create a `PartGroup`, lift above `min_z_mm` if needed,
/// and place the cage into `job`.
pub fn create_sinterbox(
    job: &mut BuildJob,
    instance_ids: &[InstanceId],
    cfg: &SinterboxConfig,
    font_bytes: Option<&[u8]>,
) -> Result<SinterboxResult> {
    let (mesh, metrics) = generate_sinterbox_mesh(job, instance_ids, cfg, font_bytes)?;
    let floor_z = job.min_z_mm.max(0.0);
    let lift = if mesh.bounds[0][2] < floor_z {
        floor_z - mesh.bounds[0][2]
    } else {
        0.0
    };

    let raw_label = cfg.label_text.trim();
    let group_seed = cfg.group_name.trim();
    let group_name = if !group_seed.is_empty() {
        group_seed.to_owned()
    } else if cfg.label_enabled && !raw_label.is_empty() {
        format!("Sinterbox: {raw_label}")
    } else {
        format!("Sinterbox #{}", job.groups.len() + 1)
    };
    let part_name = if cfg.label_enabled && !raw_label.is_empty() {
        format!("Sinterbox ({raw_label})")
    } else {
        group_name.clone()
    };

    let gid = job.create_group(group_name);

    let mut target_set = std::collections::HashSet::with_capacity(instance_ids.len());
    for &id in instance_ids {
        target_set.insert(id);
    }
    if lift > 0.0 {
        for inst in &mut job.instances {
            if target_set.contains(&inst.id) {
                inst.transform[2][3] += lift;
            }
        }
    }
    for &id in instance_ids {
        let _ = job.set_instance_group(id, Some(gid));
    }

    let cage_pid = job.add_part(part_name, Arc::new(mesh));
    let mut tf = IDENTITY_TRANSFORM;
    tf[2][3] = lift;
    let cage_iid = job.add_instance(cage_pid, tf)?;
    let _ = job.set_instance_group(cage_iid, Some(gid));
    let _ = job.set_instance_transparent(cage_iid, cfg.cage_transparent);

    Ok(SinterboxResult {
        group_id: gid,
        cage_part_id: cage_pid,
        cage_instance_id: cage_iid,
        lifted_z_mm: lift,
        metrics,
    })
}

#[derive(Debug, Clone, Copy)]
struct ValidatedParams {
    shape: CageShape,
    inner_clearance_mm: f32,
    window_mm: f32,
    bar_mm: f32,
    label_panel: LabelPanel,
    label_depth_mm: f32,
}

fn validate_params(cfg: &SinterboxConfig) -> Result<ValidatedParams> {
    let vals = [
        cfg.inner_clearance_mm,
        cfg.window_size_mm,
        cfg.bar_thickness_mm,
        cfg.label_depth_mm,
    ];
    if vals.iter().any(|v| !v.is_finite()) {
        return Err(NestCraftError::InvalidInput(
            "Sinterbox parameters must be finite numbers".to_owned(),
        ));
    }
    if cfg.inner_clearance_mm < 0.05
        || cfg.window_size_mm < 0.5
        || cfg.bar_thickness_mm < 0.2
        || cfg.label_depth_mm < 0.05
    {
        return Err(NestCraftError::InvalidInput(
            "Sinterbox dimensions must be positive".to_owned(),
        ));
    }
    Ok(ValidatedParams {
        shape: cfg.shape,
        inner_clearance_mm: cfg.inner_clearance_mm.clamp(0.1, 100.0),
        window_mm: cfg.window_size_mm.clamp(0.5, 150.0),
        bar_mm: cfg.bar_thickness_mm.clamp(0.2, 10.0),
        label_panel: cfg.label_panel,
        label_depth_mm: cfg.label_depth_mm.clamp(0.1, 10.0),
    })
}

struct GridPlan {
    n: [usize; 3],
    sub_dims: [usize; 3],
    coords: [Vec<f32>; 3],
    occupied: Vec<bool>,
    panel: ChosenPanel,
}

#[derive(Debug, Clone)]
struct ChosenPanel {
    face: LabelPanel,
    /// Fixed sub-grid index along the face's normal axis (`0..sub_dims[normal_axis]`).
    slice_sub_idx: usize,
    /// Normal axis index (`0=X, 1=Y, 2=Z`).
    normal_axis: usize,
    /// Sub-grid ranges `[start_sub, end_sub]` (inclusive) along `(u_axis, v_axis)`.
    u_sub_range: (usize, usize),
    v_sub_range: (usize, usize),
    /// Physical outer face origin & basis vectors (`u_dir × v_dir == n_dir`).
    origin: [f32; 3],
    u_dir: [f32; 3],
    v_dir: [f32; 3],
    n_dir: [f32; 3],
    u_span_mm: f32,
    v_span_mm: f32,
    /// Axis index (`0=X, 1=Y, 2=Z`) corresponding to `u_dir` and `v_dir`.
    u_axis: usize,
    v_axis: usize,
    /// Sub-grid coordinate index at the start of `u_dir` / `v_dir` (for backing strip mapping).
    u_min_mm: f32,
    v_min_mm: f32,
    /// Whether text runs along `u_dir` (`true`) or rotated +90° along `v_dir` (`false`).
    text_along_u: bool,
}

impl ChosenPanel {
    fn long_span_mm(&self) -> f32 {
        if self.text_along_u {
            self.u_span_mm
        } else {
            self.v_span_mm
        }
    }

    fn short_span_mm(&self) -> f32 {
        if self.text_along_u {
            self.v_span_mm
        } else {
            self.u_span_mm
        }
    }
}

fn build_grid_plan(
    job: &BuildJob,
    instance_ids: &[InstanceId],
    parts_bounds: Aabb,
    params: &ValidatedParams,
    exact_tight_fit: bool,
) -> Result<GridPlan> {
    let clr = params.inner_clearance_mm;
    let b = params.bar_mm;
    let w = params.window_mm;

    let inner_min = [
        parts_bounds[0][0] - clr,
        parts_bounds[0][1] - clr,
        parts_bounds[0][2] - clr,
    ];
    let inner_max = [
        parts_bounds[1][0] + clr,
        parts_bounds[1][1] + clr,
        parts_bounds[1][2] + clr,
    ];

    let mut n = [1usize; 3];
    let mut coords: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut cell_bounds: [Vec<[f32; 2]>; 3] = [Vec::new(), Vec::new(), Vec::new()];

    for k in 0..3 {
        let span = (inner_max[k] - inner_min[k]).max(1.0);
        let nk = (((span + b) / (w + b)).ceil() as usize).clamp(1, MAX_GRID_CELLS_PER_AXIS);
        n[k] = nk;
        let actual_w = ((span - (nk.saturating_sub(1) as f32) * b) / (nk as f32)).max(0.5);

        let mut c = Vec::with_capacity(2 * nk + 2);
        c.push(inner_min[k] - b);
        c.push(inner_min[k]);
        let mut cursor = inner_min[k];
        let mut cb = Vec::with_capacity(nk);
        for idx in 0..nk {
            let cell_start = cursor;
            cursor += actual_w;
            let cell_end = cursor;
            cb.push([cell_start, cell_end]);
            c.push(cursor);
            if idx + 1 < nk {
                cursor += b;
                c.push(cursor);
            }
        }
        c.push(cursor + b);
        coords[k] = c;
        cell_bounds[k] = cb;
    }

    let total_cells = n[0] * n[1] * n[2];
    let mut occupied = if params.shape == CageShape::Box || !exact_tight_fit {
        vec![true; total_cells]
    } else {
        compute_tight_fit_occupancy(job, instance_ids, n, &cell_bounds, clr)?
    };

    if params.shape == CageShape::TightFit && exact_tight_fit {
        fill_enclosed_voids(n, &mut occupied);
        connect_occupied_components(n, &mut occupied);
        eliminate_coarse_pinches(n, &mut occupied);
    }

    let sub_dims = [2 * n[0] + 1, 2 * n[1] + 1, 2 * n[2] + 1];
    let panel = find_best_label_panel(n, &occupied, &coords, params.label_panel);

    Ok(GridPlan {
        n,
        sub_dims,
        coords,
        occupied,
        panel,
    })
}

#[inline]
fn c_idx(n: [usize; 3], x: usize, y: usize, z: usize) -> usize {
    (x * n[1] + y) * n[2] + z
}

fn compute_tight_fit_occupancy(
    job: &BuildJob,
    instance_ids: &[InstanceId],
    n: [usize; 3],
    cell_bounds: &[Vec<[f32; 2]>; 3],
    clearance_mm: f32,
) -> Result<Vec<bool>> {
    let mut occupied = vec![false; n[0] * n[1] * n[2]];
    let clr_sq = clearance_mm * clearance_mm;

    for &iid in instance_ids {
        let inst = job
            .instances
            .iter()
            .find(|i| i.id == iid)
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown instance id {}", iid.0)))?;
        let proto = job
            .part(inst.part_id)
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown part id {}", inst.part_id.0)))?;

        let world_verts: Vec<[f32; 3]> = proto
            .mesh
            .positions
            .iter()
            .map(|&p| transform_point(&inst.transform, p))
            .collect();

        // Always mark the cell containing each instance's centroid so tiny meshes are covered.
        let wb = inst.world_bounds(proto.mesh.bounds);
        let center = [
            0.5 * (wb[0][0] + wb[1][0]),
            0.5 * (wb[0][1] + wb[1][1]),
            0.5 * (wb[0][2] + wb[1][2]),
        ];
        let cx = find_axis_cell(&cell_bounds[0], center[0]);
        let cy = find_axis_cell(&cell_bounds[1], center[1]);
        let cz = find_axis_cell(&cell_bounds[2], center[2]);
        occupied[c_idx(n, cx, cy, cz)] = true;

        for &[i0, i1, i2] in &proto.mesh.indices {
            let a = world_verts[i0 as usize];
            let b = world_verts[i1 as usize];
            let c = world_verts[i2 as usize];
            let tri_min = [
                a[0].min(b[0]).min(c[0]) - clearance_mm,
                a[1].min(b[1]).min(c[1]) - clearance_mm,
                a[2].min(b[2]).min(c[2]) - clearance_mm,
            ];
            let tri_max = [
                a[0].max(b[0]).max(c[0]) + clearance_mm,
                a[1].max(b[1]).max(c[1]) + clearance_mm,
                a[2].max(b[2]).max(c[2]) + clearance_mm,
            ];
            let x_range = axis_cell_range(&cell_bounds[0], tri_min[0], tri_max[0]);
            let y_range = axis_cell_range(&cell_bounds[1], tri_min[1], tri_max[1]);
            let z_range = axis_cell_range(&cell_bounds[2], tri_min[2], tri_max[2]);

            for ix in x_range.0..=x_range.1 {
                let bx = cell_bounds[0][ix];
                for iy in y_range.0..=y_range.1 {
                    let by = cell_bounds[1][iy];
                    for (iz, &bz) in cell_bounds[2]
                        .iter()
                        .enumerate()
                        .take(z_range.1 + 1)
                        .skip(z_range.0)
                    {
                        let idx = c_idx(n, ix, iy, iz);
                        if occupied[idx] {
                            continue;
                        }
                        if tri_to_aabb_dist_sq(a, b, c, [bx[0], by[0], bz[0]], [bx[1], by[1], bz[1]]) <= clr_sq {
                            occupied[idx] = true;
                        }
                    }
                }
            }
        }
    }

    if !occupied.iter().any(|&v| v) {
        occupied.fill(true);
    }

    Ok(occupied)
}

fn find_axis_cell(cells: &[[f32; 2]], val: f32) -> usize {
    for (idx, &[lo, hi]) in cells.iter().enumerate() {
        if val <= hi || idx + 1 == cells.len() {
            let _ = lo;
            return idx;
        }
    }
    0
}

fn axis_cell_range(cells: &[[f32; 2]], min_v: f32, max_v: f32) -> (usize, usize) {
    let last = cells.len().saturating_sub(1);
    let mut lo = last;
    let mut hi = 0usize;
    for (idx, &[c_lo, c_hi]) in cells.iter().enumerate() {
        if c_hi >= min_v && c_lo <= max_v {
            lo = lo.min(idx);
            hi = hi.max(idx);
        }
    }
    if lo > hi { (0, 0) } else { (lo, hi) }
}

/// Conservative & fast squared distance between triangle `(a, b, c)` and axis-aligned box `[bmin, bmax]`.
fn tri_to_aabb_dist_sq(
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    bmin: [f32; 3],
    bmax: [f32; 3],
) -> f32 {
    let box_center = [
        0.5 * (bmin[0] + bmax[0]),
        0.5 * (bmin[1] + bmax[1]),
        0.5 * (bmin[2] + bmax[2]),
    ];
    let p_tri = closest_point_on_triangle(box_center, a, b, c);
    let mut d_tri_box = 0.0_f32;
    for k in 0..3 {
        let clamped = p_tri[k].clamp(bmin[k], bmax[k]);
        let diff = p_tri[k] - clamped;
        d_tri_box += diff * diff;
    }
    // Also check triangle vertices + edge midpoints against the AABB so thin slivers spanning a cell corner are caught.
    let mut best = d_tri_box;
    let samples = [
        a,
        b,
        c,
        [0.5 * (a[0] + b[0]), 0.5 * (a[1] + b[1]), 0.5 * (a[2] + b[2])],
        [0.5 * (b[0] + c[0]), 0.5 * (b[1] + c[1]), 0.5 * (b[2] + c[2])],
        [0.5 * (c[0] + a[0]), 0.5 * (c[1] + a[1]), 0.5 * (c[2] + a[2])],
    ];
    for s in samples {
        let mut d2 = 0.0_f32;
        for k in 0..3 {
            let clamped = s[k].clamp(bmin[k], bmax[k]);
            let diff = s[k] - clamped;
            d2 += diff * diff;
        }
        best = best.min(d2);
    }
    best
}

fn closest_point_on_triangle(p: [f32; 3], a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let sub = |u: [f32; 3], v: [f32; 3]| [u[0] - v[0], u[1] - v[1], u[2] - v[2]];
    let dot = |u: [f32; 3], v: [f32; 3]| u[0] * v[0] + u[1] * v[1] + u[2] * v[2];
    let add_scaled = |u: [f32; 3], v: [f32; 3], s: f32| [u[0] + v[0] * s, u[1] + v[1] * s, u[2] + v[2] * s];

    let ab = sub(b, a);
    let ac = sub(c, a);
    let ap = sub(p, a);
    let d1 = dot(ab, ap);
    let d2 = dot(ac, ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }

    let bp = sub(p, b);
    let d3 = dot(ab, bp);
    let d4 = dot(ac, bp);
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }

    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return add_scaled(a, ab, v);
    }

    let cp = sub(p, c);
    let d5 = dot(ab, cp);
    let d6 = dot(ac, cp);
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }

    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return add_scaled(a, ac, w);
    }

    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return add_scaled(b, sub(c, b), w);
    }

    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    [
        a[0] + ab[0] * v + ac[0] * w,
        a[1] + ab[1] * v + ac[1] * w,
        a[2] + ab[2] * v + ac[2] * w,
    ]
}

/// Fill any empty cells completely trapped inside occupied cells so the cage has no sealed internal cavities.
fn fill_enclosed_voids(n: [usize; 3], occupied: &mut [bool]) {
    let px = n[0] + 2;
    let py = n[1] + 2;
    let pz = n[2] + 2;
    let mut reachable = vec![false; px * py * pz];
    let p_idx = |x: usize, y: usize, z: usize| (x * py + y) * pz + z;

    let mut queue = VecDeque::new();
    reachable[0] = true;
    queue.push_back((0usize, 0usize, 0usize));

    while let Some((x, y, z)) = queue.pop_front() {
        let dirs: [(isize, isize, isize); 6] = [
            (-1, 0, 0),
            (1, 0, 0),
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
        ];
        for (dx, dy, dz) in dirs {
            let nx_i = x as isize + dx;
            let ny_i = y as isize + dy;
            let nz_i = z as isize + dz;
            if nx_i < 0
                || ny_i < 0
                || nz_i < 0
                || nx_i >= px as isize
                || ny_i >= py as isize
                || nz_i >= pz as isize
            {
                continue;
            }
            let (ux, uy, uz) = (nx_i as usize, ny_i as usize, nz_i as usize);
            let pi = p_idx(ux, uy, uz);
            if reachable[pi] {
                continue;
            }
            let is_occ = ux >= 1
                && ux <= n[0]
                && uy >= 1
                && uy <= n[1]
                && uz >= 1
                && uz <= n[2]
                && occupied[c_idx(n, ux - 1, uy - 1, uz - 1)];
            if !is_occ {
                reachable[pi] = true;
                queue.push_back((ux, uy, uz));
            }
        }
    }

    for ix in 0..n[0] {
        for iy in 0..n[1] {
            for iz in 0..n[2] {
                let ci = c_idx(n, ix, iy, iz);
                if !occupied[ci] && !reachable[p_idx(ix + 1, iy + 1, iz + 1)] {
                    occupied[ci] = true;
                }
            }
        }
    }
}

/// Bridge disconnected 6-connected occupied clusters so the Tight Fit Sinterbox is always a single cage.
fn connect_occupied_components(n: [usize; 3], occupied: &mut [bool]) {
    loop {
        let mut visited = vec![false; occupied.len()];
        let mut comps: Vec<Vec<[usize; 3]>> = Vec::new();
        for ix in 0..n[0] {
            for iy in 0..n[1] {
                for iz in 0..n[2] {
                    let start = c_idx(n, ix, iy, iz);
                    if !occupied[start] || visited[start] {
                        continue;
                    }
                    let mut comp = Vec::new();
                    let mut q = VecDeque::new();
                    visited[start] = true;
                    q.push_back([ix, iy, iz]);
                    while let Some([x, y, z]) = q.pop_front() {
                        comp.push([x, y, z]);
                        let nbrs = [
                            (x.checked_sub(1), Some(y), Some(z)),
                            ((x + 1 < n[0]).then_some(x + 1), Some(y), Some(z)),
                            (Some(x), y.checked_sub(1), Some(z)),
                            (Some(x), (y + 1 < n[1]).then_some(y + 1), Some(z)),
                            (Some(x), Some(y), z.checked_sub(1)),
                            (Some(x), Some(y), (z + 1 < n[2]).then_some(z + 1)),
                        ];
                        for (ox, oy, oz) in nbrs {
                            if let (Some(nx), Some(ny), Some(nz)) = (ox, oy, oz) {
                                let ni = c_idx(n, nx, ny, nz);
                                if occupied[ni] && !visited[ni] {
                                    visited[ni] = true;
                                    q.push_back([nx, ny, nz]);
                                }
                            }
                        }
                    }
                    comps.push(comp);
                }
            }
        }

        if comps.len() <= 1 {
            break;
        }

        let mut best_dist = usize::MAX;
        let mut best_pair = ([0usize; 3], [0usize; 3]);
        for &a in &comps[0] {
            for other in comps.iter().skip(1) {
                for &b in other {
                    let d = a[0].abs_diff(b[0]) + a[1].abs_diff(b[1]) + a[2].abs_diff(b[2]);
                    if d < best_dist {
                        best_dist = d;
                        best_pair = (a, b);
                    }
                }
            }
        }

        let (mut cur, target) = best_pair;
        while cur[0] != target[0] {
            if cur[0] < target[0] { cur[0] += 1; } else { cur[0] -= 1; }
            occupied[c_idx(n, cur[0], cur[1], cur[2])] = true;
        }
        while cur[1] != target[1] {
            if cur[1] < target[1] { cur[1] += 1; } else { cur[1] -= 1; }
            occupied[c_idx(n, cur[0], cur[1], cur[2])] = true;
        }
        while cur[2] != target[2] {
            if cur[2] < target[2] { cur[2] += 1; } else { cur[2] -= 1; }
            occupied[c_idx(n, cur[0], cur[1], cur[2])] = true;
        }
    }
}

/// Remove 2x2 planar diagonal checkerboards in the coarse grid so no non-manifold edges can arise.
fn eliminate_coarse_pinches(n: [usize; 3], occupied: &mut [bool]) {
    for _ in 0..8 {
        let mut changed = false;

        // XY slices
        for z in 0..n[2] {
            for x in 0..n[0].saturating_sub(1) {
                for y in 0..n[1].saturating_sub(1) {
                    let a = occupied[c_idx(n, x, y, z)];
                    let b = occupied[c_idx(n, x + 1, y, z)];
                    let c = occupied[c_idx(n, x, y + 1, z)];
                    let d = occupied[c_idx(n, x + 1, y + 1, z)];
                    if a && d && !b && !c {
                        occupied[c_idx(n, x + 1, y, z)] = true;
                        changed = true;
                    } else if b && c && !a && !d {
                        occupied[c_idx(n, x, y, z)] = true;
                        changed = true;
                    }
                }
            }
        }

        // XZ slices
        for y in 0..n[1] {
            for x in 0..n[0].saturating_sub(1) {
                for z in 0..n[2].saturating_sub(1) {
                    let a = occupied[c_idx(n, x, y, z)];
                    let b = occupied[c_idx(n, x + 1, y, z)];
                    let c = occupied[c_idx(n, x, y, z + 1)];
                    let d = occupied[c_idx(n, x + 1, y, z + 1)];
                    if a && d && !b && !c {
                        occupied[c_idx(n, x + 1, y, z)] = true;
                        changed = true;
                    } else if b && c && !a && !d {
                        occupied[c_idx(n, x, y, z)] = true;
                        changed = true;
                    }
                }
            }
        }

        // YZ slices
        for x in 0..n[0] {
            for y in 0..n[1].saturating_sub(1) {
                for z in 0..n[2].saturating_sub(1) {
                    let a = occupied[c_idx(n, x, y, z)];
                    let b = occupied[c_idx(n, x, y + 1, z)];
                    let c = occupied[c_idx(n, x, y, z + 1)];
                    let d = occupied[c_idx(n, x, y + 1, z + 1)];
                    if a && d && !b && !c {
                        occupied[c_idx(n, x, y + 1, z)] = true;
                        changed = true;
                    } else if b && c && !a && !d {
                        occupied[c_idx(n, x, y, z)] = true;
                        changed = true;
                    }
                }
            }
        }

        if !changed {
            break;
        }
    }
}

fn find_best_label_panel(
    n: [usize; 3],
    occupied: &[bool],
    coords: &[Vec<f32>; 3],
    requested: LabelPanel,
) -> ChosenPanel {
    let mut best_overall: Option<(f32, ChosenPanel)> = None;
    let mut best_requested: Option<(f32, ChosenPanel)> = None;

    for face in LabelPanel::FACES {
        if let Some((area, cand)) = best_panel_on_face(n, occupied, coords, face) {
            if best_overall.as_ref().is_none_or(|(a, _)| area > *a * 1.001) {
                best_overall = Some((area, cand.clone()));
            }
            if requested == face && best_requested.as_ref().is_none_or(|(a, _)| area > *a) {
                best_requested = Some((area, cand));
            }
        }
    }

    if let Some((_, p)) = best_requested {
        return p;
    }
    if let Some((_, p)) = best_overall {
        return p;
    }

    // Fallback (unreachable on non-empty grid, kept panic-free)
    let x0 = coords[0].first().copied().unwrap_or(0.0);
    let x1 = coords[0].last().copied().unwrap_or(10.0);
    let y0 = coords[1].first().copied().unwrap_or(0.0);
    let y1 = coords[1].last().copied().unwrap_or(10.0);
    let z1 = coords[2].last().copied().unwrap_or(10.0);
    let u_span = (x1 - x0).max(1.0);
    let v_span = (y1 - y0).max(1.0);
    ChosenPanel {
        face: LabelPanel::TopPosZ,
        slice_sub_idx: 2 * n[2],
        normal_axis: 2,
        u_sub_range: (0, 2 * n[0]),
        v_sub_range: (0, 2 * n[1]),
        origin: [x0, y0, z1],
        u_dir: [1.0, 0.0, 0.0],
        v_dir: [0.0, 1.0, 0.0],
        n_dir: [0.0, 0.0, 1.0],
        u_span_mm: u_span,
        v_span_mm: v_span,
        u_axis: 0,
        v_axis: 1,
        u_min_mm: x0,
        v_min_mm: y0,
        text_along_u: u_span >= v_span,
    }
}

fn best_panel_on_face(
    n: [usize; 3],
    occupied: &[bool],
    coords: &[Vec<f32>; 3],
    face: LabelPanel,
) -> Option<(f32, ChosenPanel)> {
    let (normal_axis, normal_sign, u_axis, v_axis) = match face {
        LabelPanel::TopPosZ | LabelPanel::AutoLargest => (2, 1i8, 0, 1),
        LabelPanel::BottomNegZ => (2, -1i8, 0, 1),
        LabelPanel::FrontNegY => (1, -1i8, 0, 2),
        LabelPanel::BackPosY => (1, 1i8, 0, 2),
        LabelPanel::RightPosX => (0, 1i8, 1, 2),
        LabelPanel::LeftNegX => (0, -1i8, 1, 2),
    };

    let nu = n[u_axis];
    let nv = n[v_axis];
    let nw = n[normal_axis];

    let mut best_area = 0.0_f32;
    let mut best_panel: Option<ChosenPanel> = None;

    for w_cell in 0..nw {
        let mut mask = vec![false; nu * nv];
        let mut any = false;
        for u in 0..nu {
            for v in 0..nv {
                let mut cur = [0usize; 3];
                cur[u_axis] = u;
                cur[v_axis] = v;
                cur[normal_axis] = w_cell;
                if !occupied[c_idx(n, cur[0], cur[1], cur[2])] {
                    continue;
                }
                let neighbor_occ = if normal_sign > 0 {
                    if w_cell + 1 < nw {
                        let mut nxt = cur;
                        nxt[normal_axis] = w_cell + 1;
                        occupied[c_idx(n, nxt[0], nxt[1], nxt[2])]
                    } else {
                        false
                    }
                } else if w_cell > 0 {
                    let mut nxt = cur;
                    nxt[normal_axis] = w_cell - 1;
                    occupied[c_idx(n, nxt[0], nxt[1], nxt[2])]
                } else {
                    false
                };
                if !neighbor_occ {
                    mask[u * nv + v] = true;
                    any = true;
                }
            }
        }
        if !any {
            continue;
        }

        // Largest contiguous rectangle in coarse cell coordinates (maximizing physical mm² area).
        let mut heights = vec![0usize; nv];
        let mut stack: Vec<(usize, usize)> = Vec::with_capacity(nv);
        for u in 0..nu {
            for v in 0..nv {
                if mask[u * nv + v] {
                    heights[v] += 1;
                } else {
                    heights[v] = 0;
                }
            }
            stack.clear();
            for (v, h) in heights.iter().copied().chain(std::iter::once(0)).enumerate() {
                let mut start_v = v;
                while let Some(&(prev_v, prev_h)) = stack.last() {
                    if prev_h <= h {
                        break;
                    }
                    stack.pop();
                    let u1 = u;
                    let u0 = u + 1 - prev_h;
                    let v0 = prev_v;
                    let v1 = v - 1;

                    let su0 = 2 * u0;
                    let su1 = 2 * u1 + 2;
                    let sv0 = 2 * v0;
                    let sv1 = 2 * v1 + 2;
                    let span_u = (coords[u_axis][su1 + 1] - coords[u_axis][su0]).max(0.0);
                    let span_v = (coords[v_axis][sv1 + 1] - coords[v_axis][sv0]).max(0.0);
                    let area = span_u * span_v;
                    if area > best_area {
                        best_area = area;
                        let slice_sub_idx = if normal_sign > 0 { 2 * w_cell + 2 } else { 2 * w_cell };
                        let n_coord = if normal_sign > 0 {
                            coords[normal_axis][slice_sub_idx + 1]
                        } else {
                            coords[normal_axis][slice_sub_idx]
                        };
                        let u_lo = coords[u_axis][su0];
                        let u_hi = coords[u_axis][su1 + 1];
                        let v_lo = coords[v_axis][sv0];
                        let v_hi = coords[v_axis][sv1 + 1];
                        let cand = make_chosen_panel(
                            face,
                            slice_sub_idx,
                            normal_axis,
                            normal_sign,
                            n_coord,
                            u_axis,
                            v_axis,
                            (su0, su1),
                            (sv0, sv1),
                            (u_lo, u_hi),
                            (v_lo, v_hi),
                        );
                        best_panel = Some(cand);
                    }
                    start_v = prev_v;
                }
                if h > 0 && stack.last().is_none_or(|&(_, top_h)| top_h < h) {
                    stack.push((start_v, h));
                }
            }
        }
    }

    best_panel.map(|p| (best_area, p))
}

#[allow(clippy::too_many_arguments)]
fn make_chosen_panel(
    face: LabelPanel,
    slice_sub_idx: usize,
    normal_axis: usize,
    normal_sign: i8,
    n_coord: f32,
    u_axis: usize,
    v_axis: usize,
    u_sub_range: (usize, usize),
    v_sub_range: (usize, usize),
    u_mm: (f32, f32),
    v_mm: (f32, f32),
) -> ChosenPanel {
    let mut origin = [0.0_f32; 3];
    origin[normal_axis] = n_coord;

    // Orient (u_dir, v_dir) so u_dir x v_dir == n_dir and text is upright when viewed from outside.
    let (u_flip, v_flip) = match face {
        LabelPanel::TopPosZ | LabelPanel::AutoLargest => (false, false), // (+X, +Y) -> +Z
        LabelPanel::BottomNegZ => (false, true),                         // (+X, -Y) -> -Z
        LabelPanel::FrontNegY => (false, false),                         // (+X, +Z) -> -Y
        LabelPanel::BackPosY => (true, false),                           // (-X, +Z) -> +Y
        LabelPanel::RightPosX => (false, false),                         // (+Y, +Z) -> +X
        LabelPanel::LeftNegX => (true, false),                           // (-Y, +Z) -> -X
    };

    let mut u_dir = [0.0_f32; 3];
    u_dir[u_axis] = if u_flip { -1.0 } else { 1.0 };
    origin[u_axis] = if u_flip { u_mm.1 } else { u_mm.0 };

    let mut v_dir = [0.0_f32; 3];
    v_dir[v_axis] = if v_flip { -1.0 } else { 1.0 };
    origin[v_axis] = if v_flip { v_mm.1 } else { v_mm.0 };

    let mut n_dir = [0.0_f32; 3];
    n_dir[normal_axis] = f32::from(normal_sign);

    let u_span_mm = (u_mm.1 - u_mm.0).max(0.1);
    let v_span_mm = (v_mm.1 - v_mm.0).max(0.1);
    let text_along_u = u_span_mm >= v_span_mm;

    ChosenPanel {
        face,
        slice_sub_idx,
        normal_axis,
        u_sub_range,
        v_sub_range,
        origin,
        u_dir,
        v_dir,
        n_dir,
        u_span_mm,
        v_span_mm,
        u_axis,
        v_axis,
        u_min_mm: u_mm.0,
        v_min_mm: v_mm.0,
        text_along_u,
    }
}

#[inline]
fn s_idx(sub_dims: [usize; 3], x: usize, y: usize, z: usize) -> usize {
    (x * sub_dims[1] + y) * sub_dims[2] + z
}

fn build_solid_subgrid(plan: &GridPlan) -> Vec<bool> {
    let [sx_max, sy_max, sz_max] = plan.sub_dims;
    let mut solid = vec![false; sx_max * sy_max * sz_max];

    let touches_axis = |s: usize, nk: usize| -> [Option<Option<usize>>; 2] {
        if s % 2 == 1 {
            [Some(Some(s / 2)), None]
        } else {
            let left = (s > 0).then(|| s / 2 - 1);
            let right = (s / 2 < nk).then_some(s / 2);
            [Some(left), Some(right)]
        }
    };

    for sx in 0..sx_max {
        let xs = touches_axis(sx, plan.n[0]);
        let ex = usize::from(sx % 2 == 0);
        for sy in 0..sy_max {
            let ys = touches_axis(sy, plan.n[1]);
            let ey = usize::from(sy % 2 == 0);
            for sz in 0..sz_max {
                let zs = touches_axis(sz, plan.n[2]);
                let ez = usize::from(sz % 2 == 0);
                let even_count = ex + ey + ez;
                if even_count < 2 {
                    continue;
                }
                let mut any_occ = false;
                let mut any_empty = false;
                for ox in xs.into_iter().flatten() {
                    for oy in ys.into_iter().flatten() {
                        for oz in zs.into_iter().flatten() {
                            match (ox, oy, oz) {
                                (Some(cx), Some(cy), Some(cz)) if plan.occupied[c_idx(plan.n, cx, cy, cz)] => {
                                    any_occ = true;
                                }
                                _ => {
                                    any_empty = true;
                                }
                            }
                        }
                    }
                }
                if any_occ && any_empty {
                    solid[s_idx(plan.sub_dims, sx, sy, sz)] = true;
                }
            }
        }
    }

    solid
}

fn apply_label_backing(plan: &GridPlan, label: &LabelLayout2D, solid: &mut [bool]) {
    let p = &plan.panel;
    let [sx_max, sy_max, sz_max] = plan.sub_dims;
    let w = p.slice_sub_idx;

    let (short_axis, short_min_mm, band_lo, band_hi, long_range, short_range) = if p.text_along_u {
        (
            p.v_axis,
            p.v_min_mm,
            label.v_min - 1.0,
            label.v_max + 1.0,
            p.u_sub_range,
            p.v_sub_range,
        )
    } else {
        (
            p.u_axis,
            p.u_min_mm,
            label.u_min - 1.0,
            label.u_max + 1.0,
            p.v_sub_range,
            p.u_sub_range,
        )
    };

    let _ = short_min_mm;
    let (long_axis, long_range_ref, short_range_ref) = if p.text_along_u {
        (p.u_axis, long_range, short_range)
    } else {
        (p.v_axis, long_range, short_range)
    };

    for s_short in short_range_ref.0..=short_range_ref.1 {
        let lo = plan.coords[short_axis][s_short];
        let hi = plan.coords[short_axis][s_short + 1];
        if hi < band_lo || lo > band_hi {
            continue;
        }
        for s_long in long_range_ref.0..=long_range_ref.1 {
            let mut sub = [0usize; 3];
            sub[p.normal_axis] = w;
            sub[short_axis] = s_short;
            sub[long_axis] = s_long;
            if sub[0] < sx_max && sub[1] < sy_max && sub[2] < sz_max {
                solid[s_idx(plan.sub_dims, sub[0], sub[1], sub[2])] = true;
            }
        }
    }
}

fn emit_subgrid_mesh(
    coords: &[Vec<f32>; 3],
    solid: &[bool],
    sub_dims: [usize; 3],
) -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
    let [sx, sy, sz] = sub_dims;
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<[u32; 3]> = Vec::new();
    let mut vert_map: HashMap<(usize, usize, usize), u32> = HashMap::new();

    let mut intern = |ix: usize, iy: usize, iz: usize| -> u32 {
        if let Some(&id) = vert_map.get(&(ix, iy, iz)) {
            return id;
        }
        let id = positions.len() as u32;
        positions.push([coords[0][ix], coords[1][iy], coords[2][iz]]);
        vert_map.insert((ix, iy, iz), id);
        id
    };

    let is_solid = |x: isize, y: isize, z: isize| -> bool {
        if x < 0 || y < 0 || z < 0 || x >= sx as isize || y >= sy as isize || z >= sz as isize {
            return false;
        }
        solid[s_idx(sub_dims, x as usize, y as usize, z as usize)]
    };

    for x in 0..sx {
        for y in 0..sy {
            for z in 0..sz {
                if !solid[s_idx(sub_dims, x, y, z)] {
                    continue;
                }
                let (xi, yi, zi) = (x as isize, y as isize, z as isize);

                // -X
                if !is_solid(xi - 1, yi, zi) {
                    let a = intern(x, y + 1, z);
                    let b = intern(x, y, z);
                    let c = intern(x, y, z + 1);
                    let d = intern(x, y + 1, z + 1);
                    indices.push([a, b, c]);
                    indices.push([a, c, d]);
                }
                // +X
                if !is_solid(xi + 1, yi, zi) {
                    let a = intern(x + 1, y, z);
                    let b = intern(x + 1, y + 1, z);
                    let c = intern(x + 1, y + 1, z + 1);
                    let d = intern(x + 1, y, z + 1);
                    indices.push([a, b, c]);
                    indices.push([a, c, d]);
                }
                // -Y
                if !is_solid(xi, yi - 1, zi) {
                    let a = intern(x, y, z);
                    let b = intern(x + 1, y, z);
                    let c = intern(x + 1, y, z + 1);
                    let d = intern(x, y, z + 1);
                    indices.push([a, b, c]);
                    indices.push([a, c, d]);
                }
                // +Y
                if !is_solid(xi, yi + 1, zi) {
                    let a = intern(x + 1, y + 1, z);
                    let b = intern(x, y + 1, z);
                    let c = intern(x, y + 1, z + 1);
                    let d = intern(x + 1, y + 1, z + 1);
                    indices.push([a, b, c]);
                    indices.push([a, c, d]);
                }
                // -Z
                if !is_solid(xi, yi, zi - 1) {
                    let a = intern(x, y + 1, z);
                    let b = intern(x + 1, y + 1, z);
                    let c = intern(x + 1, y, z);
                    let d = intern(x, y, z);
                    indices.push([a, b, c]);
                    indices.push([a, c, d]);
                }
                // +Z
                if !is_solid(xi, yi, zi + 1) {
                    let a = intern(x, y, z + 1);
                    let b = intern(x + 1, y, z + 1);
                    let c = intern(x + 1, y + 1, z + 1);
                    let d = intern(x, y + 1, z + 1);
                    indices.push([a, b, c]);
                    indices.push([a, c, d]);
                }
            }
        }
    }

    (positions, indices)
}

#[derive(Debug, Clone)]
struct Poly2D {
    /// Original loop rings (outer first or mixed; normalized so outer is CCW, holes are CW).
    loops: Vec<Vec<[f32; 2]>>,
    /// Triangulated vertex list and CCW triangles into `verts`.
    verts: Vec<[f32; 2]>,
    tris: Vec<[u32; 3]>,
}

struct LabelLayout2D {
    polys: Vec<Poly2D>,
    u_min: f32,
    u_max: f32,
    v_min: f32,
    v_max: f32,
}

fn layout_label_2d(text: &str, panel: &ChosenPanel, font_bytes: Option<&[u8]>) -> LabelLayout2D {
    let mut raw_polys = Vec::new();
    let mut cursor_x = 0.0_f32;
    let mut ascent_ref = 1.0_f32;

    if let Some(bytes) = font_bytes
        && let Ok(mut face) = ttf_parser::Face::parse(bytes, 0)
    {
        face.set_variation(ttf_parser::Tag::from_bytes(b"wght"), 700.0);
        let upm = f32::from(face.units_per_em().max(1));
        ascent_ref = (f32::from(face.ascender()) / upm).clamp(0.5, 1.4);

        for ch in text.chars() {
            if ch.is_control() {
                continue;
            }
            if ch == ' ' {
                cursor_x += 0.35;
                continue;
            }
            if let Some(gid) = face.glyph_index(ch) {
                let adv = face
                    .glyph_hor_advance(gid)
                    .map(|a| f32::from(a) / upm)
                    .unwrap_or(0.55);
                if let Some(poly) = outline_glyph(&face, gid, upm, cursor_x) {
                    raw_polys.push(poly);
                } else if !ch.is_whitespace() {
                    raw_polys.extend(fallback_char_polys(ch, cursor_x));
                }
                cursor_x += adv.max(0.2);
            } else {
                raw_polys.extend(fallback_char_polys(ch, cursor_x));
                cursor_x += 0.68;
            }
        }
    } else {
        for ch in text.chars() {
            if ch == ' ' {
                cursor_x += 0.4;
            } else {
                raw_polys.extend(fallback_char_polys(ch, cursor_x));
                cursor_x += 0.68;
            }
        }
    }

    if raw_polys.is_empty() {
        raw_polys.extend(fallback_char_polys('?', 0.0));
    }

    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for poly in &raw_polys {
        for p in &poly.verts {
            min_x = min_x.min(p[0]);
            max_x = max_x.max(p[0]);
            min_y = min_y.min(p[1]);
            max_y = max_y.max(p[1]);
        }
    }
    let text_w = (max_x - min_x).max(0.2);
    let text_h = (max_y - min_y).max(0.2 * ascent_ref);
    let text_cx = 0.5 * (min_x + max_x);
    let text_cy = 0.5 * (min_y + max_y);

    let max_long = (panel.long_span_mm() * 0.85).max(1.0);
    let max_short = (panel.short_span_mm() * 0.68).max(1.0);
    let scale = (max_long / text_w).min(max_short / text_h).clamp(0.5, 24.0);

    let panel_cu = 0.5 * panel.u_span_mm;
    let panel_cv = 0.5 * panel.v_span_mm;

    let map_pt = |p: [f32; 2]| -> [f32; 2] {
        let lx = (p[0] - text_cx) * scale;
        let ly = (p[1] - text_cy) * scale;
        if panel.text_along_u {
            [panel_cu + lx, panel_cv + ly]
        } else {
            // Rotate +90° in (u, v) so text reads along v_dir (bottom-to-top) and height spans -u_dir.
            [panel_cu - ly, panel_cv + lx]
        }
    };

    let mut u_min = f32::INFINITY;
    let mut u_max = f32::NEG_INFINITY;
    let mut v_min = f32::INFINITY;
    let mut v_max = f32::NEG_INFINITY;

    for poly in &mut raw_polys {
        for p in &mut poly.verts {
            *p = map_pt(*p);
            u_min = u_min.min(p[0]);
            u_max = u_max.max(p[0]);
            v_min = v_min.min(p[1]);
            v_max = v_max.max(p[1]);
        }
        for ring in &mut poly.loops {
            for p in ring.iter_mut() {
                *p = map_pt(*p);
            }
        }
    }

    LabelLayout2D {
        polys: raw_polys,
        u_min: panel.u_min_mm + u_min,
        u_max: panel.u_min_mm + u_max,
        v_min: panel.v_min_mm + v_min,
        v_max: panel.v_min_mm + v_max,
    }
}

fn append_extruded_label(
    panel: &ChosenPanel,
    layout: &LabelLayout2D,
    depth_mm: f32,
    positions: &mut Vec<[f32; 3]>,
    indices: &mut Vec<[u32; 3]>,
) {
    let z0 = -0.10_f32; // Slightly seated inside the solid backing wall
    let z1 = depth_mm;

    let to_world = |uv: [f32; 2], n_off: f32| -> [f32; 3] {
        [
            panel.origin[0] + panel.u_dir[0] * uv[0] + panel.v_dir[0] * uv[1] + panel.n_dir[0] * n_off,
            panel.origin[1] + panel.u_dir[1] * uv[0] + panel.v_dir[1] * uv[1] + panel.n_dir[1] * n_off,
            panel.origin[2] + panel.u_dir[2] * uv[0] + panel.v_dir[2] * uv[1] + panel.n_dir[2] * n_off,
        ]
    };

    for poly in &layout.polys {
        if poly.tris.is_empty() {
            continue;
        }
        // Build vertex pairs per loop so side walls share exact cap boundary vertices!
        // Note: `poly.verts` stores vertices in loop order concatenated across `poly.loops`.
        let n_v = poly.verts.len();
        let base_bot = positions.len() as u32;
        for &uv in &poly.verts {
            positions.push(to_world(uv, z0));
        }
        let base_top = positions.len() as u32;
        for &uv in &poly.verts {
            positions.push(to_world(uv, z1));
        }

        // Front (+n) and back (-n) caps
        for &[a, b, c] in &poly.tris {
            if a as usize >= n_v || b as usize >= n_v || c as usize >= n_v {
                continue;
            }
            indices.push([base_top + a, base_top + b, base_top + c]);
            indices.push([base_bot + a, base_bot + c, base_bot + b]);
        }

        // Side walls along each boundary ring (outer is CCW, holes are CW)
        let mut offset = 0u32;
        for ring in &poly.loops {
            let m = ring.len() as u32;
            if m < 3 {
                offset += m;
                continue;
            }
            for i in 0..m {
                let j = (i + 1) % m;
                let b0 = base_bot + offset + i;
                let b1 = base_bot + offset + j;
                let t0 = base_top + offset + i;
                let t1 = base_top + offset + j;
                indices.push([b0, b1, t1]);
                indices.push([b0, t1, t0]);
            }
            offset += m;
        }
    }
}

struct OutlineCollector {
    scale: f32,
    x_off: f32,
    loops: Vec<Vec<[f32; 2]>>,
    cur: Vec<[f32; 2]>,
    last: [f32; 2],
}

impl OutlineCollector {
    fn push_pt(&mut self, p: [f32; 2]) {
        if let Some(&prev) = self.cur.last() {
            let dx = p[0] - prev[0];
            let dy = p[1] - prev[1];
            if dx * dx + dy * dy < 1e-8 {
                return;
            }
        }
        self.cur.push(p);
        self.last = p;
    }

    fn finish_contour(&mut self) {
        if self.cur.len() >= 2 {
            let first = self.cur[0];
            if let Some(&last) = self.cur.last() {
                let dx = first[0] - last[0];
                let dy = first[1] - last[1];
                if dx * dx + dy * dy < 1e-8 {
                    self.cur.pop();
                }
            }
        }
        if self.cur.len() >= 3 {
            let clean = clean_ring(&self.cur);
            if clean.len() >= 3 && signed_area_2d(&clean).abs() > 1e-6 {
                self.loops.push(clean);
            }
        }
        self.cur.clear();
    }
}

impl ttf_parser::OutlineBuilder for OutlineCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.finish_contour();
        let p = [self.x_off + x * self.scale, y * self.scale];
        self.cur.push(p);
        self.last = p;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.push_pt([self.x_off + x * self.scale, y * self.scale]);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let p0 = self.last;
        let p1 = [self.x_off + x1 * self.scale, y1 * self.scale];
        let p2 = [self.x_off + x * self.scale, y * self.scale];
        const STEPS: usize = 5;
        for s in 1..=STEPS {
            let t = s as f32 / STEPS as f32;
            let u = 1.0 - t;
            self.push_pt([
                u * u * p0[0] + 2.0 * u * t * p1[0] + t * t * p2[0],
                u * u * p0[1] + 2.0 * u * t * p1[1] + t * t * p2[1],
            ]);
        }
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let p0 = self.last;
        let p1 = [self.x_off + x1 * self.scale, y1 * self.scale];
        let p2 = [self.x_off + x2 * self.scale, y2 * self.scale];
        let p3 = [self.x_off + x * self.scale, y * self.scale];
        const STEPS: usize = 6;
        for s in 1..=STEPS {
            let t = s as f32 / STEPS as f32;
            let u = 1.0 - t;
            self.push_pt([
                u * u * u * p0[0] + 3.0 * u * u * t * p1[0] + 3.0 * u * t * t * p2[0] + t * t * t * p3[0],
                u * u * u * p0[1] + 3.0 * u * u * t * p1[1] + 3.0 * u * t * t * p2[1] + t * t * t * p3[1],
            ]);
        }
    }

    fn close(&mut self) {
        self.finish_contour();
    }
}

fn outline_glyph(
    face: &ttf_parser::Face<'_>,
    gid: ttf_parser::GlyphId,
    upm: f32,
    x_off: f32,
) -> Option<Poly2D> {
    let mut collector = OutlineCollector {
        scale: 1.0 / upm,
        x_off,
        loops: Vec::new(),
        cur: Vec::new(),
        last: [0.0, 0.0],
    };
    face.outline_glyph(gid, &mut collector)?;
    collector.finish_contour();
    if collector.loops.is_empty() {
        return None;
    }
    triangulate_loops(&collector.loops)
}

fn clean_ring(pts: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let mut out = pts.to_vec();
    for _ in 0..4 {
        let n = out.len();
        if n < 3 {
            break;
        }
        let mut next = Vec::with_capacity(n);
        for i in 0..n {
            let a = out[(i + n - 1) % n];
            let b = out[i];
            let c = out[(i + 1) % n];
            let ab = [b[0] - a[0], b[1] - a[1]];
            let bc = [c[0] - b[0], c[1] - b[1]];
            let len_ab = (ab[0] * ab[0] + ab[1] * ab[1]).sqrt();
            let len_bc = (bc[0] * bc[0] + bc[1] * bc[1]).sqrt();
            if len_ab < 1e-5 || len_bc < 1e-5 {
                continue;
            }
            let cross = ab[0] * bc[1] - ab[1] * bc[0];
            if cross.abs() < 1e-6 * len_ab * len_bc {
                // Skip strictly collinear points
                continue;
            }
            next.push(b);
        }
        if next.len() == out.len() {
            break;
        }
        out = next;
    }
    out
}

fn signed_area_2d(pts: &[[f32; 2]]) -> f32 {
    let n = pts.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = 0.0_f32;
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    0.5 * sum
}

fn point_in_ring(p: [f32; 2], ring: &[[f32; 2]]) -> bool {
    let n = ring.len();
    let mut inside = false;
    let mut j = n.saturating_sub(1);
    for i in 0..n {
        let pi = ring[i];
        let pj = ring[j];
        let intersect = ((pi[1] > p[1]) != (pj[1] > p[1]))
            && (p[0] < (pj[0] - pi[0]) * (p[1] - pi[1]) / ((pj[1] - pi[1]) + 1e-18) + pi[0]);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Normalize arbitrary font loops (using nesting depth parity so TrueType & CFF both work),
/// bridge holes into outer loops, and ear-clip triangulate into a single [`Poly2D`].
fn triangulate_loops(raw_loops: &[Vec<[f32; 2]>]) -> Option<Poly2D> {
    let n = raw_loops.len();
    if n == 0 {
        return None;
    }
    let areas: Vec<f32> = raw_loops.iter().map(|l| signed_area_2d(l).abs()).collect();
    let mut depths = vec![0usize; n];
    let mut parent = vec![None::<usize>; n];

    for i in 0..n {
        let probe = raw_loops[i][0];
        let mut best_parent_area = f32::INFINITY;
        for j in 0..n {
            if i == j || areas[j] <= areas[i] {
                continue;
            }
            if point_in_ring(probe, &raw_loops[j]) {
                depths[i] += 1;
                if areas[j] < best_parent_area {
                    best_parent_area = areas[j];
                    parent[i] = Some(j);
                }
            }
        }
    }

    let mut ordered_loops: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut all_verts: Vec<[f32; 2]> = Vec::new();
    let mut all_tris: Vec<[u32; 3]> = Vec::new();

    for i in 0..n {
        if !depths[i].is_multiple_of(2) {
            continue;
        }
        // Outer loop: enforce CCW
        let mut outer = raw_loops[i].clone();
        if signed_area_2d(&outer) < 0.0 {
            outer.reverse();
        }

        // Collect direct child holes (depth == depths[i] + 1 and parent == Some(i))
        let mut holes: Vec<Vec<[f32; 2]>> = Vec::new();
        for j in 0..n {
            if parent[j] == Some(i) && depths[j] % 2 == 1 {
                let mut hole = raw_loops[j].clone();
                if signed_area_2d(&hole) > 0.0 {
                    hole.reverse(); // Holes must be CW
                }
                holes.push(hole);
            }
        }

        // Sort holes by rightmost X descending for standard David Eberly hole-bridging
        holes.sort_by(|a, b| {
            let ax = a.iter().map(|p| p[0]).fold(f32::NEG_INFINITY, f32::max);
            let bx = b.iter().map(|p| p[0]).fold(f32::NEG_INFINITY, f32::max);
            bx.total_cmp(&ax)
        });

        let base_vert = all_verts.len() as u32;
        let outer_len = outer.len();
        all_verts.extend_from_slice(&outer);
        ordered_loops.push(outer.clone());

        let mut merged: Vec<u32> = (0..outer_len as u32).map(|k| base_vert + k).collect();

        for hole in holes {
            let hole_start = all_verts.len() as u32;
            let hole_len = hole.len();
            all_verts.extend_from_slice(&hole);
            ordered_loops.push(hole);

            let hole_indices: Vec<u32> = (0..hole_len as u32).map(|k| hole_start + k).collect();
            merged = bridge_hole_into_outer(&all_verts, &merged, &hole_indices)?;
        }

        let tris = ear_clip_indices(&all_verts, &merged)?;
        all_tris.extend(tris);
    }

    if all_tris.is_empty() {
        return None;
    }

    let poly = Poly2D {
        loops: ordered_loops,
        verts: all_verts,
        tris: all_tris,
    };
    if !verify_poly2d_manifold(&poly) {
        return None;
    }
    Some(poly)
}

fn verify_poly2d_manifold(poly: &Poly2D) -> bool {
    let mut boundary_edges: HashMap<(u32, u32), i32> = HashMap::new();
    let mut offset = 0u32;
    for ring in &poly.loops {
        let m = ring.len() as u32;
        if m < 3 {
            return false;
        }
        for i in 0..m {
            let u = offset + i;
            let v = offset + ((i + 1) % m);
            *boundary_edges.entry((u, v)).or_insert(0) += 1;
        }
        offset += m;
    }

    let mut tri_edges: HashMap<(u32, u32), i32> = HashMap::new();
    for &[a, b, c] in &poly.tris {
        if a == b || b == c || c == a {
            return false;
        }
        for (u, v) in [(a, b), (b, c), (c, a)] {
            *tri_edges.entry((u, v)).or_insert(0) += 1;
        }
    }

    for (&(u, v), &count) in &tri_edges {
        if count != 1 {
            return false;
        }
        let rev = tri_edges.get(&(v, u)).copied().unwrap_or(0);
        let bnd = boundary_edges.get(&(u, v)).copied().unwrap_or(0);
        if bnd == 1 {
            if rev != 0 {
                return false;
            }
        } else if rev != 1 {
            return false;
        }
    }

    for (&(u, v), &bnd) in &boundary_edges {
        if bnd != 1 || tri_edges.get(&(u, v)).copied().unwrap_or(0) != 1 {
            return false;
        }
    }
    true
}

fn bridge_hole_into_outer(
    verts: &[[f32; 2]],
    outer: &[u32],
    hole: &[u32],
) -> Option<Vec<u32>> {
    if outer.len() < 3 || hole.len() < 3 {
        return None;
    }
    let mut m_pos = 0usize;
    let mut max_x = f32::NEG_INFINITY;
    for (idx, &vi) in hole.iter().enumerate() {
        if verts[vi as usize][0] > max_x {
            max_x = verts[vi as usize][0];
            m_pos = idx;
        }
    }
    let m_idx = hole[m_pos];
    let m = verts[m_idx as usize];

    // Ray cast +X from `m` against edges of `outer`
    let n_out = outer.len();
    let mut best_t = f32::INFINITY;
    let mut best_edge = None;
    let mut best_ix = [0.0_f32; 2];

    for i in 0..n_out {
        let a = verts[outer[i] as usize];
        let b = verts[outer[(i + 1) % n_out] as usize];
        if (a[1] <= m[1] && b[1] > m[1]) || (b[1] <= m[1] && a[1] > m[1]) {
            let t_edge = (m[1] - a[1]) / (b[1] - a[1]);
            let x_int = a[0] + t_edge * (b[0] - a[0]);
            if x_int >= m[0] - 1e-6 && (x_int - m[0]) < best_t {
                best_t = x_int - m[0];
                best_edge = Some(i);
                best_ix = [x_int, m[1]];
            }
        }
    }

    let edge_i = best_edge?;
    let i0 = edge_i;
    let i1 = (edge_i + 1) % n_out;
    let p_pos = if verts[outer[i0] as usize][0] >= verts[outer[i1] as usize][0] {
        i0
    } else {
        i1
    };
    let p_pt = verts[outer[p_pos] as usize];

    // Check if any reflex vertex of `outer` lies inside triangle (m, best_ix, p_pt)
    let mut visible_pos = p_pos;
    let mut best_angle = f32::INFINITY;
    for (k, &vk) in outer.iter().enumerate() {
        if vk == outer[p_pos] {
            continue;
        }
        let q = verts[vk as usize];
        if q[0] <= m[0] {
            continue;
        }
        if point_in_tri_2d(q, m, best_ix, p_pt) {
            let dx = q[0] - m[0];
            let dy = (q[1] - m[1]).abs();
            let ang = dy.atan2(dx);
            if ang < best_angle {
                best_angle = ang;
                visible_pos = k;
            }
        }
    }

    let mut out = Vec::with_capacity(outer.len() + hole.len() + 2);
    for (k, &v) in outer.iter().enumerate() {
        out.push(v);
        if k == visible_pos {
            for step in 0..=hole.len() {
                out.push(hole[(m_pos + step) % hole.len()]);
            }
            out.push(v);
        }
    }
    Some(out)
}

fn ear_clip_indices(verts: &[[f32; 2]], ring: &[u32]) -> Option<Vec<[u32; 3]>> {
    if ring.len() < 3 {
        return None;
    }
    let mut rem = ring.to_vec();
    let mut tris = Vec::with_capacity(rem.len().saturating_sub(2));
    let mut guard = rem.len() * rem.len() + 16;

    while rem.len() > 3 {
        if guard == 0 {
            return None;
        }
        guard -= 1;

        let n = rem.len();
        let mut clipped = false;
        for i in 0..n {
            let prev = rem[(i + n - 1) % n];
            let curr = rem[i];
            let next = rem[(i + 1) % n];

            if prev == curr || curr == next || prev == next {
                rem.remove(i);
                clipped = true;
                break;
            }

            let a = verts[prev as usize];
            let b = verts[curr as usize];
            let c = verts[next as usize];
            let area2 = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            if area2 <= 1e-12 {
                continue;
            }

            let mut contains = false;
            for (k, &vk) in rem.iter().enumerate() {
                if k == (i + n - 1) % n || k == i || k == (i + 1) % n {
                    continue;
                }
                if vk == prev || vk == curr || vk == next {
                    continue;
                }
                if point_in_tri_2d(verts[vk as usize], a, b, c) {
                    contains = true;
                    break;
                }
            }
            if !contains {
                tris.push([prev, curr, next]);
                rem.remove(i);
                clipped = true;
                break;
            }
        }
        if !clipped {
            return None;
        }
    }

    if rem.len() == 3 {
        let (p, c, n) = (rem[0], rem[1], rem[2]);
        if p != c && c != n && p != n {
            let a = verts[p as usize];
            let b = verts[c as usize];
            let d = verts[n as usize];
            let area2 = (b[0] - a[0]) * (d[1] - a[1]) - (b[1] - a[1]) * (d[0] - a[0]);
            if area2.abs() > 1e-10 {
                tris.push([p, c, n]);
            }
        }
    }
    Some(tris)
}

fn point_in_tri_2d(p: [f32; 2], a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> bool {
    let cross = |u: [f32; 2], v: [f32; 2], w: [f32; 2]| {
        (v[0] - u[0]) * (w[1] - u[1]) - (v[1] - u[1]) * (w[0] - u[0])
    };
    let c1 = cross(a, b, p);
    let c2 = cross(b, c, p);
    let c3 = cross(c, a, p);
    let has_neg = c1 < -1e-7 || c2 < -1e-7 || c3 < -1e-7;
    let has_pos = c1 > 1e-7 || c2 > 1e-7 || c3 > 1e-7;
    !(has_neg && has_pos)
}

/// Simple stroke-block fallback glyph generator (each bar is a separate watertight rectangle)
/// used when no font bytes are provided or a glyph outline is unsupported.
fn fallback_char_polys(ch: char, x_off: f32) -> Vec<Poly2D> {
    let rects: &[[f32; 4]] = match ch.to_ascii_uppercase() {
        'A' => &[
            [0.0, 0.0, 0.14, 0.72],
            [0.42, 0.0, 0.56, 0.72],
            [0.0, 0.58, 0.56, 0.72],
            [0.0, 0.28, 0.56, 0.42],
        ],
        'B' | '8' => &[
            [0.0, 0.0, 0.14, 0.72],
            [0.42, 0.0, 0.56, 0.72],
            [0.0, 0.58, 0.56, 0.72],
            [0.0, 0.29, 0.56, 0.43],
            [0.0, 0.0, 0.56, 0.14],
        ],
        'C' => &[[0.0, 0.0, 0.14, 0.72], [0.0, 0.58, 0.56, 0.72], [0.0, 0.0, 0.56, 0.14]],
        'D' | '0' | 'O' => &[
            [0.0, 0.0, 0.14, 0.72],
            [0.42, 0.0, 0.56, 0.72],
            [0.0, 0.58, 0.56, 0.72],
            [0.0, 0.0, 0.56, 0.14],
        ],
        'E' => &[
            [0.0, 0.0, 0.14, 0.72],
            [0.0, 0.58, 0.56, 0.72],
            [0.0, 0.29, 0.46, 0.43],
            [0.0, 0.0, 0.56, 0.14],
        ],
        'F' => &[
            [0.0, 0.0, 0.14, 0.72],
            [0.0, 0.58, 0.56, 0.72],
            [0.0, 0.29, 0.46, 0.43],
        ],
        'H' => &[
            [0.0, 0.0, 0.14, 0.72],
            [0.42, 0.0, 0.56, 0.72],
            [0.0, 0.29, 0.56, 0.43],
        ],
        'I' | '1' => &[[0.21, 0.0, 0.35, 0.72]],
        'L' => &[[0.0, 0.0, 0.14, 0.72], [0.0, 0.0, 0.56, 0.14]],
        '-' | '_' => &[[0.06, 0.29, 0.50, 0.43]],
        '.' => &[[0.20, 0.0, 0.36, 0.16]],
        _ => &[
            [0.0, 0.0, 0.14, 0.72],
            [0.42, 0.0, 0.56, 0.72],
            [0.0, 0.58, 0.56, 0.72],
            [0.0, 0.0, 0.56, 0.14],
        ],
    };

    rects
        .iter()
        .map(|&[x0, y0, x1, y1]| {
            let verts = vec![
                [x_off + x0, y0],
                [x_off + x1, y0],
                [x_off + x1, y1],
                [x_off + x0, y1],
            ];
            Poly2D {
                loops: vec![verts.clone()],
                verts,
                tris: vec![[0, 1, 2], [0, 2, 3]],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifold::analyze_mesh;
    use crate::model::{MachineProfile, transform_from_translation_euler_deg};

    fn make_box_mesh(dx: f32, dy: f32, dz: f32) -> Arc<TriMesh> {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [dx, 0.0, 0.0],
            [dx, dy, 0.0],
            [0.0, dy, 0.0],
            [0.0, 0.0, dz],
            [dx, 0.0, dz],
            [dx, dy, dz],
            [0.0, dy, dz],
        ];
        let indices = vec![
            [0, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [1, 2, 6],
            [1, 6, 5],
            [2, 3, 7],
            [2, 7, 6],
            [3, 0, 4],
            [3, 4, 7],
        ];
        Arc::new(TriMesh::from_indexed(positions, indices).expect("valid box"))
    }

    #[test]
    fn recommended_window_and_bar_thickness_rules() {
        assert!((recommended_window_size_mm(6.0) - 4.0).abs() < 1e-4);
        assert!(window_exceeds_safe_limit(4.5, 6.0));
        assert!(!window_exceeds_safe_limit(4.0, 6.0));

        assert!((recommended_bar_thickness_mm(3.0) - 0.80).abs() < 1e-4);
        assert!((recommended_bar_thickness_mm(4.0) - 0.80).abs() < 1e-4);
        assert!((recommended_bar_thickness_mm(6.0) - 0.90).abs() < 1e-4);
        assert!((recommended_bar_thickness_mm(8.0) - 1.00).abs() < 1e-4);
        assert!((recommended_bar_thickness_mm(12.0) - 1.00).abs() < 1e-4);
    }

    #[test]
    fn box_sinterbox_is_watertight_manifold_with_and_without_label() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let p = job.add_part("Clip", make_box_mesh(12.0, 8.0, 6.0));
        let i1 = job
            .add_instance(p, transform_from_translation_euler_deg([20.0, 20.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("i1");
        let i2 = job
            .add_instance(p, transform_from_translation_euler_deg([38.0, 20.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("i2");

        let mut cfg = SinterboxConfig {
            shape: CageShape::Box,
            inner_clearance_mm: 2.5,
            window_size_mm: 4.0,
            bar_thickness_mm: 0.8,
            label_enabled: false,
            ..Default::default()
        };
        let (mesh_unlabeled, _) = generate_sinterbox_mesh(&job, &[i1, i2], &cfg, None).expect("unlabeled");
        let rep0 = analyze_mesh(&mesh_unlabeled);
        assert!(rep0.is_watertight_manifold(), "unlabeled manifold: {rep0:?}");

        cfg.label_enabled = true;
        cfg.label_text = "ABOPQR80-CLIP".to_owned();
        let (mesh_fallback, _) = generate_sinterbox_mesh(&job, &[i1, i2], &cfg, None).expect("fallback label");
        let rep1 = analyze_mesh(&mesh_fallback);
        assert!(rep1.is_watertight_manifold(), "fallback labeled manifold: {rep1:?}");

        let ttf = include_bytes!("../../look/fonts/ibm-plex-sans/IBMPlexSans-Variable.ttf");
        cfg.label_text = "ABOPQR80abdegopq-123".to_owned();
        let (mesh_ttf, _) = generate_sinterbox_mesh(&job, &[i1, i2], &cfg, Some(ttf)).expect("ttf label");
        let rep2 = analyze_mesh(&mesh_ttf);
        assert!(rep2.is_watertight_manifold(), "ttf labeled manifold: {rep2:?}");
    }

    #[test]
    fn all_ascii_printable_glyphs_triangulate_cleanly_in_plex_sans() {
        let ttf = include_bytes!("../../look/fonts/ibm-plex-sans/IBMPlexSans-Variable.ttf");
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let p = job.add_part("Wide", make_box_mesh(120.0, 30.0, 20.0));
        let i1 = job.add_instance_centered_on_floor(p).expect("i1");

        let ascii: String = ('!'..='~').collect();
        let cfg = SinterboxConfig {
            shape: CageShape::Box,
            inner_clearance_mm: 2.5,
            window_size_mm: 10.0,
            bar_thickness_mm: 1.0,
            label_enabled: true,
            label_text: ascii,
            ..Default::default()
        };
        let (mesh, _) = generate_sinterbox_mesh(&job, &[i1], &cfg, Some(ttf)).expect("ascii mesh");
        let rep = analyze_mesh(&mesh);
        assert!(rep.is_watertight_manifold(), "ascii ttf manifold: {rep:?}");
    }

    #[test]
    fn tight_fit_sinterbox_carves_l_shape_and_is_watertight_manifold() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let p = job.add_part("Block", make_box_mesh(15.0, 15.0, 15.0));
        let i1 = job
            .add_instance(p, transform_from_translation_euler_deg([0.0, 0.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("i1");
        let i2 = job
            .add_instance(p, transform_from_translation_euler_deg([45.0, 0.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("i2");
        let i3 = job
            .add_instance(p, transform_from_translation_euler_deg([0.0, 45.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("i3");

        let ttf = include_bytes!("../../look/fonts/ibm-plex-sans/IBMPlexSans-Variable.ttf");
        let cfg_box = SinterboxConfig {
            shape: CageShape::Box,
            inner_clearance_mm: 2.5,
            window_size_mm: 10.0,
            bar_thickness_mm: 1.0,
            label_enabled: true,
            label_text: "L-BATCH".to_owned(),
            ..Default::default()
        };
        let cfg_tight = SinterboxConfig {
            shape: CageShape::TightFit,
            ..cfg_box.clone()
        };

        let (mesh_box, _) = generate_sinterbox_mesh(&job, &[i1, i2, i3], &cfg_box, Some(ttf)).expect("box");
        let (mesh_tight, _) = generate_sinterbox_mesh(&job, &[i1, i2, i3], &cfg_tight, Some(ttf)).expect("tight");

        let rep_tight = analyze_mesh(&mesh_tight);
        assert!(rep_tight.is_watertight_manifold(), "tight fit manifold: {rep_tight:?}");
        assert!(
            mesh_tight.volume_mm3 < mesh_box.volume_mm3 * 0.92,
            "expected TightFit volume ({}) < Box volume ({})",
            mesh_tight.volume_mm3,
            mesh_box.volume_mm3
        );
    }

    #[test]
    fn selects_largest_panel_automatically() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        // X-Y flat part -> TopPosZ should win
        let p_z = job.add_part("FlatXY", make_box_mesh(60.0, 40.0, 8.0));
        let iz = job.add_instance_centered_on_floor(p_z).expect("iz");
        let cfg = SinterboxConfig::default();
        let mz = preview_sinterbox_metrics(&job, &[iz], &cfg, None).expect("mz");
        assert_eq!(mz.chosen_panel, LabelPanel::TopPosZ);

        // X-Z tall wall -> FrontNegY should win
        let p_y = job.add_part("TallXZ", make_box_mesh(60.0, 8.0, 50.0));
        let iy = job.add_instance_centered_on_floor(p_y).expect("iy");
        let my = preview_sinterbox_metrics(&job, &[iy], &cfg, None).expect("my");
        assert_eq!(my.chosen_panel, LabelPanel::FrontNegY);

        // Y-Z tall wall -> RightPosX should win
        let p_x = job.add_part("TallYZ", make_box_mesh(8.0, 60.0, 50.0));
        let ix = job.add_instance_centered_on_floor(p_x).expect("ix");
        let mx = preview_sinterbox_metrics(&job, &[ix], &cfg, None).expect("mx");
        assert_eq!(mx.chosen_panel, LabelPanel::RightPosX);
    }

    #[test]
    fn create_sinterbox_groups_parts_lifts_above_floor_and_roundtrips_nestc() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let p = job.add_part("Gear", make_box_mesh(10.0, 10.0, 6.0));
        let i1 = job
            .add_instance(p, transform_from_translation_euler_deg([20.0, 20.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("i1");
        let i2 = job
            .add_instance(p, transform_from_translation_euler_deg([35.0, 20.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("i2");

        let cfg = SinterboxConfig {
            shape: CageShape::Box,
            inner_clearance_mm: 2.5,
            window_size_mm: 4.0,
            bar_thickness_mm: 0.8,
            label_enabled: true,
            label_text: "Gear".to_owned(),
            ..Default::default()
        };

        let res = create_sinterbox(&mut job, &[i1, i2], &cfg, None).expect("created");
        assert!(res.lifted_z_mm > 3.0);
        assert!(job.is_instance_above_floor(res.cage_instance_id));
        assert_eq!(job.groups.len(), 1);
        for &id in &[i1, i2, res.cage_instance_id] {
            let inst = job.instances.iter().find(|i| i.id == id).expect("inst");
            assert_eq!(inst.group_id, Some(res.group_id));
        }

        let cage_inst = job.instances.iter().find(|i| i.id == res.cage_instance_id).expect("cage");
        assert!(cage_inst.transparent);

        let bytes = crate::io::nestc::write_nestc(&job).expect("write nestc");
        let loaded = crate::io::nestc::read_nestc(&bytes).expect("read nestc");
        assert_eq!(loaded.groups.len(), 1);
        assert_eq!(loaded.instances.len(), 3);
    }

    #[test]
    fn rejects_empty_or_invalid_inputs_without_panicking() {
        let job = BuildJob::new(MachineProfile::FormigaP110);
        let cfg = SinterboxConfig::default();
        assert!(analyze_sinterbox_inputs(&job, &[]).is_err());
        assert!(analyze_sinterbox_inputs(&job, &[InstanceId(999)]).is_err());

        let mut job2 = BuildJob::new(MachineProfile::FormigaP110);
        let p = job2.add_part("Cube", make_box_mesh(10.0, 10.0, 10.0));
        let iid = job2.add_instance_centered_on_floor(p).expect("iid");
        let bad = SinterboxConfig {
            window_size_mm: f32::NAN,
            ..cfg
        };
        assert!(generate_sinterbox_mesh(&job2, &[iid], &bad, None).is_err());
    }
}
