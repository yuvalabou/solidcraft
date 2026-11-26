//! Constructive 3D true-shape SLS auto-nester (`Stage 1` multi-order BLBF) with resource
//! guardrails, open-cavity support, topological interlocking rejection, Z-ceiling enforcement,
//! incremental scope controls, and exact `MeshBvh` / `collision.rs` verification.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use rayon::ThreadPoolBuilder;
use rayon::prelude::*;

use crate::collision;
use crate::interlock::{InterlockConfig, VoxelAabb, can_part_escape_tray, tight_solid_bounds};
use crate::model::{
    Aabb, BuildJob, InstanceId, PartId, PartInstance, RotationConstraint, Transform3x4, TriMesh,
    aabbs_overlap, point_inside_mesh_x_ray, transform_point,
};
use crate::voxel::{
    BitGrid3D, CpuBudget, EdtScratch, FreeAngleSet, HeightMap2D, MemoryEstimate, MeshBvh,
    PartOrientationMask, PartVoxelConfig, ZOnlyStep, adaptive_part_half_clearance_mm,
    auto_fit_resolution_mm, build_part_orientation_catalog, dilate_clearance_mask,
    estimate_voxel_memory_bytes, generate_candidate_rotations, is_cancelled,
    rotation_from_transform, voxelize_mesh_flood_fill,
};
use crate::{NestCraftError, Result};

/// Quality/speed preset for 3D auto-nesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Nest3dPreset {
    Fast,
    #[default]
    Balanced,
    Refined,
    Custom,
}

/// Search strategy / optimization depth for 3D auto-nesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Nest3dOptimizerMode {
    ConstructiveOnly,
    RuinAndRecreate,
    Genetic,
    #[default]
    HybridGaRuin,
}

/// How part-to-part clearance is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClearanceMode {
    #[default]
    Fixed,
    AdaptiveByPartMass,
}

/// Override policy for per-part rotation constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrientationFreedom {
    #[default]
    RespectPartConstraints,
    ForceFixed,
    ForceZOnly,
    ForceFree,
}

/// Execution phase reported via [`Nest3dProgress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nest3dPhase {
    Preprocessing,
    Constructive,
    Refining,
    Compacting,
    Verifying,
    Done,
}

/// Full configuration for 3D auto-nesting.
#[derive(Debug, Clone, PartialEq)]
pub struct Nest3dConfig {
    pub preset: Nest3dPreset,
    pub selected_only: bool,
    pub keep_in_bounds_fixed: bool,
    pub min_distance_mm: f32,
    pub clearance_mode: ClearanceMode,
    pub adaptive_max_distance_mm: f32,
    pub separate_z_distance: bool,
    pub min_z_distance_mm: f32,
    pub resolution_mm: f32,
    pub wall_margin_mm: f32,
    pub min_z_mm: f32,
    pub max_z_limit_mm: Option<f32>,
    pub prevent_interlocking: bool,
    pub allow_open_cavities: bool,
    pub avoid_no_build_zone: bool,
    pub optimizer_mode: Nest3dOptimizerMode,
    pub orientation_freedom: OrientationFreedom,
    pub free_angle_set: FreeAngleSet,
    pub z_only_step: ZOnlyStep,
    pub use_pca_prealign: bool,
    pub thermal_weight: f32,
    pub target_density_pct: f32,
    pub slice_smoothness_weight: f32,
    pub gravity_settle_after: bool,
    pub allow_micro_rotations: bool,
    pub max_tilt_deg: f32,
    pub time_limit_secs: f32,
    pub cpu_budget: CpuBudget,
    pub max_memory_bytes: Option<usize>,
}

impl Default for Nest3dConfig {
    fn default() -> Self {
        Self {
            preset: Nest3dPreset::Balanced,
            selected_only: false,
            keep_in_bounds_fixed: false,
            min_distance_mm: 2.5,
            clearance_mode: ClearanceMode::Fixed,
            adaptive_max_distance_mm: 5.0,
            separate_z_distance: false,
            min_z_distance_mm: 3.5,
            resolution_mm: 1.0,
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            max_z_limit_mm: None,
            prevent_interlocking: true,
            allow_open_cavities: true,
            avoid_no_build_zone: true,
            optimizer_mode: Nest3dOptimizerMode::HybridGaRuin,
            orientation_freedom: OrientationFreedom::RespectPartConstraints,
            free_angle_set: FreeAngleSet::Octahedral24,
            z_only_step: ZOnlyStep::Deg45,
            use_pca_prealign: true,
            thermal_weight: 0.0,
            target_density_pct: 25.0,
            slice_smoothness_weight: 0.0,
            gravity_settle_after: true,
            allow_micro_rotations: false,
            max_tilt_deg: 10.0,
            time_limit_secs: 20.0,
            cpu_budget: CpuBudget::Balanced,
            max_memory_bytes: None,
        }
    }
}

impl Nest3dConfig {
    /// Initialize configuration seeded with machine margins and clearances from `job`.
    pub fn from_job(job: &BuildJob) -> Self {
        let clearance = job.clearance_mm.max(0.0);
        Self {
            min_distance_mm: clearance,
            adaptive_max_distance_mm: (clearance * 2.0).max(clearance + 1.5),
            min_z_distance_mm: (clearance + 1.0).max(clearance),
            wall_margin_mm: job.wall_margin_mm.max(0.0),
            min_z_mm: job.min_z_mm.max(0.0),
            ..Self::default()
        }
    }

    /// Apply a preset (`Fast`, `Balanced`, `Refined`, `Custom`) while keeping scope/clearance settings.
    pub fn apply_preset(&mut self, preset: Nest3dPreset) {
        self.preset = preset;
        match preset {
            Nest3dPreset::Fast => {
                self.resolution_mm = 2.0;
                self.optimizer_mode = Nest3dOptimizerMode::ConstructiveOnly;
                self.free_angle_set = FreeAngleSet::Octahedral24;
                self.z_only_step = ZOnlyStep::Deg90;
                self.thermal_weight = 0.0;
                self.slice_smoothness_weight = 0.0;
                self.gravity_settle_after = false;
                self.allow_micro_rotations = false;
                self.time_limit_secs = 8.0;
            }
            Nest3dPreset::Balanced => {
                self.resolution_mm = 1.0;
                self.optimizer_mode = Nest3dOptimizerMode::HybridGaRuin;
                self.free_angle_set = FreeAngleSet::Octahedral24;
                self.z_only_step = ZOnlyStep::Deg45;
                self.thermal_weight = 0.0;
                self.slice_smoothness_weight = 0.0;
                self.gravity_settle_after = true;
                self.allow_micro_rotations = false;
                self.time_limit_secs = 20.0;
            }
            Nest3dPreset::Refined => {
                self.resolution_mm = 0.5;
                self.optimizer_mode = Nest3dOptimizerMode::HybridGaRuin;
                self.free_angle_set = FreeAngleSet::Tilted48;
                self.z_only_step = ZOnlyStep::Deg30;
                self.thermal_weight = 0.25;
                self.target_density_pct = 25.0;
                self.slice_smoothness_weight = 0.20;
                self.gravity_settle_after = true;
                self.allow_micro_rotations = true;
                self.max_tilt_deg = 10.0;
                self.time_limit_secs = 45.0;
            }
            Nest3dPreset::Custom => {}
        }
    }

    /// Effective Z ceiling inside the machine envelope, clamped to `[min_z + 1.0, machine_z - wall_margin]`.
    pub fn effective_max_z(&self, job: &BuildJob) -> f32 {
        let [_, _, wz] = job.machine.size_mm();
        let wm = self.wall_margin_mm.max(0.0);
        let min_z = self.min_z_mm.max(0.0);
        let ceiling = (wz - wm).max(min_z + 1.0);
        match self.max_z_limit_mm {
            Some(cap) => cap.clamp(min_z + 1.0, ceiling),
            None => ceiling,
        }
    }

    /// Resolve effective rotation constraint for an instance.
    pub fn effective_constraint(&self, inst_constraint: RotationConstraint) -> RotationConstraint {
        match self.orientation_freedom {
            OrientationFreedom::RespectPartConstraints => inst_constraint,
            OrientationFreedom::ForceFixed => RotationConstraint::Fixed,
            OrientationFreedom::ForceZOnly => RotationConstraint::ZOnly,
            OrientationFreedom::ForceFree => RotationConstraint::Free,
        }
    }

    /// Half-clearance `(r_xy_mm, r_z_mm)` for a prototype with solid volume `volume_mm3`.
    pub fn part_half_clearance_xy_z(&self, volume_mm3: f64) -> (f32, f32) {
        let min_xy = self.min_distance_mm.max(0.0);
        let min_z = if self.separate_z_distance {
            self.min_z_distance_mm.max(0.0)
        } else {
            min_xy
        };
        match self.clearance_mode {
            ClearanceMode::Fixed => (0.5 * min_xy, 0.5 * min_z),
            ClearanceMode::AdaptiveByPartMass => {
                let max_xy = self.adaptive_max_distance_mm.max(min_xy);
                let r_xy = adaptive_part_half_clearance_mm(volume_mm3, min_xy, max_xy);
                let r_z = if self.separate_z_distance {
                    if min_xy > 1e-4 {
                        r_xy * (min_z / min_xy)
                    } else {
                        0.5 * min_z
                    }
                } else {
                    r_xy
                };
                (r_xy, r_z)
            }
        }
    }
}

/// Progress update emitted during 3D auto-nesting.
#[derive(Debug, Clone)]
pub struct Nest3dProgress {
    pub phase: Nest3dPhase,
    pub label: String,
    pub done: usize,
    pub total: usize,
    pub best_max_z_mm: f32,
    pub best_density_pct: f64,
    pub best_placed: usize,
    pub best_overflow: usize,
    pub elapsed_ms: u64,
    pub live_transforms: Option<Vec<(InstanceId, Transform3x4)>>,
}

/// Summary returned by [`nest_3d`] and [`nest_3d_with_callbacks`].
#[derive(Debug, Clone, PartialEq)]
pub struct Nest3dResult {
    pub placed_count: usize,
    pub overflow_count: usize,
    pub fixed_count: usize,
    pub max_z_mm: f32,
    pub packing_density_pct: f64,
    pub interlock_rejected_attempts: usize,
    pub exact_gate_repairs: usize,
    pub stopped_early: bool,
    pub elapsed_ms: u64,
    pub estimated_peak_bytes: usize,
}

/// Return `true` when `inst` lies wholly inside the raw machine envelope
/// (`[bed_min_x..bed_max_x, bed_min_y..bed_max_y, 0.0..machine_z]`, without strict `wall_margin_mm`).
pub fn instance_inside_raw_machine(job: &BuildJob, inst: &PartInstance) -> bool {
    let Some(proto) = job.part(inst.part_id) else {
        return false;
    };
    let wb = inst.world_bounds(proto.mesh.bounds);
    let ([bx0, by0], [bx1, by1]) = job.bed_xy_bounds();
    let [_, _, bz] = job.machine.size_mm();
    const EPS: f32 = 1e-3;
    wb[0][0] >= bx0 - EPS
        && wb[1][0] <= bx1 + EPS
        && wb[0][1] >= by0 - EPS
        && wb[1][1] <= by1 + EPS
        && wb[0][2] >= -EPS
        && wb[1][2] <= bz + EPS
}

struct ScopePartition {
    movable_indices: Vec<usize>,
    fixed_indices: Vec<usize>,
}

fn partition_instances(
    job: &BuildJob,
    cfg: &Nest3dConfig,
    selected: Option<&HashSet<InstanceId>>,
) -> ScopePartition {
    let sel_filter = cfg
        .selected_only
        .then_some(selected)
        .flatten()
        .filter(|s| !s.is_empty());

    let mut movable_indices = Vec::new();
    let mut fixed_indices = Vec::new();
    for (idx, inst) in job.instances.iter().enumerate() {
        if !job.is_instance_visible(inst) {
            continue;
        }
        let selected_ok = sel_filter.is_none_or(|s| s.contains(&inst.id));
        let fixed_by_bounds = cfg.keep_in_bounds_fixed && instance_inside_raw_machine(job, inst);
        let is_movable = !inst.locked && selected_ok && !fixed_by_bounds;
        if is_movable {
            movable_indices.push(idx);
        } else {
            fixed_indices.push(idx);
        }
    }
    ScopePartition {
        movable_indices,
        fixed_indices,
    }
}

fn estimate_geometry_inputs(
    job: &BuildJob,
    cfg: &Nest3dConfig,
    movable_indices: &[usize],
) -> ([f32; 3], [usize; 3], Vec<([f32; 3], usize)>, f32, usize) {
    let pitch = cfg.resolution_mm.clamp(0.5, 4.0);
    let ([bx0, by0], [bx1, by1]) = job.bed_xy_bounds();
    let wm = cfg.wall_margin_mm.max(0.0);
    let min_z = cfg.min_z_mm.max(0.0);
    let max_z = cfg.effective_max_z(job);
    let span = [
        ((bx1 - bx0) - 2.0 * wm).max(pitch),
        ((by1 - by0) - 2.0 * wm).max(pitch),
        (max_z - min_z).max(pitch),
    ];

    let mut max_half_xy = 0.0_f32;
    let mut max_half_z = 0.0_f32;
    let mut unique_boxes = Vec::new();
    let mut seen_parts = HashSet::new();

    for &idx in movable_indices {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let (hx, hz) = cfg.part_half_clearance_xy_z(proto.mesh.volume_mm3);
        max_half_xy = max_half_xy.max(hx);
        max_half_z = max_half_z.max(hz);
        if seen_parts.insert(proto.id) {
            let eff_c = cfg.effective_constraint(inst.constraint);
            let orient_est = match eff_c {
                RotationConstraint::Fixed => 1,
                RotationConstraint::ZOnly => match cfg.z_only_step {
                    ZOnlyStep::Deg90 => 4,
                    ZOnlyStep::Deg45 => 8,
                    ZOnlyStep::Deg30 => 12,
                },
                RotationConstraint::Free => match cfg.free_angle_set {
                    FreeAngleSet::Octahedral24 => 24,
                    FreeAngleSet::Tilted48 => 48,
                    FreeAngleSet::Dense64 => 64,
                },
            };
            unique_boxes.push((proto.mesh.dimensions(), orient_est));
        }
    }

    let pad_xy = if max_half_xy > 0.0 {
        (max_half_xy / pitch + 0.35).ceil() as usize + 2
    } else {
        2
    };
    let pad_z = if max_half_z > 0.0 {
        (max_half_z / pitch + 0.35).ceil() as usize + 2
    } else {
        2
    };
    let tray_dims = [
        ((span[0] / pitch + 1e-4).floor().max(1.0) as usize) + 2 * pad_xy,
        ((span[1] / pitch + 1e-4).floor().max(1.0) as usize) + 2 * pad_xy,
        ((span[2] / pitch + 1e-4).floor().max(1.0) as usize) + 2 * pad_z,
    ];
    let max_clear_mm = (max_half_xy * 2.0).max(max_half_z * 2.0);
    let workers = cfg.cpu_budget.resolve_workers().min(PORTFOLIO_COUNT);
    (span, tray_dims, unique_boxes, max_clear_mm, workers)
}

/// Estimate peak voxel/EDT RAM requirement (bytes) before starting 3D nesting.
pub fn estimate_nest3d_memory(
    job: &BuildJob,
    cfg: &Nest3dConfig,
    selected: Option<&HashSet<InstanceId>>,
) -> MemoryEstimate {
    let scope = partition_instances(job, cfg, selected);
    let (_, tray_dims, unique_boxes, max_clear_mm, workers) =
        estimate_geometry_inputs(job, cfg, &scope.movable_indices);
    estimate_voxel_memory_bytes(
        tray_dims,
        &unique_boxes,
        cfg.resolution_mm,
        max_clear_mm,
        workers,
    )
}

/// Return the finest `resolution_mm >= cfg.resolution_mm` (up to `4.0 mm`) that stays within
/// `max_memory_bytes`.
pub fn auto_fit_nest3d_resolution(
    job: &BuildJob,
    cfg: &Nest3dConfig,
    selected: Option<&HashSet<InstanceId>>,
    max_memory_bytes: usize,
) -> f32 {
    let scope = partition_instances(job, cfg, selected);
    let (span, _, unique_boxes, max_clear_mm, workers) =
        estimate_geometry_inputs(job, cfg, &scope.movable_indices);
    auto_fit_resolution_mm(
        span,
        &unique_boxes,
        cfg.resolution_mm,
        max_clear_mm,
        workers,
        max_memory_bytes,
    )
}

/// Run 3D auto-nesting synchronously using default callbacks.
pub fn nest_3d(
    job: &mut BuildJob,
    cfg: &Nest3dConfig,
    selected: Option<&HashSet<InstanceId>>,
) -> Result<Nest3dResult> {
    nest_3d_with_callbacks(job, cfg, selected, None, None, None)
}

type CatalogKey = (PartId, u8, [u32; 9]);

const PORTFOLIO_COUNT: usize = 6;

#[derive(Clone)]
struct CachedMaskEntry {
    orient: PartOrientationMask,
    tight_solid: VoxelAabb,
}

#[derive(Clone)]
struct FixedObstacle {
    mesh: Arc<TriMesh>,
    bvh: Arc<MeshBvh>,
    transform: Transform3x4,
    world_bounds: Aabb,
    half_xy_z: (f32, f32),
}

#[derive(Clone)]
struct PlacedNeighbor {
    inst_id: InstanceId,
    part_id: PartId,
    catalog_key: CatalogKey,
    orient_idx: usize,
    origin: [usize; 3],
    mesh: Arc<TriMesh>,
    bvh: Arc<MeshBvh>,
    transform: Transform3x4,
    world_bounds: Aabb,
    half_xy_z: (f32, f32),
}

struct PortfolioOutcome {
    transforms: Vec<(usize, Transform3x4)>,
    placed_count: usize,
    overflow_count: usize,
    max_z_mm: f32,
    total_placed_vol_mm3: f64,
    interlock_rejections: usize,
    stopped_early: bool,
}

fn constraint_tag(c: RotationConstraint) -> u8 {
    match c {
        RotationConstraint::Free => 0,
        RotationConstraint::ZOnly => 1,
        RotationConstraint::Fixed => 2,
    }
}

fn rot_bits_key(rot: &[[f32; 3]; 3]) -> [u32; 9] {
    let norm = |v: f32| if v == 0.0 { 0.0_f32.to_bits() } else { v.to_bits() };
    [
        norm(rot[0][0]),
        norm(rot[0][1]),
        norm(rot[0][2]),
        norm(rot[1][0]),
        norm(rot[1][1]),
        norm(rot[1][2]),
        norm(rot[2][0]),
        norm(rot[2][1]),
        norm(rot[2][2]),
    ]
}

fn should_stop(cancel: Option<&AtomicBool>, stop_and_apply: Option<&AtomicBool>) -> bool {
    is_cancelled(cancel) || is_cancelled(stop_and_apply)
}

fn stamp_at_signed(tray: &mut BitGrid3D, mask: &BitGrid3D, ox: isize, oy: isize, oz: isize) {
    let z_start = (-oz).max(0) as usize;
    let z_end = ((tray.nz as isize) - oz).clamp(0, mask.nz as isize) as usize;
    let y_start = (-oy).max(0) as usize;
    let y_end = ((tray.ny as isize) - oy).clamp(0, mask.ny as isize) as usize;
    let x_start = (-ox).max(0) as usize;
    let x_end = ((tray.nx as isize) - ox).clamp(0, mask.nx as isize) as usize;
    if x_start >= x_end || y_start >= y_end || z_start >= z_end {
        return;
    }
    for iz in z_start..z_end {
        let tz = (oz + iz as isize) as usize;
        for iy in y_start..y_end {
            let ty = (oy + iy as isize) as usize;
            for ix in x_start..x_end {
                if mask.get(ix, iy, iz) {
                    tray.set((ox + ix as isize) as usize, ty, tz);
                }
            }
        }
    }
}

fn update_hmap_from_signed(hmap: &mut HeightMap2D, mask: &BitGrid3D, ox: isize, oy: isize, oz: isize) {
    let x_start = (-ox).max(0) as usize;
    let x_end = ((hmap.nx as isize) - ox).clamp(0, mask.nx as isize) as usize;
    let y_start = (-oy).max(0) as usize;
    let y_end = ((hmap.ny as isize) - oy).clamp(0, mask.ny as isize) as usize;
    if x_start >= x_end || y_start >= y_end {
        return;
    }
    for iy in y_start..y_end {
        let ty = (oy + iy as isize) as usize;
        let row = ty * hmap.nx;
        for ix in x_start..x_end {
            let tx = (ox + ix as isize) as usize;
            let mut top = 0_usize;
            for iz in (0..mask.nz).rev() {
                if oz + iz as isize >= 0 && mask.get(ix, iy, iz) {
                    top = (oz + iz as isize + 1) as usize;
                    break;
                }
            }
            let h = u16::try_from(top).unwrap_or(u16::MAX);
            if h > hmap.heights[row + tx] {
                hmap.heights[row + tx] = h;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn pair_violates_clearance(
    mesh_a: &TriMesh,
    bvh_a: &MeshBvh,
    tf_a: &Transform3x4,
    wb_a: Aabb,
    half_a: (f32, f32),
    mesh_b: &TriMesh,
    bvh_b: &MeshBvh,
    tf_b: &Transform3x4,
    wb_b: Aabb,
    half_b: (f32, f32),
) -> bool {
    let req_xy = (half_a.0 + half_b.0).max(0.0);
    let req_z = (half_a.1 + half_b.1).max(0.0);
    let max_req = req_xy.max(req_z);
    let pad_a = [
        [wb_a[0][0] - max_req, wb_a[0][1] - max_req, wb_a[0][2] - max_req],
        [wb_a[1][0] + max_req, wb_a[1][1] + max_req, wb_a[1][2] + max_req],
    ];
    if !aabbs_overlap(pad_a, wb_b) {
        return false;
    }

    if let Some((pa, pb, dist)) =
        MeshBvh::min_distance_between(bvh_a, mesh_a, tf_a, bvh_b, mesh_b, tf_b, max_req + 1e-2)
    {
        if dist <= 1e-4 {
            return true;
        }
        if (req_xy - req_z).abs() <= 1e-4 {
            if dist <= req_xy + 1e-3 {
                return true;
            }
        } else {
            let dx = if req_xy > 1e-4 { (pa[0] - pb[0]) / req_xy } else { 0.0 };
            let dy = if req_xy > 1e-4 { (pa[1] - pb[1]) / req_xy } else { 0.0 };
            let dz = if req_z > 1e-4 { (pa[2] - pb[2]) / req_z } else { 0.0 };
            if (dx * dx + dy * dy + dz * dz) <= 1.0 + 1e-3 {
                return true;
            }
        }
    }

    // Check full containment inside closed neighbor shells when surfaces don't intersect.
    if aabbs_overlap(wb_a, wb_b) {
        if let Some(v0) = mesh_a.positions.first() {
            let wp = transform_point(tf_a, *v0);
            if aabbs_overlap([wp, wp], wb_b) && point_inside_mesh_x_ray(wp, mesh_b, tf_b) {
                return true;
            }
        }
        if let Some(v0) = mesh_b.positions.first() {
            let wp = transform_point(tf_b, *v0);
            if aabbs_overlap([wp, wp], wb_a) && point_inside_mesh_x_ray(wp, mesh_a, tf_a) {
                return true;
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn nbz_violated(
    mesh_a: &TriMesh,
    bvh_a: &MeshBvh,
    tf_a: &Transform3x4,
    wb_a: Aabb,
    nbz_mesh: &TriMesh,
    nbz_bvh: &MeshBvh,
    nbz_tf: &Transform3x4,
    nbz_wb: Aabb,
    nbz_clearance: f32,
) -> bool {
    let pad = nbz_clearance.max(0.0);
    let pad_a = [
        [wb_a[0][0] - pad, wb_a[0][1] - pad, wb_a[0][2] - pad],
        [wb_a[1][0] + pad, wb_a[1][1] + pad, wb_a[1][2] + pad],
    ];
    if !aabbs_overlap(pad_a, nbz_wb) {
        return false;
    }
    if MeshBvh::min_distance_between(bvh_a, mesh_a, tf_a, nbz_bvh, nbz_mesh, nbz_tf, pad + 1e-2)
        .is_some_and(|(_, _, d)| d <= pad + 1e-4)
    {
        return true;
    }
    if aabbs_overlap(wb_a, nbz_wb)
        && let Some(v0) = mesh_a.positions.first()
    {
        let wp = transform_point(tf_a, *v0);
        if aabbs_overlap([wp, wp], nbz_wb) && point_inside_mesh_x_ray(wp, nbz_mesh, nbz_tf) {
            return true;
        }
    }
    false
}

fn aabb_of_rotated_mesh(mesh: &TriMesh, tf: &Transform3x4) -> Aabb {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for ix in 0..2 {
        for iy in 0..2 {
            for iz in 0..2 {
                let p = [mesh.bounds[ix][0], mesh.bounds[iy][1], mesh.bounds[iz][2]];
                let w = transform_point(tf, p);
                for k in 0..3 {
                    min[k] = min[k].min(w[k]);
                    max[k] = max[k].max(w[k]);
                }
            }
        }
    }
    [min, max]
}

fn place_overflow_strip(
    indices: &[usize],
    transforms_out: &mut HashMap<usize, Transform3x4>,
    job: &BuildJob,
    cfg: &Nest3dConfig,
    bed_max_x: f32,
    bed_min_y: f32,
    bed_max_y: f32,
) {
    let min_z = cfg.min_z_mm.max(0.0);
    let gap = cfg.min_distance_mm.max(1.0);
    let mut ox = bed_max_x + 15.0;
    let mut oy = bed_min_y;
    let mut row_w = 0.0_f32;
    let max_row_y = bed_max_y.max(bed_min_y + 100.0);

    for &idx in indices {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let cur_tf = transforms_out.get(&idx).copied().unwrap_or(inst.transform);
        let rot = if cfg.effective_constraint(inst.constraint) == RotationConstraint::Fixed {
            [
                [inst.transform[0][0], inst.transform[0][1], inst.transform[0][2]],
                [inst.transform[1][0], inst.transform[1][1], inst.transform[1][2]],
                [inst.transform[2][0], inst.transform[2][1], inst.transform[2][2]],
            ]
        } else {
            [
                [cur_tf[0][0], cur_tf[0][1], cur_tf[0][2]],
                [cur_tf[1][0], cur_tf[1][1], cur_tf[1][2]],
                [cur_tf[2][0], cur_tf[2][1], cur_tf[2][2]],
            ]
        };
        let zero_tf = [
            [rot[0][0], rot[0][1], rot[0][2], 0.0],
            [rot[1][0], rot[1][1], rot[1][2], 0.0],
            [rot[2][0], rot[2][1], rot[2][2], 0.0],
        ];
        let wb0 = aabb_of_rotated_mesh(&proto.mesh, &zero_tf);
        let dx = (wb0[1][0] - wb0[0][0]).max(0.1);
        let dy = (wb0[1][1] - wb0[0][1]).max(0.1);

        if oy > bed_min_y && oy + dy > max_row_y {
            oy = bed_min_y;
            ox += row_w + gap;
            row_w = 0.0;
        }
        let tf = [
            [rot[0][0], rot[0][1], rot[0][2], ox - wb0[0][0]],
            [rot[1][0], rot[1][1], rot[1][2], oy - wb0[0][1]],
            [rot[2][0], rot[2][1], rot[2][2], min_z - wb0[0][2]],
        ];
        transforms_out.insert(idx, tf);
        oy += dy + gap;
        row_w = row_w.max(dx);
    }
}

/// Run 3D auto-nesting with cooperative cancellation, early stop-and-apply, and live progress updates.
pub fn nest_3d_with_callbacks(
    job: &mut BuildJob,
    cfg: &Nest3dConfig,
    selected: Option<&HashSet<InstanceId>>,
    cancel: Option<&AtomicBool>,
    stop_and_apply: Option<&AtomicBool>,
    progress: Option<&(dyn Fn(Nest3dProgress) + Sync)>,
) -> Result<Nest3dResult> {
    let t0 = Instant::now();
    let scope = partition_instances(job, cfg, selected);
    let fixed_count = scope.fixed_indices.len();

    let emit = |phase: Nest3dPhase,
                label: String,
                done: usize,
                total: usize,
                best_max_z_mm: f32,
                best_density_pct: f64,
                best_placed: usize,
                best_overflow: usize,
                live_transforms: Option<Vec<(InstanceId, Transform3x4)>>| {
        if let Some(cb) = progress {
            cb(Nest3dProgress {
                phase,
                label,
                done,
                total,
                best_max_z_mm,
                best_density_pct,
                best_placed,
                best_overflow,
                elapsed_ms: t0.elapsed().as_millis() as u64,
                live_transforms,
            });
        }
    };

    if scope.movable_indices.is_empty() {
        let max_z = job.max_z_mm();
        let density = job.packing_density_pct();
        emit(
            Nest3dPhase::Done,
            "Nothing movable to nest".to_owned(),
            1,
            1,
            max_z,
            density,
            0,
            0,
            None,
        );
        return Ok(Nest3dResult {
            placed_count: 0,
            overflow_count: 0,
            fixed_count,
            max_z_mm: max_z,
            packing_density_pct: density,
            interlock_rejected_attempts: 0,
            exact_gate_repairs: 0,
            stopped_early: false,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            estimated_peak_bytes: 0,
        });
    }

    let est = estimate_nest3d_memory(job, cfg, selected);
    if let Some(limit) = cfg.max_memory_bytes
        && est.total_bytes > limit
    {
        return Err(NestCraftError::InvalidInput(format!(
            "estimated RAM ({:.1} MB) exceeds limit ({:.1} MB) — increase resolution_mm or limit",
            est.total_bytes as f64 / (1024.0 * 1024.0),
            limit as f64 / (1024.0 * 1024.0)
        )));
    }

    let pitch = cfg.resolution_mm.clamp(0.5, 4.0);
    let ([bx0, by0], [bx1, by1]) = job.bed_xy_bounds();
    let wm = cfg.wall_margin_mm.max(0.0);
    let min_z = cfg.min_z_mm.max(0.0);
    let max_z_limit = cfg.effective_max_z(job);
    let tray_min = [bx0 + wm, by0 + wm, min_z];
    let tray_max = [
        (bx1 - wm).max(tray_min[0] + pitch),
        (by1 - wm).max(tray_min[1] + pitch),
        max_z_limit.max(tray_min[2] + pitch),
    ];
    let usable_nx = ((tray_max[0] - tray_min[0]) / pitch + 1e-4).floor().max(1.0) as usize;
    let usable_ny = ((tray_max[1] - tray_min[1]) / pitch + 1e-4).floor().max(1.0) as usize;
    let usable_nz = ((tray_max[2] - tray_min[2]) / pitch + 1e-4).floor().max(1.0) as usize;

    // Collect part prototypes & half-clearances across movable and fixed instances.
    let mut max_half_xy = 0.0_f32;
    let mut max_half_z = 0.0_f32;
    let mut bvh_map: HashMap<PartId, Arc<MeshBvh>> = HashMap::new();
    let mut catalog_specs: Vec<(CatalogKey, Arc<TriMesh>, [[f32; 3]; 3], RotationConstraint, (f32, f32))> =
        Vec::new();
    let mut seen_catalogs: HashSet<CatalogKey> = HashSet::new();

    for &idx in scope.movable_indices.iter().chain(scope.fixed_indices.iter()) {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let (hx, hz) = cfg.part_half_clearance_xy_z(proto.mesh.volume_mm3);
        max_half_xy = max_half_xy.max(hx);
        max_half_z = max_half_z.max(hz);
        bvh_map
            .entry(proto.id)
            .or_insert_with(|| Arc::new(MeshBvh::build(&proto.mesh)));
    }

    for &idx in &scope.movable_indices {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let eff_c = cfg.effective_constraint(inst.constraint);
        let rot = rotation_from_transform(&inst.transform);
        let rkey = if eff_c == RotationConstraint::Free {
            [0_u32; 9]
        } else {
            rot_bits_key(&rot)
        };
        let ckey = (proto.id, constraint_tag(eff_c), rkey);
        if seen_catalogs.insert(ckey) {
            let halves = cfg.part_half_clearance_xy_z(proto.mesh.volume_mm3);
            catalog_specs.push((ckey, Arc::clone(&proto.mesh), rot, eff_c, halves));
        }
    }

    let max_pad_xy = if max_half_xy > 0.0 {
        (max_half_xy / pitch + 0.35).ceil() as usize + 3
    } else {
        3
    };
    let max_pad_z = if max_half_z > 0.0 {
        (max_half_z / pitch + 0.35).ceil() as usize + 3
    } else {
        3
    };
    let tray_pad = [max_pad_xy, max_pad_xy, max_pad_z];
    let tray_nx = usable_nx + 2 * tray_pad[0];
    let tray_ny = usable_ny + 2 * tray_pad[1];
    let tray_nz = usable_nz + 2 * tray_pad[2];
    let tray_world_origin = [
        tray_min[0] - (tray_pad[0] as f32) * pitch,
        tray_min[1] - (tray_pad[1] as f32) * pitch,
        tray_min[2] - (tray_pad[2] as f32) * pitch,
    ];

    let workers = cfg.cpu_budget.resolve_workers();
    let pool = ThreadPoolBuilder::new().num_threads(workers).build().ok();

    let total_pre = catalog_specs.len().max(1);
    emit(
        Nest3dPhase::Preprocessing,
        format!("Voxelizing 0/{total_pre} parts ({workers} threads)..."),
        0,
        total_pre,
        job.max_z_mm(),
        job.packing_density_pct(),
        0,
        0,
        None,
    );

    let done_pre = AtomicUsize::new(0);
    let build_one_catalog = |(ckey, mesh, rot, eff_c, halves): &(
        CatalogKey,
        Arc<TriMesh>,
        [[f32; 3]; 3],
        RotationConstraint,
        (f32, f32),
    )| -> Result<(CatalogKey, Vec<CachedMaskEntry>)> {
        if is_cancelled(cancel) {
            return Err(NestCraftError::InvalidInput("3D nesting cancelled".to_owned()));
        }
        let candidates = generate_candidate_rotations(
            mesh,
            *rot,
            *eff_c,
            cfg.free_angle_set,
            cfg.z_only_step,
            cfg.use_pca_prealign,
        );
        let voxel_cfg = PartVoxelConfig {
            resolution_mm: pitch,
            half_clearance_xy_mm: halves.0,
            half_clearance_z_mm: halves.1,
            fill_open_cavities: !cfg.allow_open_cavities,
            coarse_prune_pitch_mm: (pitch * 3.0).clamp(2.5, 5.0),
            max_memory_bytes: cfg.max_memory_bytes,
        };
        let raw_catalog = build_part_orientation_catalog(mesh, &candidates, voxel_cfg, cancel)?;
        let entries = raw_catalog
            .into_iter()
            .filter_map(|orient| {
                let tight_solid = tight_solid_bounds(&orient.solid_mask)?;
                Some(CachedMaskEntry {
                    orient,
                    tight_solid,
                })
            })
            .collect::<Vec<_>>();
        let d = done_pre.fetch_add(1, Ordering::Relaxed) + 1;
        emit(
            Nest3dPhase::Preprocessing,
            format!("Voxelizing {d}/{total_pre} parts..."),
            d,
            total_pre,
            0.0,
            0.0,
            0,
            0,
            None,
        );
        Ok((*ckey, entries))
    };

    let catalog_vec: Vec<(CatalogKey, Vec<CachedMaskEntry>)> = if let Some(ref p) = pool {
        p.install(|| catalog_specs.par_iter().map(build_one_catalog).collect::<Result<Vec<_>>>())?
    } else {
        catalog_specs
            .iter()
            .map(build_one_catalog)
            .collect::<Result<Vec<_>>>()?
    };

    if is_cancelled(cancel) {
        return Err(NestCraftError::InvalidInput("3D nesting cancelled".to_owned()));
    }

    let catalogs: HashMap<CatalogKey, Vec<CachedMaskEntry>> = catalog_vec.into_iter().collect();

    // Build reusable base tray stamped with all static obstacles (fixed instances + NoBuildZone).
    let mut base_tray_clearance = BitGrid3D::new(tray_nx, tray_ny, tray_nz, cfg.max_memory_bytes)?;
    let mut base_tray_solid = BitGrid3D::new(tray_nx, tray_ny, tray_nz, cfg.max_memory_bytes)?;
    let mut base_hmap = HeightMap2D::new(tray_nx, tray_ny, cfg.max_memory_bytes)?;
    let mut fixed_obstacles = Vec::with_capacity(scope.fixed_indices.len());
    let mut fixed_voxel_aabbs: Vec<VoxelAabb> = Vec::with_capacity(scope.fixed_indices.len() + 1);

    let mut edt_scratch = EdtScratch::default();
    for &idx in &scope.fixed_indices {
        if is_cancelled(cancel) {
            return Err(NestCraftError::InvalidInput("3D nesting cancelled".to_owned()));
        }
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let bvh = bvh_map
            .get(&proto.id)
            .cloned()
            .unwrap_or_else(|| Arc::new(MeshBvh::build(&proto.mesh)));
        let rot = rotation_from_transform(&inst.transform);
        let (hx, hz) = cfg.part_half_clearance_xy_z(proto.mesh.volume_mm3);
        let rxy_vox = if hx > 0.0 { hx / pitch + 0.35 } else { 0.0 };
        let rz_vox = if hz > 0.0 { hz / pitch + 0.35 } else { 0.0 };
        let pad_f = [rxy_vox.ceil() as usize, rxy_vox.ceil() as usize, rz_vox.ceil() as usize];
        let (solid, origin_mm, _) =
            voxelize_mesh_flood_fill(&proto.mesh, &rot, pitch, pad_f, false, cfg.max_memory_bytes, cancel)?;
        let clear = dilate_clearance_mask(
            &solid,
            rxy_vox,
            rz_vox,
            &mut edt_scratch,
            cfg.max_memory_bytes,
            cancel,
        )?;
        let ox = ((inst.transform[0][3] + origin_mm[0] - tray_world_origin[0]) / pitch).round() as isize;
        let oy = ((inst.transform[1][3] + origin_mm[1] - tray_world_origin[1]) / pitch).round() as isize;
        let oz = ((inst.transform[2][3] + origin_mm[2] - tray_world_origin[2]) / pitch).round() as isize;

        stamp_at_signed(&mut base_tray_solid, &solid, ox, oy, oz);
        stamp_at_signed(&mut base_tray_clearance, &clear, ox, oy, oz);
        update_hmap_from_signed(&mut base_hmap, &clear, ox, oy, oz);

        if let Some(tight) = tight_solid_bounds(&solid) {
            let clipped_lo = [
                (ox + tight[0][0] as isize).clamp(0, tray_nx as isize) as usize,
                (oy + tight[0][1] as isize).clamp(0, tray_ny as isize) as usize,
                (oz + tight[0][2] as isize).clamp(0, tray_nz as isize) as usize,
            ];
            let clipped_hi = [
                (ox + tight[1][0] as isize).clamp(0, tray_nx as isize) as usize,
                (oy + tight[1][1] as isize).clamp(0, tray_ny as isize) as usize,
                (oz + tight[1][2] as isize).clamp(0, tray_nz as isize) as usize,
            ];
            if clipped_lo[0] < clipped_hi[0]
                && clipped_lo[1] < clipped_hi[1]
                && clipped_lo[2] < clipped_hi[2]
            {
                fixed_voxel_aabbs.push([clipped_lo, clipped_hi]);
            }
        }
        // Temporary fixed voxel masks drop right here at end of iteration.
        fixed_obstacles.push(FixedObstacle {
            mesh: Arc::clone(&proto.mesh),
            bvh,
            transform: inst.transform,
            world_bounds: inst.world_bounds(proto.mesh.bounds),
            half_xy_z: (hx, hz),
        });
    }

    let nbz_exact: Option<(Arc<TriMesh>, Arc<MeshBvh>, Transform3x4, Aabb, f32)> =
        if cfg.avoid_no_build_zone
            && let Some(zone) = &job.no_build_zone
            && zone.enabled
        {
            let nbz_tf = zone.transform();
            let nbz_rot = rotation_from_transform(&nbz_tf);
            let nbz_bvh = Arc::new(MeshBvh::build(&zone.mesh));
            let nbz_rad_xy = (zone.clearance_mm.max(0.0) + max_half_xy) / pitch + 0.5;
            let nbz_rad_z = (zone.clearance_mm.max(0.0) + max_half_z) / pitch + 0.5;
            let pad_n = [
                nbz_rad_xy.ceil() as usize,
                nbz_rad_xy.ceil() as usize,
                nbz_rad_z.ceil() as usize,
            ];
            let (solid_n, origin_n, _) = voxelize_mesh_flood_fill(
                &zone.mesh,
                &nbz_rot,
                pitch,
                pad_n,
                true,
                cfg.max_memory_bytes,
                cancel,
            )?;
            let clear_n = dilate_clearance_mask(
                &solid_n,
                nbz_rad_xy,
                nbz_rad_z,
                &mut edt_scratch,
                cfg.max_memory_bytes,
                cancel,
            )?;
            let ox = ((nbz_tf[0][3] + origin_n[0] - tray_world_origin[0]) / pitch).round() as isize;
            let oy = ((nbz_tf[1][3] + origin_n[1] - tray_world_origin[1]) / pitch).round() as isize;
            let oz = ((nbz_tf[2][3] + origin_n[2] - tray_world_origin[2]) / pitch).round() as isize;
            stamp_at_signed(&mut base_tray_solid, &solid_n, ox, oy, oz);
            stamp_at_signed(&mut base_tray_clearance, &clear_n, ox, oy, oz);
            update_hmap_from_signed(&mut base_hmap, &clear_n, ox, oy, oz);
            Some((
                Arc::clone(&zone.mesh),
                nbz_bvh,
                nbz_tf,
                zone.world_bounds(),
                zone.clearance_mm.max(0.0),
            ))
        } else {
            None
        };
    drop(edt_scratch);

    // Precompute ordering keys for the 6 deterministic constructive portfolios.
    struct InstanceOrderKey {
        idx: usize,
        id: InstanceId,
        vol: f64,
        bbox_vol: f32,
        max_dim: f32,
        min_orient_z: f32,
        area: f64,
        catalog_key: CatalogKey,
        halves: (f32, f32),
    }
    let mut base_items = Vec::with_capacity(scope.movable_indices.len());
    for &idx in &scope.movable_indices {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let eff_c = cfg.effective_constraint(inst.constraint);
        let rot = rotation_from_transform(&inst.transform);
        let rkey = if eff_c == RotationConstraint::Free {
            [0_u32; 9]
        } else {
            rot_bits_key(&rot)
        };
        let catalog_key = (proto.id, constraint_tag(eff_c), rkey);
        let dims = proto.mesh.dimensions();
        let bbox_vol = dims[0] * dims[1] * dims[2];
        let max_dim = dims[0].max(dims[1]).max(dims[2]);
        let min_orient_z = catalogs
            .get(&catalog_key)
            .and_then(|cat| cat.first())
            .map_or(dims[2], |e| e.orient.rotated_dims_mm[2]);
        base_items.push(InstanceOrderKey {
            idx,
            id: inst.id,
            vol: proto.mesh.volume_mm3,
            bbox_vol,
            max_dim,
            min_orient_z,
            area: proto.mesh.surface_area_mm2,
            catalog_key,
            halves: cfg.part_half_clearance_xy_z(proto.mesh.volume_mm3),
        });
    }

    let mut orderings: Vec<Vec<usize>> = Vec::with_capacity(PORTFOLIO_COUNT);
    for kind in 0..PORTFOLIO_COUNT {
        let mut perm: Vec<usize> = (0..base_items.len()).collect();
        match kind {
            0 => perm.sort_by(|&a, &b| {
                base_items[b]
                    .vol
                    .total_cmp(&base_items[a].vol)
                    .then_with(|| base_items[a].id.0.cmp(&base_items[b].id.0))
            }),
            1 => perm.sort_by(|&a, &b| {
                base_items[b]
                    .bbox_vol
                    .total_cmp(&base_items[a].bbox_vol)
                    .then_with(|| base_items[a].id.0.cmp(&base_items[b].id.0))
            }),
            2 => perm.sort_by(|&a, &b| {
                base_items[b]
                    .max_dim
                    .total_cmp(&base_items[a].max_dim)
                    .then_with(|| base_items[a].id.0.cmp(&base_items[b].id.0))
            }),
            3 => perm.sort_by(|&a, &b| {
                base_items[b]
                    .min_orient_z
                    .total_cmp(&base_items[a].min_orient_z)
                    .then_with(|| base_items[a].id.0.cmp(&base_items[b].id.0))
            }),
            4 => perm.sort_by(|&a, &b| {
                base_items[b]
                    .area
                    .total_cmp(&base_items[a].area)
                    .then_with(|| base_items[a].id.0.cmp(&base_items[b].id.0))
            }),
            _ => {
                // Alternating largest/smallest interleaving helps small parts fill gaps inside big ones.
                perm.sort_by(|&a, &b| {
                    base_items[b]
                        .vol
                        .total_cmp(&base_items[a].vol)
                        .then_with(|| base_items[a].id.0.cmp(&base_items[b].id.0))
                });
                let mut interleaved = Vec::with_capacity(perm.len());
                let mut lo = 0_usize;
                let mut hi = perm.len();
                let mut take_front = true;
                while lo < hi {
                    if take_front {
                        interleaved.push(perm[lo]);
                        lo += 1;
                    } else {
                        hi -= 1;
                        interleaved.push(perm[hi]);
                    }
                    take_front = !take_front;
                }
                perm = interleaved;
            }
        }
        orderings.push(perm);
    }

    emit(
        Nest3dPhase::Constructive,
        format!("Running {PORTFOLIO_COUNT} 3D BLBF portfolios..."),
        0,
        PORTFOLIO_COUNT,
        job.max_z_mm(),
        job.packing_density_pct(),
        0,
        base_items.len(),
        None,
    );

    let bed_vol_xy = f64::from((bx1 - bx0).max(1.0)) * f64::from((by1 - by0).max(1.0));
    let fixed_vol_mm3: f64 = scope
        .fixed_indices
        .iter()
        .filter_map(|&idx| job.part(job.instances[idx].part_id).map(|p| p.mesh.volume_mm3))
        .sum();
    let fixed_max_z: f32 = fixed_obstacles
        .iter()
        .map(|f| f.world_bounds[1][2])
        .fold(0.0_f32, f32::max);

    let done_portfolios = AtomicUsize::new(0);
    let interlock_cfg = InterlockConfig::default();

    let run_portfolio = |perm: &Vec<usize>| -> Result<PortfolioOutcome> {
        let mut tray_clearance = base_tray_clearance.clone();
        let mut tray_solid = base_tray_solid.clone();
        let mut hmap = base_hmap.clone();
        let mut placed: Vec<PlacedNeighbor> = Vec::with_capacity(perm.len());
        let mut placed_voxel_aabbs: Vec<VoxelAabb> = Vec::with_capacity(fixed_voxel_aabbs.len() + perm.len());
        placed_voxel_aabbs.extend_from_slice(&fixed_voxel_aabbs);
        let mut placed_xy_centers: Vec<[isize; 2]> = Vec::with_capacity(fixed_obstacles.len() + perm.len());
        for va in &fixed_voxel_aabbs {
            placed_xy_centers.push([
                ((va[0][0] + va[1][0]) / 2) as isize,
                ((va[0][1] + va[1][1]) / 2) as isize,
            ]);
        }

        let mut transforms_out: Vec<(usize, Transform3x4)> = Vec::with_capacity(perm.len());
        let mut overflow_indices: Vec<usize> = Vec::new();
        let mut max_z_mm = fixed_max_z;
        let mut total_placed_vol_mm3 = fixed_vol_mm3;
        let mut interlock_rejections = 0_usize;
        let mut stopped = false;

        for (step_idx, &item_pos) in perm.iter().enumerate() {
            if should_stop(cancel, stop_and_apply) {
                stopped = true;
                for &rem_pos in &perm[step_idx..] {
                    overflow_indices.push(base_items[rem_pos].idx);
                }
                break;
            }
            let item = &base_items[item_pos];
            let inst = &job.instances[item.idx];
            let Some(proto) = job.part(inst.part_id) else {
                continue;
            };
            let bvh = bvh_map
                .get(&proto.id)
                .cloned()
                .unwrap_or_else(|| Arc::new(MeshBvh::build(&proto.mesh)));
            let Some(catalog) = catalogs.get(&item.catalog_key) else {
                overflow_indices.push(item.idx);
                continue;
            };

            struct BestCand {
                score: f32,
                z_top_vox: usize,
                orient_idx: usize,
                origin: [usize; 3],
                tf: Transform3x4,
                wb: Aabb,
                solid_aabb: VoxelAabb,
            }

            let mut best: Option<BestCand> = None;
            let eff_fixed_rot = cfg.effective_constraint(inst.constraint) == RotationConstraint::Fixed;

            for (orient_idx, entry) in catalog.iter().enumerate() {
                if should_stop(cancel, stop_and_apply) {
                    stopped = true;
                    break;
                }
                let orient = &entry.orient;
                let s_lo = entry.tight_solid[0];
                let s_hi = entry.tight_solid[1];
                let solid_dx = s_hi[0].saturating_sub(s_lo[0]);
                let solid_dy = s_hi[1].saturating_sub(s_lo[1]);
                let solid_dz = s_hi[2].saturating_sub(s_lo[2]);
                if solid_dx > usable_nx || solid_dy > usable_ny || solid_dz > usable_nz {
                    continue;
                }

                let ox_min = (tray_pad[0] as isize - s_lo[0] as isize).max(0) as usize;
                let oy_min = (tray_pad[1] as isize - s_lo[1] as isize).max(0) as usize;
                let oz_min = (tray_pad[2] as isize - s_lo[2] as isize).max(0) as usize;
                let ox_max = (tray_pad[0] + usable_nx)
                    .saturating_sub(s_hi[0])
                    .min(tray_nx.saturating_sub(orient.clearance_mask.nx));
                let oy_max = (tray_pad[1] + usable_ny)
                    .saturating_sub(s_hi[1])
                    .min(tray_ny.saturating_sub(orient.clearance_mask.ny));
                let oz_max = (tray_pad[2] + usable_nz)
                    .saturating_sub(s_hi[2])
                    .min(tray_nz.saturating_sub(orient.clearance_mask.nz));
                if ox_min > ox_max || oy_min > oy_max || oz_min > oz_max {
                    continue;
                }

                let max_allowed_z_vox = best
                    .as_ref()
                    .map_or(oz_max, |b| oz_max.min((b.z_top_vox + 1).saturating_sub(s_hi[2])));
                if oz_min > max_allowed_z_vox {
                    continue;
                }

                let rot = if eff_fixed_rot {
                    [
                        [inst.transform[0][0], inst.transform[0][1], inst.transform[0][2]],
                        [inst.transform[1][0], inst.transform[1][1], inst.transform[1][2]],
                        [inst.transform[2][0], inst.transform[2][1], inst.transform[2][2]],
                    ]
                } else {
                    orient.rotation
                };
                let zero_tf = [
                    [rot[0][0], rot[0][1], rot[0][2], 0.0],
                    [rot[1][0], rot[1][1], rot[1][2], 0.0],
                    [rot[2][0], rot[2][1], rot[2][2], 0.0],
                ];
                let local_rot_wb = aabb_of_rotated_mesh(&proto.mesh, &zero_tf);

                let area = (ox_max - ox_min + 1).saturating_mul(oy_max - oy_min + 1);
                let stride = if area > 45_000 {
                    3
                } else if area > 12_000 {
                    2
                } else {
                    1
                };

                let mut orient_found_oz: Option<usize> = None;

                let mut try_candidate = |ox: usize, oy: usize, oz: usize, best_slot: &mut Option<BestCand>| {
                    let tx = tray_world_origin[0] + (ox as f32) * pitch - orient.grid_origin_mm[0];
                    let ty = tray_world_origin[1] + (oy as f32) * pitch - orient.grid_origin_mm[1];
                    let mut tz = tray_world_origin[2] + (oz as f32) * pitch - orient.grid_origin_mm[2];

                    const BEPS: f32 = 1e-3;
                    if tx + local_rot_wb[0][0] < tray_min[0] - BEPS
                        || tx + local_rot_wb[1][0] > tray_max[0] + BEPS
                        || ty + local_rot_wb[0][1] < tray_min[1] - BEPS
                        || ty + local_rot_wb[1][1] > tray_max[1] + BEPS
                    {
                        return false;
                    }
                    let min_wz = tz + local_rot_wb[0][2];
                    if min_wz < tray_min[2] {
                        tz += tray_min[2] - min_wz;
                    }
                    if tz + local_rot_wb[1][2] > tray_max[2] + BEPS {
                        return false;
                    }

                    let wb = [
                        [
                            tx + local_rot_wb[0][0],
                            ty + local_rot_wb[0][1],
                            tz + local_rot_wb[0][2],
                        ],
                        [
                            tx + local_rot_wb[1][0],
                            ty + local_rot_wb[1][1],
                            tz + local_rot_wb[1][2],
                        ],
                    ];
                    let tf = [
                        [rot[0][0], rot[0][1], rot[0][2], tx],
                        [rot[1][0], rot[1][1], rot[1][2], ty],
                        [rot[2][0], rot[2][1], rot[2][2], tz],
                    ];

                    let z_top = oz + s_hi[2];
                    let contact = tray_clearance.contact_score_at(&orient.clearance_mask, ox, oy, oz) as f32;
                    let score = 100.0 * (z_top as f32)
                        + 15.0 * (oz as f32)
                        - 0.25 * contact
                        + 0.02 * ((ox + oy) as f32);

                    if best_slot.as_ref().is_some_and(|b| score >= b.score) {
                        return false;
                    }

                    // Voxel clearance check (`hmap` shortcut skips empty upper air in O(footprint)).
                    let above_hmap = oz >= hmap.max_height_in_rect(
                        ox,
                        oy,
                        orient.clearance_mask.nx,
                        orient.clearance_mask.ny,
                    );
                    if !above_hmap && tray_clearance.overlaps_at(&orient.clearance_mask, ox, oy, oz) {
                        return false;
                    }

                    // Exact NoBuildZone & BVH clearance check before expensive interlocking BFS.
                    if let Some((ref nbz_mesh, ref nbz_bvh, ref nbz_tf, nbz_wb, nbz_cl)) = nbz_exact
                        && nbz_violated(&proto.mesh, &bvh, &tf, wb, nbz_mesh, nbz_bvh, nbz_tf, nbz_wb, nbz_cl)
                    {
                        return false;
                    }
                    for fx in &fixed_obstacles {
                        if pair_violates_clearance(
                            &proto.mesh,
                            &bvh,
                            &tf,
                            wb,
                            item.halves,
                            &fx.mesh,
                            &fx.bvh,
                            &fx.transform,
                            fx.world_bounds,
                            fx.half_xy_z,
                        ) {
                            return false;
                        }
                    }
                    for pl in &placed {
                        if pair_violates_clearance(
                            &proto.mesh,
                            &bvh,
                            &tf,
                            wb,
                            item.halves,
                            &pl.mesh,
                            &pl.bvh,
                            &pl.transform,
                            pl.world_bounds,
                            pl.half_xy_z,
                        ) {
                            return false;
                        }
                    }

                    if cfg.prevent_interlocking
                        && !can_part_escape_tray(
                            &tray_solid,
                            &orient.solid_mask,
                            [ox, oy, oz],
                            &placed_voxel_aabbs,
                            &interlock_cfg,
                            cancel,
                        )
                        .unwrap_or(false)
                    {
                        interlock_rejections += 1;
                        return false;
                    }

                    let solid_aabb = [
                        [ox + s_lo[0], oy + s_lo[1], oz + s_lo[2]],
                        [ox + s_hi[0], oy + s_hi[1], oz + s_hi[2]],
                    ];
                    *best_slot = Some(BestCand {
                        score,
                        z_top_vox: z_top,
                        orient_idx,
                        origin: [ox, oy, oz],
                        tf,
                        wb,
                        solid_aabb,
                    });
                    true
                };

                for oz in oz_min..=max_allowed_z_vox {
                    if should_stop(cancel, stop_and_apply) {
                        stopped = true;
                        break;
                    }
                    if let Some(found_oz) = orient_found_oz
                        && oz > found_oz + 1
                    {
                        break;
                    }
                    let mut layer_hit = false;

                    // Probe centers of placed parts / cavities at 1-voxel precision when open cavities are allowed.
                    if cfg.allow_open_cavities && stride > 1 {
                        let half_w = (s_lo[0] + s_hi[0]) / 2;
                        let half_d = (s_lo[1] + s_hi[1]) / 2;
                        for &[cx, cy] in &placed_xy_centers {
                            let base_x = (cx - half_w as isize).clamp(ox_min as isize, ox_max as isize) as usize;
                            let base_y = (cy - half_d as isize).clamp(oy_min as isize, oy_max as isize) as usize;
                            let dx0 = base_x.saturating_sub(2).max(ox_min);
                            let dx1 = (base_x + 2).min(ox_max);
                            let dy0 = base_y.saturating_sub(2).max(oy_min);
                            let dy1 = (base_y + 2).min(oy_max);
                            for py in dy0..=dy1 {
                                for px in dx0..=dx1 {
                                    if try_candidate(px, py, oz, &mut best) {
                                        layer_hit = true;
                                    }
                                }
                            }
                        }
                    }

                    let mut oy = oy_min;
                    while oy <= oy_max {
                        let mut ox = ox_min;
                        while ox <= ox_max {
                            if try_candidate(ox, oy, oz, &mut best) {
                                layer_hit = true;
                                if stride > 1 {
                                    let r_x0 = ox.saturating_sub(stride - 1).max(ox_min);
                                    let r_x1 = (ox + stride - 1).min(ox_max);
                                    let r_y0 = oy.saturating_sub(stride - 1).max(oy_min);
                                    let r_y1 = (oy + stride - 1).min(oy_max);
                                    for ry in r_y0..=r_y1 {
                                        for rx in r_x0..=r_x1 {
                                            if rx != ox || ry != oy {
                                                let _ = try_candidate(rx, ry, oz, &mut best);
                                            }
                                        }
                                    }
                                }
                            }
                            ox += stride;
                        }
                        oy += stride;
                    }

                    if layer_hit && orient_found_oz.is_none() {
                        orient_found_oz = Some(oz);
                    }
                }
                if stopped {
                    break;
                }
            }

            if let Some(winner) = best {
                let entry = &catalog[winner.orient_idx];
                let [ox, oy, oz] = winner.origin;
                tray_clearance.stamp_at(&entry.orient.clearance_mask, ox, oy, oz);
                tray_solid.stamp_at(&entry.orient.solid_mask, ox, oy, oz);
                hmap.update_from_stamp(&entry.orient.clearance_mask, ox, oy, oz);
                placed_voxel_aabbs.push(winner.solid_aabb);
                placed_xy_centers.push([
                    ((winner.solid_aabb[0][0] + winner.solid_aabb[1][0]) / 2) as isize,
                    ((winner.solid_aabb[0][1] + winner.solid_aabb[1][1]) / 2) as isize,
                ]);
                max_z_mm = max_z_mm.max(winner.wb[1][2]);
                total_placed_vol_mm3 += proto.mesh.volume_mm3;
                transforms_out.push((item.idx, winner.tf));
                placed.push(PlacedNeighbor {
                    inst_id: inst.id,
                    part_id: proto.id,
                    catalog_key: item.catalog_key,
                    orient_idx: winner.orient_idx,
                    origin: winner.origin,
                    mesh: Arc::clone(&proto.mesh),
                    bvh,
                    transform: winner.tf,
                    world_bounds: winner.wb,
                    half_xy_z: item.halves,
                });
            } else {
                overflow_indices.push(item.idx);
            }
        }

        let overflow_count = overflow_indices.len();
        let placed_count = placed.len();
        if !overflow_indices.is_empty() {
            let mut tf_map: HashMap<usize, Transform3x4> = transforms_out.iter().copied().collect();
            place_overflow_strip(&overflow_indices, &mut tf_map, job, cfg, bx1, by0, by1);
            for idx in &overflow_indices {
                if let Some(tf) = tf_map.get(idx).copied() {
                    transforms_out.push((*idx, tf));
                }
            }
        }

        let d = done_portfolios.fetch_add(1, Ordering::Relaxed) + 1;
        let density_pct = if max_z_mm > 1e-4 {
            (total_placed_vol_mm3 / (bed_vol_xy * f64::from(max_z_mm))) * 100.0
        } else {
            0.0
        };
        emit(
            Nest3dPhase::Constructive,
            format!("Constructive portfolio {d}/{PORTFOLIO_COUNT}..."),
            d,
            PORTFOLIO_COUNT,
            max_z_mm,
            density_pct,
            placed_count,
            overflow_count,
            None,
        );

        let _ = placed.first().map(|p| (p.part_id, &p.catalog_key, p.orient_idx, p.origin, p.inst_id));

        Ok(PortfolioOutcome {
            transforms: transforms_out,
            placed_count,
            overflow_count,
            max_z_mm,
            total_placed_vol_mm3,
            interlock_rejections,
            stopped_early: stopped,
        })
    };

    let outcomes: Vec<PortfolioOutcome> = if let Some(ref p) = pool {
        p.install(|| orderings.par_iter().map(run_portfolio).collect::<Result<Vec<_>>>())?
    } else {
        orderings
            .iter()
            .map(run_portfolio)
            .collect::<Result<Vec<_>>>()?
    };

    // Release voxel structures immediately (staged memory release before verification/future compaction).
    drop(base_tray_clearance);
    drop(base_tray_solid);
    drop(base_hmap);
    drop(catalogs);

    if is_cancelled(cancel) {
        return Err(NestCraftError::InvalidInput("3D nesting cancelled".to_owned()));
    }

    let best = outcomes
        .into_iter()
        .min_by(|a, b| {
            a.overflow_count
                .cmp(&b.overflow_count)
                .then_with(|| a.max_z_mm.total_cmp(&b.max_z_mm))
                .then_with(|| b.total_placed_vol_mm3.total_cmp(&a.total_placed_vol_mm3))
        })
        .ok_or_else(|| NestCraftError::InvalidInput("no nesting portfolio completed".to_owned()))?;

    let mut applied_tf: HashMap<usize, Transform3x4> = best.transforms.into_iter().collect();
    for (&idx, &tf) in &applied_tf {
        if let Some(inst) = job.instances.get_mut(idx) {
            inst.transform = tf;
        }
    }

    let movable_set: HashSet<usize> = scope.movable_indices.iter().copied().collect();
    let idx_by_inst_id: HashMap<InstanceId, usize> = job
        .instances
        .iter()
        .enumerate()
        .map(|(i, inst)| (inst.id, i))
        .collect();
    let mut gate_repairs: Vec<usize> = Vec::new();
    let mut overflow_id_set: HashSet<InstanceId> = HashSet::new();
    for &idx in &scope.movable_indices {
        if let Some(inst) = job.instances.get(idx)
            && let Some(proto) = job.part(inst.part_id)
        {
            let wb = inst.world_bounds(proto.mesh.bounds);
            if wb[0][0] > bx1 + 5.0 {
                overflow_id_set.insert(inst.id);
            }
        }
    }

    // Stage 3: Physics Gravity Compaction (Rapier3D + V-HACD), excluding overflow strip instances.
    let mut stage3_stopped_early = false;
    if cfg.gravity_settle_after && !is_cancelled(cancel) && !is_cancelled(stop_and_apply) {
        let placed_before = scope.movable_indices.len().saturating_sub(overflow_id_set.len());
        let overflow_before = overflow_id_set.len();
        emit(
            Nest3dPhase::Compacting,
            "Physics gravity compaction (V-HACD)...".to_owned(),
            0,
            1,
            job.max_z_mm(),
            job.packing_density_pct(),
            placed_before,
            overflow_before,
            None,
        );
        let pre_density_s3 = job.packing_density_pct();
        let settle_cfg = crate::compact::GravitySettleConfig::from_nest3d(cfg);
        let settle_cb = |sp: crate::compact::GravitySettleProgress| {
            let approx_density = if sp.max_z_mm > 1e-4 {
                (best.total_placed_vol_mm3 / (bed_vol_xy * f64::from(sp.max_z_mm))) * 100.0
            } else {
                pre_density_s3
            };
            emit(
                Nest3dPhase::Compacting,
                format!("Physics gravity compaction (step {}/{})", sp.step, sp.total_steps),
                sp.step as usize,
                sp.total_steps.max(1) as usize,
                sp.max_z_mm,
                approx_density,
                placed_before,
                overflow_before,
                sp.preview_transforms,
            );
        };
        let s3 = crate::compact::gravity_settle_with_orientation_freedom(
            job,
            &settle_cfg,
            cfg.orientation_freedom,
            selected,
            Some(&overflow_id_set),
            cancel,
            stop_and_apply,
            Some(&settle_cb),
        )?;
        stage3_stopped_early = s3.stopped_early;
        for &idx in &scope.movable_indices {
            if let Some(inst) = job.instances.get(idx) {
                applied_tf.insert(idx, inst.transform);
            }
        }
    }

    // Exact Final Gate: verify clearance via collision.rs / exact BVH and relocate any violating
    // movable non-overflow instance to +X overflow.
    emit(
        Nest3dPhase::Verifying,
        "Running exact final clearance gate...".to_owned(),
        1,
        1,
        job.max_z_mm(),
        job.packing_density_pct(),
        best.placed_count,
        best.overflow_count,
        None,
    );

    if !cfg.separate_z_distance
        && cfg.clearance_mode == ClearanceMode::Fixed
        && (cfg.min_distance_mm - job.clearance_mm).abs() < 1e-3
    {
        let rep = collision::detect_collisions(job, None, 3.0);
        for pair in rep.pairs {
            if overflow_id_set.contains(&pair.a) || overflow_id_set.contains(&pair.b) {
                continue;
            }
            let victim_id = if idx_by_inst_id
                .get(&pair.b)
                .is_some_and(|i| movable_set.contains(i))
            {
                Some(pair.b)
            } else if idx_by_inst_id
                .get(&pair.a)
                .is_some_and(|i| movable_set.contains(i))
            {
                Some(pair.a)
            } else {
                None
            };
            if let Some(vid) = victim_id
                && let Some(&vidx) = idx_by_inst_id.get(&vid)
                && !gate_repairs.contains(&vidx)
            {
                gate_repairs.push(vidx);
                overflow_id_set.insert(vid);
            }
        }
    }

    // Also run exact pair clearance + NBZ check with configured rules (covers anisotropic & adaptive).
    let non_overflow_movable: Vec<usize> = scope
        .movable_indices
        .iter()
        .copied()
        .filter(|i| {
            job.instances
                .get(*i)
                .is_some_and(|inst| !overflow_id_set.contains(&inst.id))
        })
        .collect();

    for (pos_a, &ia) in non_overflow_movable.iter().enumerate() {
        let inst_a = &job.instances[ia];
        let Some(proto_a) = job.part(inst_a.part_id) else {
            continue;
        };
        let bvh_a = bvh_map
            .get(&proto_a.id)
            .cloned()
            .unwrap_or_else(|| Arc::new(MeshBvh::build(&proto_a.mesh)));
        let wb_a = inst_a.world_bounds(proto_a.mesh.bounds);
        let half_a = cfg.part_half_clearance_xy_z(proto_a.mesh.volume_mm3);

        let mut bad = false;
        if let Some((ref nbz_mesh, ref nbz_bvh, ref nbz_tf, nbz_wb, nbz_cl)) = nbz_exact
            && nbz_violated(&proto_a.mesh, &bvh_a, &inst_a.transform, wb_a, nbz_mesh, nbz_bvh, nbz_tf, nbz_wb, nbz_cl)
        {
            bad = true;
        }
        if !bad {
            for fx in &fixed_obstacles {
                if pair_violates_clearance(
                    &proto_a.mesh,
                    &bvh_a,
                    &inst_a.transform,
                    wb_a,
                    half_a,
                    &fx.mesh,
                    &fx.bvh,
                    &fx.transform,
                    fx.world_bounds,
                    fx.half_xy_z,
                ) {
                    bad = true;
                    break;
                }
            }
        }
        if !bad {
            for &ib in &non_overflow_movable[(pos_a + 1)..] {
                let inst_b = &job.instances[ib];
                if overflow_id_set.contains(&inst_b.id) {
                    continue;
                }
                let Some(proto_b) = job.part(inst_b.part_id) else {
                    continue;
                };
                let bvh_b = bvh_map
                    .get(&proto_b.id)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(MeshBvh::build(&proto_b.mesh)));
                let wb_b = inst_b.world_bounds(proto_b.mesh.bounds);
                let half_b = cfg.part_half_clearance_xy_z(proto_b.mesh.volume_mm3);
                if pair_violates_clearance(
                    &proto_a.mesh,
                    &bvh_a,
                    &inst_a.transform,
                    wb_a,
                    half_a,
                    &proto_b.mesh,
                    &bvh_b,
                    &inst_b.transform,
                    wb_b,
                    half_b,
                ) {
                    bad = true;
                    break;
                }
            }
        }
        if bad && !gate_repairs.contains(&ia) {
            gate_repairs.push(ia);
            overflow_id_set.insert(inst_a.id);
        }
    }

    let exact_gate_repairs = gate_repairs.len();
    if !gate_repairs.is_empty() {
        place_overflow_strip(&gate_repairs, &mut applied_tf, job, cfg, bx1, by0, by1);
        for &idx in &gate_repairs {
            if let (Some(inst), Some(tf)) = (job.instances.get_mut(idx), applied_tf.get(&idx).copied()) {
                inst.transform = tf;
            }
        }
    }

    let final_overflow = overflow_id_set.len();
    let final_placed = scope.movable_indices.len().saturating_sub(final_overflow);
    let final_max_z = job.max_z_mm();
    let final_density = job.packing_density_pct();
    let stopped_early = best.stopped_early || stage3_stopped_early || is_cancelled(stop_and_apply);

    emit(
        Nest3dPhase::Done,
        "3D nesting complete".to_owned(),
        1,
        1,
        final_max_z,
        final_density,
        final_placed,
        final_overflow,
        None,
    );

    Ok(Nest3dResult {
        placed_count: final_placed,
        overflow_count: final_overflow,
        fixed_count,
        max_z_mm: final_max_z,
        packing_density_pct: final_density,
        interlock_rejected_attempts: best.interlock_rejections,
        exact_gate_repairs,
        stopped_early,
        elapsed_ms: t0.elapsed().as_millis() as u64,
        estimated_peak_bytes: est.total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IDENTITY_TRANSFORM, MachineProfile, transform_from_translation_euler_deg};

    fn box_mesh(min: [f32; 3], max: [f32; 3]) -> Arc<TriMesh> {
        let positions = vec![
            [min[0], min[1], min[2]],
            [max[0], min[1], min[2]],
            [max[0], max[1], min[2]],
            [min[0], max[1], min[2]],
            [min[0], min[1], max[2]],
            [max[0], min[1], max[2]],
            [max[0], max[1], max[2]],
            [min[0], max[1], max[2]],
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

    fn open_cup_mesh(outer: [f32; 3], wall: f32) -> Arc<TriMesh> {
        let [ox, oy, oz] = outer;
        let mut pos = vec![
            [0.0, 0.0, 0.0],
            [ox, 0.0, 0.0],
            [ox, oy, 0.0],
            [0.0, oy, 0.0],
            [0.0, 0.0, oz],
            [ox, 0.0, oz],
            [ox, oy, oz],
            [0.0, oy, oz],
            [wall, wall, wall],
            [ox - wall, wall, wall],
            [ox - wall, oy - wall, wall],
            [wall, oy - wall, wall],
            [wall, wall, oz],
            [ox - wall, wall, oz],
            [ox - wall, oy - wall, oz],
            [wall, oy - wall, oz],
        ];
        let idx = vec![
            [0, 2, 1],
            [0, 3, 2],
            [0, 1, 5],
            [0, 5, 4],
            [1, 2, 6],
            [1, 6, 5],
            [2, 3, 7],
            [2, 7, 6],
            [3, 0, 4],
            [3, 4, 7],
            [8, 9, 10],
            [8, 10, 11],
            [8, 13, 9],
            [8, 12, 13],
            [9, 14, 10],
            [9, 13, 14],
            [10, 15, 11],
            [10, 14, 15],
            [11, 12, 8],
            [11, 15, 12],
            [4, 5, 13],
            [4, 13, 12],
            [5, 6, 14],
            [5, 14, 13],
            [6, 7, 15],
            [6, 15, 14],
            [7, 4, 12],
            [7, 12, 15],
        ];
        let _ = pos.len();
        TriMesh::from_indexed(std::mem::take(&mut pos), idx)
            .map(Arc::new)
            .expect("valid cup")
    }

    fn torus_mesh(major_r: f32, minor_r: f32, segs: usize, sides: usize) -> Arc<TriMesh> {
        let mut pos = Vec::with_capacity(segs * sides);
        let mut idx = Vec::with_capacity(segs * sides * 2);
        for i in 0..segs {
            let u = 2.0 * std::f32::consts::PI * (i as f32) / (segs as f32);
            let (su, cu) = u.sin_cos();
            for j in 0..sides {
                let v = 2.0 * std::f32::consts::PI * (j as f32) / (sides as f32);
                let (sv, cv) = v.sin_cos();
                let r = major_r + minor_r * cv;
                pos.push([r * cu, r * su, minor_r * sv]);
            }
        }
        for i in 0..segs {
            let i1 = (i + 1) % segs;
            for j in 0..sides {
                let j1 = (j + 1) % sides;
                let a = (i * sides + j) as u32;
                let b = (i1 * sides + j) as u32;
                let c = (i1 * sides + j1) as u32;
                let d = (i * sides + j1) as u32;
                idx.push([a, b, c]);
                idx.push([a, c, d]);
            }
        }
        Arc::new(TriMesh::from_indexed(pos, idx).expect("valid torus"))
    }

    #[test]
    fn constructive_3d_packs_multiple_parts_collision_free() {
        let mut job = BuildJob::new(MachineProfile::custom("TestChamber", [100.0, 100.0, 100.0]));
        let p_a = job.add_part("Block", box_mesh([0.0, 0.0, 0.0], [22.0, 18.0, 14.0]));
        let p_b = job.add_part("Tall", box_mesh([0.0, 0.0, 0.0], [12.0, 12.0, 32.0]));
        for _ in 0..4 {
            let _ = job.add_instance(p_a, IDENTITY_TRANSFORM).expect("a");
            let _ = job.add_instance(p_b, IDENTITY_TRANSFORM).expect("b");
        }

        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.optimizer_mode = Nest3dOptimizerMode::ConstructiveOnly;
        let res = nest_3d(&mut job, &cfg, None).expect("nest3d ok");

        assert_eq!(res.placed_count, 8);
        assert_eq!(res.overflow_count, 0);
        for inst in &job.instances {
            assert!(job.is_instance_in_bounds(inst.id), "inst {} out of bounds", inst.id.0);
        }
        let col = collision::detect_collisions(&job, None, 2.0);
        assert!(col.pairs.is_empty(), "expected 0 exact collisions, got {:?}", col.pairs);
    }

    #[test]
    fn selected_only_and_keep_in_bounds_fixed_never_move_fixed_parts() {
        let mut job = BuildJob::new(MachineProfile::custom("ScopeBox", [120.0, 120.0, 120.0]));
        let cube = box_mesh([0.0, 0.0, 0.0], [16.0, 16.0, 16.0]);
        let pid = job.add_part("Cube", cube);

        // Near-wall part inside raw [0..120] envelope even though x=1.0 < wall_margin=5.0.
        let near_wall_tf = transform_from_translation_euler_deg([1.0, 10.0, 0.0], [0.0, 0.0, 0.0]);
        let fixed_in_bounds = job.add_instance(pid, near_wall_tf).expect("near wall");
        // Explicitly locked part in center.
        let locked_tf = transform_from_translation_euler_deg([50.0, 50.0, 0.0], [0.0, 0.0, 0.0]);
        let locked_id = job.add_instance(pid, locked_tf).expect("locked");
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == locked_id) {
            inst.locked = true;
        }
        // Hidden part outside chamber — must stay completely ignored & untouched.
        let hidden_tf = transform_from_translation_euler_deg([50.0, 50.0, 0.0], [0.0, 0.0, 0.0]);
        let hidden_id = job.add_instance(pid, hidden_tf).expect("hidden");
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == hidden_id) {
            inst.hidden = true;
        }
        // Movable part outside bed.
        let outside_tf = transform_from_translation_euler_deg([180.0, 20.0, 0.0], [0.0, 0.0, 0.0]);
        let movable_id = job.add_instance(pid, outside_tf).expect("outside");

        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.keep_in_bounds_fixed = true;
        cfg.selected_only = true;
        let sel: HashSet<InstanceId> = [movable_id, fixed_in_bounds].into_iter().collect();

        let res = nest_3d(&mut job, &cfg, Some(&sel)).expect("nest3d");
        assert_eq!(res.placed_count, 1);
        assert_eq!(res.fixed_count, 2);

        let get_tf = |id: InstanceId| job.instances.iter().find(|i| i.id == id).expect("inst").transform;
        assert_eq!(get_tf(fixed_in_bounds), near_wall_tf);
        assert_eq!(get_tf(locked_id), locked_tf);
        assert_eq!(get_tf(hidden_id), hidden_tf);
        assert!(job.is_instance_in_bounds(movable_id));
        let col = collision::detect_collisions(&job, None, 2.0);
        assert!(col.pairs.is_empty());
    }

    #[test]
    fn max_z_limit_enforced_and_overflow_routed_to_plus_x() {
        let mut job = BuildJob::new(MachineProfile::custom("SmallChamber", [70.0, 70.0, 120.0]));
        let pid = job.add_part("TallBlock", box_mesh([0.0, 0.0, 0.0], [30.0, 30.0, 24.0]));
        for _ in 0..4 {
            let _ = job.add_instance(pid, IDENTITY_TRANSFORM).expect("inst");
        }
        // Usable chamber is 60x60x30 (30x30x24 block rotates to 24mm side so multiple fit in XY,
        // while stacking is impossible under max_z = 30.0 and 4th block overflows to +X).
        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.max_z_limit_mm = Some(30.0);
        let res = nest_3d(&mut job, &cfg, None).expect("nest3d");
        assert!(res.placed_count >= 1);
        assert!(res.overflow_count >= 1);
        assert_eq!(res.placed_count + res.overflow_count, 4);

        let ([_, _], [bx1, _]) = job.bed_xy_bounds();
        for inst in &job.instances {
            let wb = inst.world_bounds(job.part(inst.part_id).expect("p").mesh.bounds);
            if wb[0][0] <= bx1 + 1.0 {
                assert!(wb[1][2] <= 30.0 + 1e-3, "in-bed part exceeded max_z=30: {}", wb[1][2]);
            } else {
                assert!(wb[0][0] > bx1 + 5.0, "overflow part must go to +X strip");
            }
        }
    }

    #[test]
    fn open_cavity_allowed_and_interlocking_rejected() {
        // Open cup (locked at bed floor) + small cube should nest inside the open pocket!
        let mut cup_job = BuildJob::new(MachineProfile::custom("CupTray", [60.0, 60.0, 50.0]));
        let cup_pid = cup_job.add_part("Cup", open_cup_mesh([44.0, 44.0, 32.0], 5.0));
        let cup_id = cup_job
            .add_instance(cup_pid, transform_from_translation_euler_deg([8.0, 8.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("cup");
        if let Some(inst) = cup_job.instances.iter_mut().find(|i| i.id == cup_id) {
            inst.locked = true;
            inst.constraint = RotationConstraint::Fixed;
        }
        let peg_pid = cup_job.add_part("Peg", box_mesh([0.0, 0.0, 0.0], [14.0, 14.0, 14.0]));
        let peg_id = cup_job.add_instance(peg_pid, IDENTITY_TRANSFORM).expect("peg");

        let mut cfg = Nest3dConfig::from_job(&cup_job);
        cfg.resolution_mm = 2.0;
        cfg.allow_open_cavities = true;
        cfg.prevent_interlocking = true;
        let res = nest_3d(&mut cup_job, &cfg, None).expect("cup nest");
        assert_eq!(res.placed_count, 1);
        let peg_wb = cup_job.instances[1].world_bounds(cup_job.part(peg_pid).expect("p").mesh.bounds);
        assert!(peg_wb[1][2] <= 32.0 + 1e-3, "peg should fit inside open cup cavity: {peg_wb:?}");

        // Torus interlocked setup check: placing a vertical torus passing through an already-locked horizontal torus's hole must be rejected/separated!
        let mut ring_job = BuildJob::new(MachineProfile::custom("RingTray", [80.0, 80.0, 80.0]));
        let t_pid = ring_job.add_part("Torus", torus_mesh(16.0, 4.0, 24, 10));
        let fixed_ring = ring_job
            .add_instance(t_pid, transform_from_translation_euler_deg([40.0, 40.0, 8.0], [0.0, 0.0, 0.0]))
            .expect("ring1");
        if let Some(inst) = ring_job.instances.iter_mut().find(|i| i.id == fixed_ring) {
            inst.locked = true;
        }
        let _ring2 = ring_job.add_instance(t_pid, IDENTITY_TRANSFORM).expect("ring2");
        let mut ring_cfg = Nest3dConfig::from_job(&ring_job);
        ring_cfg.resolution_mm = 2.0;
        ring_cfg.prevent_interlocking = true;
        let ring_res = nest_3d(&mut ring_job, &ring_cfg, None).expect("ring nest");
        assert_eq!(ring_res.placed_count, 1);
        let irep = crate::interlock::detect_interlocking(&ring_job, 2.0, &InterlockConfig::default(), None, None)
            .expect("interlock check");
        assert!(irep.trapped_ids.is_empty(), "rings must not interlock: {:?}", irep.trapped_ids);
        let _ = peg_id;
    }

    #[test]
    fn hostile_inputs_memory_guard_and_cancellation() {
        // 1. Empty job returns Ok cleanly.
        let mut empty = BuildJob::new(MachineProfile::FormigaP110);
        let empty_cfg = Nest3dConfig::from_job(&empty);
        let empty_res = nest_3d(&mut empty, &empty_cfg, None).expect("empty ok");
        assert_eq!(empty_res.placed_count, 0);

        // 2. Strict memory cap rejects cleanly without panicking.
        let mut job = BuildJob::new(MachineProfile::FarsoonHT403P);
        let pid = job.add_part("Cube", box_mesh([0.0, 0.0, 0.0], [20.0, 20.0, 20.0]));
        let _ = job.add_instance(pid, IDENTITY_TRANSFORM).expect("inst");
        let mut tight_cfg = Nest3dConfig::from_job(&job);
        tight_cfg.resolution_mm = 0.5;
        tight_cfg.max_memory_bytes = Some(1024);
        assert!(nest_3d(&mut job, &tight_cfg, None).is_err());

        // Auto-fit resolution brings memory under target.
        let fitted = auto_fit_nest3d_resolution(&job, &tight_cfg, None, 16 * 1024 * 1024);
        assert!(fitted > 0.5 && fitted <= 4.0);

        // 3. Cooperative cancel aborts cleanly.
        let cancel = AtomicBool::new(true);
        let mut cancel_cfg = Nest3dConfig::from_job(&job);
        cancel_cfg.resolution_mm = 2.0;
        assert!(nest_3d_with_callbacks(&mut job, &cancel_cfg, None, Some(&cancel), None, None).is_err());
    }

    #[test]
    fn anisotropic_z_and_adaptive_part_mass_clearance_verified() {
        let mut job = BuildJob::new(MachineProfile::custom("AnisoTray", [90.0, 90.0, 90.0]));
        let small = job.add_part("Small", box_mesh([0.0, 0.0, 0.0], [10.0, 10.0, 10.0]));
        let large = job.add_part("Large", box_mesh([0.0, 0.0, 0.0], [45.0, 45.0, 45.0]));
        let _ = job.add_instance(large, IDENTITY_TRANSFORM).expect("large");
        let _ = job.add_instance(small, IDENTITY_TRANSFORM).expect("small1");
        let _ = job.add_instance(small, IDENTITY_TRANSFORM).expect("small2");

        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.clearance_mode = ClearanceMode::AdaptiveByPartMass;
        cfg.min_distance_mm = 2.0;
        cfg.adaptive_max_distance_mm = 6.0;
        cfg.separate_z_distance = true;
        cfg.min_z_distance_mm = 4.0;

        let res = nest_3d(&mut job, &cfg, None).expect("aniso nest");
        assert_eq!(res.placed_count, 3);
        assert_eq!(res.overflow_count, 0);
        let bvh_l = MeshBvh::build(&job.part(large).expect("l").mesh);
        let bvh_s = MeshBvh::build(&job.part(small).expect("s").mesh);
        let _ = (bvh_l, bvh_s);
    }

    #[test]
    fn nest_3d_with_stage3_gravity_settle_compacts_cleanly() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.clearance_mm = 2.0;
        let pid = job.add_part("Block", box_mesh([0.0, 0.0, 0.0], [24.0, 24.0, 18.0]));
        for k in 0..4 {
            let _ = job
                .add_instance(
                    pid,
                    transform_from_translation_euler_deg([0.0, 0.0, (k as f32) * 30.0], [0.0, 0.0, 0.0]),
                )
                .expect("inst");
        }
        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.gravity_settle_after = true;
        let res = nest_3d(&mut job, &cfg, None).expect("stage 3 nest");
        assert_eq!(res.placed_count, 4);
        assert_eq!(res.overflow_count, 0);
        assert_eq!(res.exact_gate_repairs, 0);
        let col = collision::detect_collisions(&job, None, 2.0);
        assert!(col.pairs.is_empty(), "Stage 3 must keep valid clearance: {:?}", col.pairs);
    }
}
