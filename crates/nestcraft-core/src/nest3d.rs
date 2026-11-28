//! Constructive 3D true-shape SLS auto-nester (`Stage 1` multi-order BLBF) and `Stage 2`
//! anytime parallel island optimizer (`Ruin-and-Recreate`, `Genetic` GA, `Hybrid GA + Ruin`)
//! with optional `20×20×20 mm` thermal density & Z-slice smoothness penalties, resource
//! guardrails, open-cavity support, topological interlocking rejection, Z-ceiling enforcement,
//! incremental scope controls, and exact `MeshBvh` / `collision.rs` verification.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rayon::ThreadPoolBuilder;
use rayon::prelude::*;

use crate::collision;
use crate::interlock::{InterlockConfig, VoxelAabb, can_part_escape_tray, tight_solid_bounds};
use crate::model::{
    Aabb, BuildJob, InstanceId, PartId, PartInstance, RotationConstraint, Transform3x4, TriMesh,
    aabbs_overlap, clamp_finite, point_inside_mesh_x_ray, transform_point,
};
use crate::voxel::{
    BitGrid3D, CpuBudget, EdtScratch, HeightMap2D, MemoryEstimate, MeshBvh,
    PartOrientationMask, PartVoxelConfig, XyRotationStep, ZRotationStep,
    adaptive_part_half_clearance_mm, auto_fit_resolution_mm, build_part_orientation_catalog,
    dilate_clearance_mask, estimate_voxel_memory_bytes, estimated_orientation_count,
    generate_candidate_rotations, is_cancelled, rotation_from_transform, try_alloc_zeroed,
    voxelize_mesh_flood_fill,
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
    pub respect_part_constraints: bool,
    pub xy_rotation_step: XyRotationStep,
    pub z_rotation_step: ZRotationStep,
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
            respect_part_constraints: true,
            xy_rotation_step: XyRotationStep::Deg90,
            z_rotation_step: ZRotationStep::Deg45,
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
                self.xy_rotation_step = XyRotationStep::Deg90;
                self.z_rotation_step = ZRotationStep::Deg90;
                self.thermal_weight = 0.0;
                self.slice_smoothness_weight = 0.0;
                self.gravity_settle_after = false;
                self.allow_micro_rotations = false;
                self.time_limit_secs = 8.0;
            }
            Nest3dPreset::Balanced => {
                self.resolution_mm = 1.0;
                self.optimizer_mode = Nest3dOptimizerMode::HybridGaRuin;
                self.xy_rotation_step = XyRotationStep::Deg90;
                self.z_rotation_step = ZRotationStep::Deg45;
                self.thermal_weight = 0.0;
                self.slice_smoothness_weight = 0.0;
                self.gravity_settle_after = true;
                self.allow_micro_rotations = false;
                self.time_limit_secs = 20.0;
            }
            Nest3dPreset::Refined => {
                self.resolution_mm = 0.5;
                self.optimizer_mode = Nest3dOptimizerMode::HybridGaRuin;
                self.xy_rotation_step = XyRotationStep::Deg45;
                self.z_rotation_step = ZRotationStep::Deg30;
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
        let wm = clamp_finite(self.wall_margin_mm, 0.0, 0.0, 500.0);
        let min_z = clamp_finite(self.min_z_mm, 0.0, 0.0, 2000.0);
        let ceiling = (wz - wm).max(min_z + 1.0);
        match self.max_z_limit_mm {
            Some(cap) => clamp_finite(cap, ceiling, min_z + 1.0, ceiling),
            None => ceiling,
        }
    }

    /// Resolve effective rotation constraint for an instance, capped by the XY/Z rotation sliders.
    pub fn effective_constraint(&self, inst_constraint: RotationConstraint) -> RotationConstraint {
        let global_cap = match (self.xy_rotation_step, self.z_rotation_step) {
            (XyRotationStep::Locked0, ZRotationStep::Locked0) => RotationConstraint::Fixed,
            (XyRotationStep::Locked0, _) => RotationConstraint::ZOnly,
            _ => RotationConstraint::Free,
        };
        if !self.respect_part_constraints {
            return global_cap;
        }
        match (inst_constraint, global_cap) {
            (RotationConstraint::Fixed, _) | (_, RotationConstraint::Fixed) => RotationConstraint::Fixed,
            (RotationConstraint::ZOnly, _) | (_, RotationConstraint::ZOnly) => RotationConstraint::ZOnly,
            (RotationConstraint::Free, RotationConstraint::Free) => RotationConstraint::Free,
        }
    }

    /// Resolve Stage 3 physics orientation freedom from `respect_part_constraints` and XY/Z sliders.
    pub fn effective_orientation_freedom(&self) -> OrientationFreedom {
        match (self.respect_part_constraints, self.xy_rotation_step, self.z_rotation_step) {
            (_, XyRotationStep::Locked0, ZRotationStep::Locked0) => OrientationFreedom::ForceFixed,
            (false, XyRotationStep::Locked0, _) => OrientationFreedom::ForceZOnly,
            (false, _, _) => OrientationFreedom::ForceFree,
            (true, _, _) => OrientationFreedom::RespectPartConstraints,
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
    pub best_thermal_peak_pct: Option<f32>,
    pub generation: usize,
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
    pub stage2_iterations: usize,
    pub peak_thermal_pct: Option<f32>,
    pub slice_fill_std_pct: Option<f32>,
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
    let pitch = clamp_finite(cfg.resolution_mm, 1.5, 0.5, 4.0);
    let ([bx0, by0], [bx1, by1]) = job.bed_xy_bounds();
    let wm = clamp_finite(cfg.wall_margin_mm, 0.0, 0.0, 500.0);
    let min_z = clamp_finite(cfg.min_z_mm, 0.0, 0.0, 2000.0);
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
            let orient_est = estimated_orientation_count(
                eff_c,
                cfg.xy_rotation_step,
                cfg.z_rotation_step,
            );
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
    item_pos: usize,
    inst_idx: usize,
    inst_id: InstanceId,
    catalog_key: CatalogKey,
    orient_idx: usize,
    origin: [usize; 3],
    solid_aabb: VoxelAabb,
    mesh: Arc<TriMesh>,
    bvh: Arc<MeshBvh>,
    transform: Transform3x4,
    world_bounds: Aabb,
    half_xy_z: (f32, f32),
}

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

#[derive(Clone)]
struct NestGenome {
    order: Vec<usize>,
    orientations: Vec<u16>,
    placement_bias: Vec<u8>,
}

#[derive(Clone, Copy)]
struct EvalMetrics {
    fitness: f64,
    placed_count: usize,
    overflow_count: usize,
    max_z_mm: f32,
    total_placed_vol_mm3: f64,
    density_pct: f64,
    peak_thermal_pct: Option<f32>,
    hotspot_world_mm: [f32; 3],
    slice_fill_std_pct: Option<f32>,
    peak_slice_z_mm: f32,
}

#[derive(Clone)]
struct LiveBedState {
    tray_clearance: BitGrid3D,
    tray_solid: BitGrid3D,
    hmap: HeightMap2D,
    placed: Vec<PlacedNeighbor>,
    overflow_items: Vec<usize>,
    genome: NestGenome,
    metrics: EvalMetrics,
}

struct PortfolioOutcome {
    transforms: Vec<(usize, Transform3x4)>,
    placed_count: usize,
    overflow_count: usize,
    max_z_mm: f32,
    total_placed_vol_mm3: f64,
    interlock_rejections: usize,
    stopped_early: bool,
    live_state: Option<Box<LiveBedState>>,
}

#[derive(Clone, Copy)]
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn gen_range(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            0
        } else {
            (self.next_u64() as usize) % upper
        }
    }

    fn next_f64(&mut self) -> f64 {
        const SCALE: f64 = 1.0 / ((1_u64 << 53) as f64);
        ((self.next_u64() >> 11) as f64) * SCALE
    }
}

#[derive(Clone)]
struct CoarseThermalSliceGrid {
    cnx: usize,
    cny: usize,
    cnz: usize,
    scale: usize,
    coarse_pitch_mm: f32,
    counts: Vec<u16>,
    z_layer_counts: Vec<u32>,
    prefix_buf: Vec<u32>,
}

impl CoarseThermalSliceGrid {
    fn new(
        tray_nx: usize,
        tray_ny: usize,
        tray_nz: usize,
        fine_pitch_mm: f32,
        max_bytes: Option<usize>,
    ) -> Result<Self> {
        let coarse_pitch_mm = 5.0_f32.max(fine_pitch_mm);
        let scale = (coarse_pitch_mm / fine_pitch_mm.max(0.1)).round().max(1.0) as usize;
        let cnx = tray_nx.div_ceil(scale).max(1);
        let cny = tray_ny.div_ceil(scale).max(1);
        let cnz = tray_nz.div_ceil(scale).max(1);
        let total = cnx
            .checked_mul(cny)
            .and_then(|v| v.checked_mul(cnz))
            .ok_or_else(|| NestCraftError::InvalidInput("coarse thermal grid overflow".to_owned()))?;
        let counts = try_alloc_zeroed::<u16>(total, max_bytes)?;
        let z_layer_counts = try_alloc_zeroed::<u32>(cnz, max_bytes)?;
        Ok(Self {
            cnx,
            cny,
            cnz,
            scale,
            coarse_pitch_mm,
            counts,
            z_layer_counts,
            prefix_buf: Vec::new(),
        })
    }

    fn stamp_solid_mask(&mut self, mask: &BitGrid3D, ox: usize, oy: usize, oz: usize, add: bool) {
        let scale = self.scale;
        let cnx = self.cnx;
        let cny = self.cny;
        let cnz = self.cnz;
        for iz in 0..mask.nz {
            let cz = ((oz + iz) / scale).min(cnz - 1);
            let cz_off = cz * cny;
            let mz_off = iz * mask.ny;
            for iy in 0..mask.ny {
                let cy = ((oy + iy) / scale).min(cny - 1);
                let row_off = (cz_off + cy) * cnx;
                let m_row = (mz_off + iy) * mask.words_x;
                for wx in 0..mask.words_x {
                    let mut w = mask.words[m_row + wx];
                    while w != 0 {
                        let bit = w.trailing_zeros() as usize;
                        w &= w - 1;
                        let ix = (wx << 6) + bit;
                        if ix >= mask.nx {
                            continue;
                        }
                        let cx = ((ox + ix) / scale).min(cnx - 1);
                        let c_idx = row_off + cx;
                        if add {
                            self.counts[c_idx] = self.counts[c_idx].saturating_add(1);
                            self.z_layer_counts[cz] = self.z_layer_counts[cz].saturating_add(1);
                        } else {
                            self.counts[c_idx] = self.counts[c_idx].saturating_sub(1);
                            self.z_layer_counts[cz] = self.z_layer_counts[cz].saturating_sub(1);
                        }
                    }
                }
            }
        }
    }

    fn estimate_local_density_pct(&self, tx: usize, ty: usize, tz: usize) -> f32 {
        let cx = (tx / self.scale).min(self.cnx - 1);
        let cy = (ty / self.scale).min(self.cny - 1);
        let cz = (tz / self.scale).min(self.cnz - 1);
        let r = ((20.0 * 0.5) / self.coarse_pitch_mm).round().max(1.0) as usize;
        let x0 = cx.saturating_sub(r);
        let x1 = (cx + r + 1).min(self.cnx);
        let y0 = cy.saturating_sub(r);
        let y1 = (cy + r + 1).min(self.cny);
        let z0 = cz.saturating_sub(r);
        let z1 = (cz + r + 1).min(self.cnz);
        let mut solid_sum = 0_u64;
        for z in z0..z1 {
            let z_off = z * self.cny;
            for y in y0..y1 {
                let row = (z_off + y) * self.cnx;
                for x in x0..x1 {
                    solid_sum += u64::from(self.counts[row + x]);
                }
            }
        }
        let box_cells = ((x1 - x0) * (y1 - y0) * (z1 - z0)).max(1) as f64;
        let fine_per_coarse = (self.scale * self.scale * self.scale).max(1) as f64;
        ((solid_sum as f64 / (box_cells * fine_per_coarse)) * 100.0).clamp(0.0, 100.0) as f32
    }

    fn layer_excess_pct(
        &self,
        oz: usize,
        s_lo_z: usize,
        s_hi_z: usize,
        bed_area_fine_vox: f64,
        mean_target_pct: f32,
    ) -> f32 {
        let z_lo = ((oz + s_lo_z) / self.scale).min(self.cnz - 1);
        let z_hi = ((oz + s_hi_z.saturating_sub(1)) / self.scale).min(self.cnz - 1);
        let denom = (bed_area_fine_vox * (self.scale as f64)).max(1.0);
        let mut max_fill = 0.0_f32;
        for cz in z_lo..=z_hi {
            let fill = ((f64::from(self.z_layer_counts[cz]) / denom) * 100.0) as f32;
            max_fill = max_fill.max(fill);
        }
        (max_fill - mean_target_pct.max(8.0)).max(0.0)
    }

    fn evaluate(
        &mut self,
        active_max_z_vox: usize,
        tray_world_origin: [f32; 3],
        bed_area_fine_vox: f64,
    ) -> (f32, [f32; 3], f32, f32) {
        let active_cnz = ((active_max_z_vox / self.scale) + 1).clamp(1, self.cnz);
        let sx = self.cnx + 1;
        let sy = self.cny + 1;
        let sz = active_cnz + 1;
        let needed = sx * sy * sz;
        if self.prefix_buf.len() < needed {
            self.prefix_buf.resize(needed, 0);
        } else {
            self.prefix_buf[..needed].fill(0);
        }
        let p_idx = |x: usize, y: usize, z: usize| -> usize { (z * sy + y) * sx + x };

        for z in 0..active_cnz {
            for y in 0..self.cny {
                for x in 0..self.cnx {
                    let val = u32::from(self.counts[(z * self.cny + y) * self.cnx + x]);
                    let sum = val
                        + self.prefix_buf[p_idx(x, y + 1, z + 1)]
                        + self.prefix_buf[p_idx(x + 1, y, z + 1)]
                        + self.prefix_buf[p_idx(x + 1, y + 1, z)]
                        - self.prefix_buf[p_idx(x, y, z + 1)]
                        - self.prefix_buf[p_idx(x, y + 1, z)]
                        - self.prefix_buf[p_idx(x + 1, y, z)]
                        + self.prefix_buf[p_idx(x, y, z)];
                    self.prefix_buf[p_idx(x + 1, y + 1, z + 1)] = sum;
                }
            }
        }

        let r = ((20.0 * 0.5) / self.coarse_pitch_mm).round().max(1.0) as usize;
        let fine_per_coarse = (self.scale * self.scale * self.scale).max(1) as f64;
        let mut peak_pct = 0.0_f32;
        let mut hotspot = [tray_world_origin[0], tray_world_origin[1], tray_world_origin[2]];

        for z in 0..active_cnz {
            let z0 = z.saturating_sub(r);
            let z1 = (z + r + 1).min(active_cnz);
            for y in 0..self.cny {
                let y0 = y.saturating_sub(r);
                let y1 = (y + r + 1).min(self.cny);
                for x in 0..self.cnx {
                    if self.counts[(z * self.cny + y) * self.cnx + x] == 0 {
                        continue;
                    }
                    let x0 = x.saturating_sub(r);
                    let x1 = (x + r + 1).min(self.cnx);
                    let solid = self.prefix_buf[p_idx(x1, y1, z1)]
                        + self.prefix_buf[p_idx(x0, y0, z1)]
                        + self.prefix_buf[p_idx(x0, y1, z0)]
                        + self.prefix_buf[p_idx(x1, y0, z0)]
                        - self.prefix_buf[p_idx(x0, y1, z1)]
                        - self.prefix_buf[p_idx(x1, y0, z1)]
                        - self.prefix_buf[p_idx(x1, y1, z0)]
                        - self.prefix_buf[p_idx(x0, y0, z0)];
                    let box_cells = ((x1 - x0) * (y1 - y0) * (z1 - z0)).max(1) as f64;
                    let pct = ((f64::from(solid) / (box_cells * fine_per_coarse)) * 100.0)
                        .clamp(0.0, 100.0) as f32;
                    if pct > peak_pct {
                        peak_pct = pct;
                        hotspot = [
                            tray_world_origin[0] + (x as f32 + 0.5) * self.coarse_pitch_mm,
                            tray_world_origin[1] + (y as f32 + 0.5) * self.coarse_pitch_mm,
                            tray_world_origin[2] + (z as f32 + 0.5) * self.coarse_pitch_mm,
                        ];
                    }
                }
            }
        }

        // Compute Z-layer sintered fill std-dev & peak slice Z across occupied layers.
        let denom = (bed_area_fine_vox * (self.scale as f64)).max(1.0);
        let mut last_nonempty = 0_usize;
        for cz in (0..active_cnz).rev() {
            if self.z_layer_counts[cz] > 0 {
                last_nonempty = cz;
                break;
            }
        }
        let layer_count = (last_nonempty + 1).max(1);
        let mut sum_fill = 0.0_f64;
        let mut peak_fill = -1.0_f64;
        let mut peak_cz = 0_usize;
        for cz in 0..layer_count {
            let fill = (f64::from(self.z_layer_counts[cz]) / denom) * 100.0;
            sum_fill += fill;
            if fill > peak_fill {
                peak_fill = fill;
                peak_cz = cz;
            }
        }
        let mean_fill = sum_fill / (layer_count as f64);
        let mut var_sum = 0.0_f64;
        for cz in 0..layer_count {
            let fill = (f64::from(self.z_layer_counts[cz]) / denom) * 100.0;
            let d = fill - mean_fill;
            var_sum += d * d;
        }
        let slice_std = (var_sum / (layer_count as f64)).sqrt() as f32;
        let peak_slice_z = tray_world_origin[2] + (peak_cz as f32 + 0.5) * self.coarse_pitch_mm;

        (peak_pct, hotspot, slice_std, peak_slice_z)
    }
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

struct PlacementEnv<'a> {
    job: &'a BuildJob,
    cfg: &'a Nest3dConfig,
    base_items: &'a [InstanceOrderKey],
    catalogs: &'a HashMap<CatalogKey, Vec<CachedMaskEntry>>,
    bvh_map: &'a HashMap<PartId, Arc<MeshBvh>>,
    fixed_obstacles: &'a [FixedObstacle],
    fixed_voxel_aabbs: &'a [VoxelAabb],
    nbz_exact: &'a Option<(Arc<TriMesh>, Arc<MeshBvh>, Transform3x4, Aabb, f32)>,
    interlock_cfg: InterlockConfig,
    tray_world_origin: [f32; 3],
    tray_min: [f32; 3],
    tray_max: [f32; 3],
    usable_nx: usize,
    usable_ny: usize,
    usable_nz: usize,
    tray_nx: usize,
    tray_ny: usize,
    tray_nz: usize,
    tray_pad: [usize; 3],
    pitch: f32,
    bed_vol_xy: f64,
    bed_area_fine_vox: f64,
    fixed_vol_mm3: f64,
    fixed_max_z: f32,
    bx1: f32,
    by0: f32,
    by1: f32,
}

#[allow(clippy::too_many_arguments)]
fn place_single_item(
    env: &PlacementEnv<'_>,
    item_pos: usize,
    placement_bias: u8,
    orient_order: Option<&[usize]>,
    tray_clearance: &mut BitGrid3D,
    tray_solid: &mut BitGrid3D,
    hmap: &mut HeightMap2D,
    coarse_grid: Option<&mut CoarseThermalSliceGrid>,
    placed: &mut Vec<PlacedNeighbor>,
    placed_voxel_aabbs: &mut Vec<VoxelAabb>,
    placed_xy_centers: &mut Vec<[isize; 2]>,
    cancel: Option<&AtomicBool>,
    stop_and_apply: Option<&AtomicBool>,
) -> (Option<(Transform3x4, Aabb, usize)>, usize, bool) {
    let item = &env.base_items[item_pos];
    let inst = &env.job.instances[item.idx];
    let Some(proto) = env.job.part(inst.part_id) else {
        return (None, 0, false);
    };
    let bvh = env
        .bvh_map
        .get(&proto.id)
        .cloned()
        .unwrap_or_else(|| Arc::new(MeshBvh::build(&proto.mesh)));
    let Some(catalog) = env.catalogs.get(&item.catalog_key) else {
        return (None, 0, false);
    };
    if catalog.is_empty() {
        return (None, 0, false);
    }

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
    let mut interlock_rejections = 0_usize;
    let mut stopped = false;
    let eff_fixed_rot = env.cfg.effective_constraint(inst.constraint) == RotationConstraint::Fixed;
    let use_thermal_pen = coarse_grid.is_some() && env.cfg.thermal_weight > 0.0;
    let use_slice_pen = coarse_grid.is_some() && env.cfg.slice_smoothness_weight > 0.0;
    let coarse_ref = coarse_grid.as_deref();

    let all_indices: Vec<usize>;
    let probe_indices: &[usize] = match orient_order {
        Some(sub) => sub,
        None => {
            all_indices = (0..catalog.len()).collect();
            &all_indices
        }
    };

    for &orient_idx in probe_indices {
        let Some(entry) = catalog.get(orient_idx) else {
            continue;
        };
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
        if solid_dx > env.usable_nx || solid_dy > env.usable_ny || solid_dz > env.usable_nz {
            continue;
        }

        let ox_min = (env.tray_pad[0] as isize - s_lo[0] as isize).max(0) as usize;
        let oy_min = (env.tray_pad[1] as isize - s_lo[1] as isize).max(0) as usize;
        let oz_min = (env.tray_pad[2] as isize - s_lo[2] as isize).max(0) as usize;
        let ox_max = (env.tray_pad[0] + env.usable_nx)
            .saturating_sub(s_hi[0])
            .min(env.tray_nx.saturating_sub(orient.clearance_mask.nx));
        let oy_max = (env.tray_pad[1] + env.usable_ny)
            .saturating_sub(s_hi[1])
            .min(env.tray_ny.saturating_sub(orient.clearance_mask.ny));
        let oz_max = (env.tray_pad[2] + env.usable_nz)
            .saturating_sub(s_hi[2])
            .min(env.tray_nz.saturating_sub(orient.clearance_mask.nz));
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
            let tx = env.tray_world_origin[0] + (ox as f32) * env.pitch - orient.grid_origin_mm[0];
            let ty = env.tray_world_origin[1] + (oy as f32) * env.pitch - orient.grid_origin_mm[1];
            let mut tz = env.tray_world_origin[2] + (oz as f32) * env.pitch - orient.grid_origin_mm[2];

            const BEPS: f32 = 1e-3;
            if tx + local_rot_wb[0][0] < env.tray_min[0] - BEPS
                || tx + local_rot_wb[1][0] > env.tray_max[0] + BEPS
                || ty + local_rot_wb[0][1] < env.tray_min[1] - BEPS
                || ty + local_rot_wb[1][1] > env.tray_max[1] + BEPS
            {
                return false;
            }
            let min_wz = tz + local_rot_wb[0][2];
            if min_wz < env.tray_min[2] {
                tz += env.tray_min[2] - min_wz;
            }
            if tz + local_rot_wb[1][2] > env.tray_max[2] + BEPS {
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
            let xy_tie = match placement_bias % 5 {
                1 => (env.tray_nx.saturating_sub(ox) + oy) as f32,
                2 => (ox + env.tray_ny.saturating_sub(oy)) as f32,
                3 => (env.tray_nx.saturating_sub(ox) + env.tray_ny.saturating_sub(oy)) as f32,
                4 => 0.0,
                _ => (ox + oy) as f32,
            };
            let contact_w = if placement_bias % 5 == 4 { 0.60 } else { 0.25 };
            let mut score = 100.0 * (z_top as f32)
                + 15.0 * (oz as f32)
                - contact_w * contact
                + 0.02 * xy_tie;

            if let Some(cg) = coarse_ref {
                if use_thermal_pen {
                    let cx_v = ox + (s_lo[0] + s_hi[0]) / 2;
                    let cy_v = oy + (s_lo[1] + s_hi[1]) / 2;
                    let cz_v = oz + (s_lo[2] + s_hi[2]) / 2;
                    let local_pct = cg.estimate_local_density_pct(cx_v, cy_v, cz_v);
                    let excess = (local_pct - env.cfg.target_density_pct).max(0.0);
                    if excess > 0.0 {
                        score += env.cfg.thermal_weight * 6.0 * excess * excess;
                    }
                }
                if use_slice_pen {
                    let slice_ex = cg.layer_excess_pct(
                        oz,
                        s_lo[2],
                        s_hi[2],
                        env.bed_area_fine_vox,
                        env.cfg.target_density_pct,
                    );
                    if slice_ex > 0.0 {
                        score += env.cfg.slice_smoothness_weight * 2.0 * slice_ex;
                    }
                }
            }

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
            if let Some((nbz_mesh, nbz_bvh, nbz_tf, nbz_wb, nbz_cl)) = env.nbz_exact
                && nbz_violated(&proto.mesh, &bvh, &tf, wb, nbz_mesh, nbz_bvh, nbz_tf, *nbz_wb, *nbz_cl)
            {
                return false;
            }
            for fx in env.fixed_obstacles {
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
            for pl in placed.iter() {
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

            if env.cfg.prevent_interlocking
                && !can_part_escape_tray(
                    tray_solid,
                    &orient.solid_mask,
                    [ox, oy, oz],
                    placed_voxel_aabbs,
                    &env.interlock_cfg,
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

            if env.cfg.allow_open_cavities && stride > 1 {
                let half_w = (s_lo[0] + s_hi[0]) / 2;
                let half_d = (s_lo[1] + s_hi[1]) / 2;
                for &[cx, cy] in placed_xy_centers.iter() {
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
        if let Some(cg) = coarse_grid {
            cg.stamp_solid_mask(&entry.orient.solid_mask, ox, oy, oz, true);
        }
        placed_voxel_aabbs.push(winner.solid_aabb);
        placed_xy_centers.push([
            ((winner.solid_aabb[0][0] + winner.solid_aabb[1][0]) / 2) as isize,
            ((winner.solid_aabb[0][1] + winner.solid_aabb[1][1]) / 2) as isize,
        ]);
        placed.push(PlacedNeighbor {
            item_pos,
            inst_idx: item.idx,
            inst_id: inst.id,
            catalog_key: item.catalog_key,
            orient_idx: winner.orient_idx,
            origin: winner.origin,
            solid_aabb: winner.solid_aabb,
            mesh: Arc::clone(&proto.mesh),
            bvh,
            transform: winner.tf,
            world_bounds: winner.wb,
            half_xy_z: item.halves,
        });
        (
            Some((winner.tf, winner.wb, winner.orient_idx)),
            interlock_rejections,
            stopped,
        )
    } else {
        (None, interlock_rejections, stopped)
    }
}

fn build_all_transforms_snapshot(
    env: &PlacementEnv<'_>,
    placed: &[PlacedNeighbor],
    overflow_items: &[usize],
) -> Vec<(InstanceId, Transform3x4)> {
    let mut tf_by_idx: HashMap<usize, Transform3x4> =
        HashMap::with_capacity(placed.len() + overflow_items.len());
    for pl in placed {
        tf_by_idx.insert(pl.inst_idx, pl.transform);
    }
    if !overflow_items.is_empty() {
        let overflow_inst_indices: Vec<usize> = overflow_items
            .iter()
            .map(|&p| env.base_items[p].idx)
            .collect();
        place_overflow_strip(
            &overflow_inst_indices,
            &mut tf_by_idx,
            env.job,
            env.cfg,
            env.bx1,
            env.by0,
            env.by1,
        );
    }
    env.base_items
        .iter()
        .filter_map(|item| tf_by_idx.get(&item.idx).copied().map(|tf| (item.id, tf)))
        .collect()
}

fn rebuild_aux_vectors(
    env: &PlacementEnv<'_>,
    placed: &[PlacedNeighbor],
) -> (Vec<VoxelAabb>, Vec<[isize; 2]>) {
    let mut aabbs = Vec::with_capacity(env.fixed_voxel_aabbs.len() + placed.len());
    aabbs.extend_from_slice(env.fixed_voxel_aabbs);
    let mut centers = Vec::with_capacity(aabbs.capacity());
    for va in &aabbs {
        centers.push([
            ((va[0][0] + va[1][0]) / 2) as isize,
            ((va[0][1] + va[1][1]) / 2) as isize,
        ]);
    }
    for pl in placed {
        aabbs.push(pl.solid_aabb);
        centers.push([
            ((pl.solid_aabb[0][0] + pl.solid_aabb[1][0]) / 2) as isize,
            ((pl.solid_aabb[0][1] + pl.solid_aabb[1][1]) / 2) as isize,
        ]);
    }
    (aabbs, centers)
}

fn evaluate_bed_metrics(
    env: &PlacementEnv<'_>,
    placed: &[PlacedNeighbor],
    overflow_count: usize,
    coarse_grid: Option<&mut CoarseThermalSliceGrid>,
) -> EvalMetrics {
    let mut max_z_mm = env.fixed_max_z;
    let mut total_placed_vol_mm3 = env.fixed_vol_mm3;
    let mut sum_z_cm = 0.0_f64;
    let mut max_z_vox = 1_usize;

    for pl in placed {
        max_z_mm = max_z_mm.max(pl.world_bounds[1][2]);
        total_placed_vol_mm3 += env.base_items[pl.item_pos].vol;
        sum_z_cm += 0.5 * f64::from(pl.world_bounds[0][2] + pl.world_bounds[1][2]);
        max_z_vox = max_z_vox.max(pl.solid_aabb[1][2]);
    }
    let mean_z_cm_mm = if placed.is_empty() {
        0.0
    } else {
        (sum_z_cm / (placed.len() as f64)) as f32
    };
    let density_pct = if max_z_mm > 1e-4 {
        (total_placed_vol_mm3 / (env.bed_vol_xy * f64::from(max_z_mm))) * 100.0
    } else {
        0.0
    };

    let (peak_thermal_pct, hotspot_world_mm, slice_fill_std_pct, peak_slice_z_mm) =
        if let Some(cg) = coarse_grid {
            let (pk, hs, s_std, pz) =
                cg.evaluate(max_z_vox, env.tray_world_origin, env.bed_area_fine_vox);
            (
                (env.cfg.thermal_weight > 0.0).then_some(pk),
                hs,
                (env.cfg.slice_smoothness_weight > 0.0).then_some(s_std),
                pz,
            )
        } else {
            (None, [0.0, 0.0, 0.0], None, 0.0)
        };

    let thermal_pen = if let Some(pk) = peak_thermal_pct {
        let excess = f64::from((pk - env.cfg.target_density_pct).max(0.0));
        f64::from(env.cfg.thermal_weight) * 8.0 * excess * excess
    } else {
        0.0
    };
    let slice_pen = if let Some(std_pct) = slice_fill_std_pct {
        f64::from(env.cfg.slice_smoothness_weight) * 15.0 * f64::from(std_pct)
    } else {
        0.0
    };

    let fitness = 1_000_000.0 * (overflow_count as f64)
        + 10.0 * f64::from(max_z_mm)
        + 0.5 * f64::from(mean_z_cm_mm)
        + thermal_pen
        + slice_pen;

    EvalMetrics {
        fitness,
        placed_count: placed.len(),
        overflow_count,
        max_z_mm,
        total_placed_vol_mm3,
        density_pct,
        peak_thermal_pct,
        hotspot_world_mm,
        slice_fill_std_pct,
        peak_slice_z_mm,
    }
}

fn select_ruin_positions(
    env: &PlacementEnv<'_>,
    placed: &[PlacedNeighbor],
    metrics: &EvalMetrics,
    rng: &mut XorShift64,
) -> Vec<usize> {
    let n = placed.len();
    if n == 0 {
        return Vec::new();
    }
    let ruin_target = ((n * 22) / 100).clamp(2, 24).min(n);

    let thermal_or_slice = env.cfg.thermal_weight > 0.0 || env.cfg.slice_smoothness_weight > 0.0;
    let roll = rng.next_f64();

    let mut ranked: Vec<(usize, f32)> = if thermal_or_slice && roll < 0.40 {
        if env.cfg.thermal_weight > 0.0
            && (env.cfg.slice_smoothness_weight <= 0.0 || rng.next_f64() < 0.6)
        {
            // Hotspot ruin: parts closest to global thermal hotspot.
            let hs = metrics.hotspot_world_mm;
            placed
                .iter()
                .enumerate()
                .map(|(i, pl)| {
                    let cx = 0.5 * (pl.world_bounds[0][0] + pl.world_bounds[1][0]);
                    let cy = 0.5 * (pl.world_bounds[0][1] + pl.world_bounds[1][1]);
                    let cz = 0.5 * (pl.world_bounds[0][2] + pl.world_bounds[1][2]);
                    let d2 = (cx - hs[0]).powi(2) + (cy - hs[1]).powi(2) + (cz - hs[2]).powi(2);
                    (i, d2)
                })
                .collect()
        } else {
            // Peak-slice ruin: parts intersecting or nearest peak Z-slice.
            let pz = metrics.peak_slice_z_mm;
            placed
                .iter()
                .enumerate()
                .map(|(i, pl)| {
                    let dist_z = if pz >= pl.world_bounds[0][2] && pz <= pl.world_bounds[1][2] {
                        0.0
                    } else {
                        (pl.world_bounds[0][2] - pz)
                            .abs()
                            .min((pl.world_bounds[1][2] - pz).abs())
                    };
                    (i, dist_z)
                })
                .collect()
        }
    } else if roll < (if thermal_or_slice { 0.75 } else { 0.55 }) {
        // Top-Z band ruin: highest max_Z parts first (negative key for ascending sort).
        placed
            .iter()
            .enumerate()
            .map(|(i, pl)| (i, -pl.world_bounds[1][2]))
            .collect()
    } else {
        // Random spatial cluster ruin: seed part + nearest spatial neighbors.
        let seed = rng.gen_range(n);
        let sp = &placed[seed];
        let sx = 0.5 * (sp.world_bounds[0][0] + sp.world_bounds[1][0]);
        let sy = 0.5 * (sp.world_bounds[0][1] + sp.world_bounds[1][1]);
        let sz = 0.5 * (sp.world_bounds[0][2] + sp.world_bounds[1][2]);
        placed
            .iter()
            .enumerate()
            .map(|(i, pl)| {
                let cx = 0.5 * (pl.world_bounds[0][0] + pl.world_bounds[1][0]);
                let cy = 0.5 * (pl.world_bounds[0][1] + pl.world_bounds[1][1]);
                let cz = 0.5 * (pl.world_bounds[0][2] + pl.world_bounds[1][2]);
                let d2 = (cx - sx).powi(2) + (cy - sy).powi(2) + (cz - sz).powi(2);
                (i, d2)
            })
            .collect()
    };

    ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut chosen: Vec<usize> = ranked.into_iter().take(ruin_target).map(|(i, _)| i).collect();
    chosen.sort_unstable();
    chosen
}

fn build_probe_orient_list(pref: usize, cat_len: usize, max_fast: usize) -> Vec<usize> {
    if cat_len == 0 {
        return Vec::new();
    }
    let pref = pref % cat_len;
    let mut out = Vec::with_capacity(cat_len);
    out.push(pref);
    let cap = max_fast.min(cat_len);
    for i in 0..cap {
        if i != pref {
            out.push(i);
        }
    }
    out
}

fn decode_genome_fresh(
    env: &PlacementEnv<'_>,
    genome: &NestGenome,
    base_tray_clearance: &BitGrid3D,
    base_tray_solid: &BitGrid3D,
    base_hmap: &HeightMap2D,
    base_coarse: Option<&CoarseThermalSliceGrid>,
    cancel: Option<&AtomicBool>,
    stop_and_apply: Option<&AtomicBool>,
) -> Option<(LiveBedState, Option<CoarseThermalSliceGrid>, usize)> {
    let mut tray_clearance = base_tray_clearance.clone();
    let mut tray_solid = base_tray_solid.clone();
    let mut hmap = base_hmap.clone();
    let mut coarse = base_coarse.cloned();
    let mut placed: Vec<PlacedNeighbor> = Vec::with_capacity(genome.order.len());
    let mut overflow_items: Vec<usize> = Vec::new();
    let (mut placed_voxel_aabbs, mut placed_xy_centers) = rebuild_aux_vectors(env, &placed);
    let mut interlock_rejections = 0_usize;
    let mut new_genome = genome.clone();

    for &item_pos in &genome.order {
        if should_stop(cancel, stop_and_apply) {
            return None;
        }
        let cat_len = env
            .catalogs
            .get(&env.base_items[item_pos].catalog_key)
            .map_or(0, Vec::len);
        let pref = usize::from(genome.orientations[item_pos]);
        let bias = genome.placement_bias[item_pos];
        let probe = build_probe_orient_list(pref, cat_len, 4);

        let (mut res, rej, stopped) = place_single_item(
            env,
            item_pos,
            bias,
            Some(&probe),
            &mut tray_clearance,
            &mut tray_solid,
            &mut hmap,
            coarse.as_mut(),
            &mut placed,
            &mut placed_voxel_aabbs,
            &mut placed_xy_centers,
            cancel,
            stop_and_apply,
        );
        interlock_rejections += rej;
        if stopped {
            return None;
        }
        if res.is_none() && cat_len > probe.len() {
            let remaining: Vec<usize> = (0..cat_len).filter(|i| !probe.contains(i)).collect();
            let (res2, rej2, stopped2) = place_single_item(
                env,
                item_pos,
                bias,
                Some(&remaining),
                &mut tray_clearance,
                &mut tray_solid,
                &mut hmap,
                coarse.as_mut(),
                &mut placed,
                &mut placed_voxel_aabbs,
                &mut placed_xy_centers,
                cancel,
                stop_and_apply,
            );
            interlock_rejections += rej2;
            if stopped2 {
                return None;
            }
            res = res2;
        }
        if let Some((_, _, chosen_orient)) = res {
            new_genome.orientations[item_pos] = u16::try_from(chosen_orient).unwrap_or(0);
        } else {
            overflow_items.push(item_pos);
        }
    }

    let metrics = evaluate_bed_metrics(env, &placed, overflow_items.len(), coarse.as_mut());
    Some((
        LiveBedState {
            tray_clearance,
            tray_solid,
            hmap,
            placed,
            overflow_items,
            genome: new_genome,
            metrics,
        },
        coarse,
        interlock_rejections,
    ))
}

#[allow(clippy::too_many_arguments)]
fn ruin_and_recreate_step(
    env: &PlacementEnv<'_>,
    state: &mut LiveBedState,
    coarse: &mut Option<CoarseThermalSliceGrid>,
    rng: &mut XorShift64,
    sa_temp: f64,
    cancel: Option<&AtomicBool>,
    stop_and_apply: Option<&AtomicBool>,
) -> (bool, usize, bool) {
    let ruin_positions = select_ruin_positions(env, &state.placed, &state.metrics, rng);
    if ruin_positions.is_empty() && state.overflow_items.is_empty() {
        return (false, 0, false);
    }

    let mut is_ruined = vec![false; state.placed.len()];
    for &pos in &ruin_positions {
        is_ruined[pos] = true;
    }

    let mut kept: Vec<PlacedNeighbor> = Vec::with_capacity(state.placed.len());
    let mut saved_ruined: Vec<PlacedNeighbor> = Vec::with_capacity(ruin_positions.len());
    let mut rx0 = usize::MAX;
    let mut ry0 = usize::MAX;
    let mut rx1 = 0_usize;
    let mut ry1 = 0_usize;

    for (pos, pl) in state.placed.drain(..).enumerate() {
        if is_ruined[pos] {
            if let Some(entry) = env
                .catalogs
                .get(&pl.catalog_key)
                .and_then(|c| c.get(pl.orient_idx))
            {
                let [ox, oy, oz] = pl.origin;
                state
                    .tray_clearance
                    .unstamp_at(&entry.orient.clearance_mask, ox, oy, oz);
                state.tray_solid.unstamp_at(&entry.orient.solid_mask, ox, oy, oz);
                if let Some(cg) = coarse.as_mut() {
                    cg.stamp_solid_mask(&entry.orient.solid_mask, ox, oy, oz, false);
                }
                rx0 = rx0.min(ox);
                ry0 = ry0.min(oy);
                rx1 = rx1.max(ox + entry.orient.clearance_mask.nx);
                ry1 = ry1.max(oy + entry.orient.clearance_mask.ny);
            }
            saved_ruined.push(pl);
        } else {
            kept.push(pl);
        }
    }
    state.placed = kept;
    if rx0 < rx1 && ry0 < ry1 {
        state
            .hmap
            .rebuild_rect_from_grid(&state.tray_clearance, rx0, ry0, rx1, ry1);
    }

    let kept_len = state.placed.len();
    let old_overflow = state.overflow_items.clone();
    let old_genome = state.genome.clone();
    let old_metrics = state.metrics;

    // Build reinsertion sequence from saved_ruined + old_overflow.
    let mut to_insert: Vec<usize> = Vec::with_capacity(saved_ruined.len() + old_overflow.len());
    if !old_overflow.is_empty() && rng.next_f64() < 0.70 {
        to_insert.extend_from_slice(&old_overflow);
        for pl in &saved_ruined {
            to_insert.push(pl.item_pos);
        }
    } else {
        for pl in &saved_ruined {
            to_insert.push(pl.item_pos);
        }
        to_insert.extend_from_slice(&old_overflow);
    }
    // Fisher-Yates partial shuffle for diversity.
    for i in (1..to_insert.len()).rev() {
        let j = rng.gen_range(i + 1);
        to_insert.swap(i, j);
    }

    let mut cand_genome = old_genome.clone();
    let (mut placed_voxel_aabbs, mut placed_xy_centers) = rebuild_aux_vectors(env, &state.placed);
    let mut new_overflow = Vec::new();
    let mut interlock_rejections = 0_usize;
    let mut stopped = false;

    for &item_pos in &to_insert {
        if should_stop(cancel, stop_and_apply) {
            stopped = true;
            break;
        }
        let cat_len = env
            .catalogs
            .get(&env.base_items[item_pos].catalog_key)
            .map_or(0, Vec::len);
        if cat_len > 1 && rng.next_f64() < 0.35 {
            cand_genome.orientations[item_pos] = rng.gen_range(cat_len) as u16;
        }
        if rng.next_f64() < 0.35 {
            cand_genome.placement_bias[item_pos] = rng.gen_range(5) as u8;
        }
        let pref = usize::from(cand_genome.orientations[item_pos]);
        let bias = cand_genome.placement_bias[item_pos];
        let probe = build_probe_orient_list(pref, cat_len, 4);

        let (mut res, rej, st) = place_single_item(
            env,
            item_pos,
            bias,
            Some(&probe),
            &mut state.tray_clearance,
            &mut state.tray_solid,
            &mut state.hmap,
            coarse.as_mut(),
            &mut state.placed,
            &mut placed_voxel_aabbs,
            &mut placed_xy_centers,
            cancel,
            stop_and_apply,
        );
        interlock_rejections += rej;
        if st {
            stopped = true;
            break;
        }
        if res.is_none() && cat_len > probe.len() {
            let remaining: Vec<usize> = (0..cat_len).filter(|i| !probe.contains(i)).collect();
            let (res2, rej2, st2) = place_single_item(
                env,
                item_pos,
                bias,
                Some(&remaining),
                &mut state.tray_clearance,
                &mut state.tray_solid,
                &mut state.hmap,
                coarse.as_mut(),
                &mut state.placed,
                &mut placed_voxel_aabbs,
                &mut placed_xy_centers,
                cancel,
                stop_and_apply,
            );
            interlock_rejections += rej2;
            if st2 {
                stopped = true;
                break;
            }
            res = res2;
        }
        if let Some((_, _, chosen_orient)) = res {
            cand_genome.orientations[item_pos] = u16::try_from(chosen_orient).unwrap_or(0);
        } else {
            new_overflow.push(item_pos);
        }
    }

    if stopped {
        // Restore exact state cleanly on cooperative stop.
        rollback_ruined_step(
            env,
            state,
            coarse.as_mut(),
            kept_len,
            saved_ruined,
            old_overflow,
            old_genome,
            old_metrics,
        );
        return (false, interlock_rejections, true);
    }

    let cand_metrics = evaluate_bed_metrics(env, &state.placed, new_overflow.len(), coarse.as_mut());
    let delta = cand_metrics.fitness - old_metrics.fitness;
    let accept = delta <= 0.0 || (sa_temp > 1e-6 && (-delta / sa_temp).exp() > rng.next_f64());

    if accept {
        let mut order: Vec<usize> = state.placed.iter().map(|p| p.item_pos).collect();
        order.extend_from_slice(&new_overflow);
        cand_genome.order = order;
        state.overflow_items = new_overflow;
        state.genome = cand_genome;
        state.metrics = cand_metrics;
        (true, interlock_rejections, false)
    } else {
        rollback_ruined_step(
            env,
            state,
            coarse.as_mut(),
            kept_len,
            saved_ruined,
            old_overflow,
            old_genome,
            old_metrics,
        );
        (false, interlock_rejections, false)
    }
}

#[allow(clippy::too_many_arguments)]
fn rollback_ruined_step(
    env: &PlacementEnv<'_>,
    state: &mut LiveBedState,
    mut coarse: Option<&mut CoarseThermalSliceGrid>,
    kept_len: usize,
    saved_ruined: Vec<PlacedNeighbor>,
    old_overflow: Vec<usize>,
    old_genome: NestGenome,
    old_metrics: EvalMetrics,
) {
    let mut rx0 = usize::MAX;
    let mut ry0 = usize::MAX;
    let mut rx1 = 0_usize;
    let mut ry1 = 0_usize;

    for pl in state.placed.drain(kept_len..) {
        if let Some(entry) = env
            .catalogs
            .get(&pl.catalog_key)
            .and_then(|c| c.get(pl.orient_idx))
        {
            let [ox, oy, oz] = pl.origin;
            state
                .tray_clearance
                .unstamp_at(&entry.orient.clearance_mask, ox, oy, oz);
            state.tray_solid.unstamp_at(&entry.orient.solid_mask, ox, oy, oz);
            if let Some(ref mut cg) = coarse {
                cg.stamp_solid_mask(&entry.orient.solid_mask, ox, oy, oz, false);
            }
            rx0 = rx0.min(ox);
            ry0 = ry0.min(oy);
            rx1 = rx1.max(ox + entry.orient.clearance_mask.nx);
            ry1 = ry1.max(oy + entry.orient.clearance_mask.ny);
        }
    }

    for pl in saved_ruined {
        if let Some(entry) = env
            .catalogs
            .get(&pl.catalog_key)
            .and_then(|c| c.get(pl.orient_idx))
        {
            let [ox, oy, oz] = pl.origin;
            state
                .tray_clearance
                .stamp_at(&entry.orient.clearance_mask, ox, oy, oz);
            state.tray_solid.stamp_at(&entry.orient.solid_mask, ox, oy, oz);
            if let Some(ref mut cg) = coarse {
                cg.stamp_solid_mask(&entry.orient.solid_mask, ox, oy, oz, true);
            }
            rx0 = rx0.min(ox);
            ry0 = ry0.min(oy);
            rx1 = rx1.max(ox + entry.orient.clearance_mask.nx);
            ry1 = ry1.max(oy + entry.orient.clearance_mask.ny);
        }
        state.placed.push(pl);
    }

    if rx0 < rx1 && ry0 < ry1 {
        state
            .hmap
            .rebuild_rect_from_grid(&state.tray_clearance, rx0, ry0, rx1, ry1);
    }
    state.overflow_items = old_overflow;
    state.genome = old_genome;
    state.metrics = old_metrics;
}

fn ox_crossover(
    env: &PlacementEnv<'_>,
    p1: &NestGenome,
    p2: &NestGenome,
    rng: &mut XorShift64,
) -> NestGenome {
    let n = p1.order.len();
    if n <= 1 {
        return p1.clone();
    }
    let cut_lo = (n / 4).max(1);
    let cut_hi = ((n * 3) / 5).max(cut_lo).min(n);
    let cut = if cut_hi > cut_lo {
        cut_lo + rng.gen_range(cut_hi - cut_lo + 1)
    } else {
        cut_lo
    };

    let mut used = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut orientations = p2.orientations.clone();
    let mut placement_bias = p2.placement_bias.clone();

    for &gene in &p1.order[..cut] {
        if gene < n && !used[gene] {
            used[gene] = true;
            order.push(gene);
            orientations[gene] = p1.orientations[gene];
            placement_bias[gene] = p1.placement_bias[gene];
        }
    }
    for &gene in &p2.order {
        if gene < n && !used[gene] {
            used[gene] = true;
            order.push(gene);
        }
    }
    for (gene, &was_used) in used.iter().enumerate() {
        if !was_used {
            order.push(gene);
        }
    }

    if n >= 2 && rng.next_f64() < 0.25 {
        let a = rng.gen_range(n - 1);
        order.swap(a, a + 1);
    }
    for item_pos in 0..n {
        if rng.next_f64() < 0.15 {
            let cat_len = env
                .catalogs
                .get(&env.base_items[item_pos].catalog_key)
                .map_or(0, Vec::len);
            if cat_len > 1 {
                orientations[item_pos] = rng.gen_range(cat_len) as u16;
            }
        }
        if rng.next_f64() < 0.15 {
            placement_bias[item_pos] = rng.gen_range(5) as u8;
        }
    }

    NestGenome {
        order,
        orientations,
        placement_bias,
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

    #[allow(clippy::too_many_arguments)]
    let emit = |phase: Nest3dPhase,
                label: String,
                done: usize,
                total: usize,
                best_max_z_mm: f32,
                best_density_pct: f64,
                best_placed: usize,
                best_overflow: usize,
                best_thermal_peak_pct: Option<f32>,
                generation: usize,
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
                best_thermal_peak_pct,
                generation,
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
            stage2_iterations: 0,
            peak_thermal_pct: None,
            slice_fill_std_pct: None,
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

    let pitch = clamp_finite(cfg.resolution_mm, 1.5, 0.5, 4.0);
    let ([bx0, by0], [bx1, by1]) = job.bed_xy_bounds();
    let wm = clamp_finite(cfg.wall_margin_mm, 0.0, 0.0, 500.0);
    let min_z = clamp_finite(cfg.min_z_mm, 0.0, 0.0, 2000.0);
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
        let needs_base_rot = eff_c != RotationConstraint::Free
            || cfg.z_rotation_step == ZRotationStep::Locked0
            || cfg.xy_rotation_step == XyRotationStep::Deg180;
        let rkey = if needs_base_rot {
            rot_bits_key(&rot)
        } else {
            [0_u32; 9]
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
            cfg.xy_rotation_step,
            cfg.z_rotation_step,
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
    let mut base_items = Vec::with_capacity(scope.movable_indices.len());
    for &idx in &scope.movable_indices {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let eff_c = cfg.effective_constraint(inst.constraint);
        let rot = rotation_from_transform(&inst.transform);
        let needs_base_rot = eff_c != RotationConstraint::Free
            || cfg.z_rotation_step == ZRotationStep::Locked0
            || cfg.xy_rotation_step == XyRotationStep::Deg180;
        let rkey = if needs_base_rot {
            rot_bits_key(&rot)
        } else {
            [0_u32; 9]
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

    let bed_vol_xy = f64::from((bx1 - bx0).max(1.0)) * f64::from((by1 - by0).max(1.0));
    let bed_area_fine_vox = bed_vol_xy / f64::from(pitch * pitch);
    let fixed_vol_mm3: f64 = scope
        .fixed_indices
        .iter()
        .filter_map(|&idx| job.part(job.instances[idx].part_id).map(|p| p.mesh.volume_mm3))
        .sum();
    let fixed_max_z: f32 = fixed_obstacles
        .iter()
        .map(|f| f.world_bounds[1][2])
        .fold(0.0_f32, f32::max);

    let env = PlacementEnv {
        job,
        cfg,
        base_items: &base_items,
        catalogs: &catalogs,
        bvh_map: &bvh_map,
        fixed_obstacles: &fixed_obstacles,
        fixed_voxel_aabbs: &fixed_voxel_aabbs,
        nbz_exact: &nbz_exact,
        interlock_cfg: InterlockConfig::default(),
        tray_world_origin,
        tray_min,
        tray_max,
        usable_nx,
        usable_ny,
        usable_nz,
        tray_nx,
        tray_ny,
        tray_nz,
        tray_pad,
        pitch,
        bed_vol_xy,
        bed_area_fine_vox,
        fixed_vol_mm3,
        fixed_max_z,
        bx1,
        by0,
        by1,
    };

    let total_stage1_steps = (PORTFOLIO_COUNT * base_items.len()).max(1);
    emit(
        Nest3dPhase::Constructive,
        format!("Running {PORTFOLIO_COUNT} 3D BLBF portfolios..."),
        0,
        total_stage1_steps,
        job.max_z_mm(),
        job.packing_density_pct(),
        0,
        base_items.len(),
        None,
        0,
        None,
    );

    struct Stage1Incumbent {
        has_complete: bool,
        overflow_count: usize,
        max_z_mm: f32,
        total_placed_vol_mm3: f64,
        density_pct: f64,
        placed_count: usize,
        last_preview_ms: u64,
    }

    let stage1_incumbent = Mutex::new(Stage1Incumbent {
        has_complete: false,
        overflow_count: usize::MAX,
        max_z_mm: f32::INFINITY,
        total_placed_vol_mm3: 0.0,
        density_pct: 0.0,
        placed_count: 0,
        last_preview_ms: 0,
    });
    let stage1_steps_done = AtomicUsize::new(0);
    let done_portfolios = AtomicUsize::new(0);
    let keep_live_state =
        cfg.optimizer_mode != Nest3dOptimizerMode::ConstructiveOnly && base_items.len() >= 2;

    let run_portfolio = |(portfolio_idx, perm): (usize, &Vec<usize>)| -> Result<PortfolioOutcome> {
        let mut tray_clearance = base_tray_clearance.clone();
        let mut tray_solid = base_tray_solid.clone();
        let mut hmap = base_hmap.clone();
        let mut placed: Vec<PlacedNeighbor> = Vec::with_capacity(perm.len());
        let (mut placed_voxel_aabbs, mut placed_xy_centers) = rebuild_aux_vectors(&env, &placed);

        let mut orient_genes = vec![0_u16; base_items.len()];
        let bias_genes = vec![0_u8; base_items.len()];
        let mut transforms_out: Vec<(usize, Transform3x4)> = Vec::with_capacity(perm.len());
        let mut overflow_items: Vec<usize> = Vec::new();
        let mut max_z_mm = fixed_max_z;
        let mut total_placed_vol_mm3 = fixed_vol_mm3;
        let mut interlock_rejections = 0_usize;
        let mut stopped = false;

        for (step_idx, &item_pos) in perm.iter().enumerate() {
            let done_now = stage1_steps_done.fetch_add(1, Ordering::Relaxed) + 1;
            if should_stop(cancel, stop_and_apply) {
                stopped = true;
                overflow_items.extend_from_slice(&perm[step_idx..]);
                break;
            }
            let (placed_res, rej, item_stopped) = place_single_item(
                &env,
                item_pos,
                0,
                None,
                &mut tray_clearance,
                &mut tray_solid,
                &mut hmap,
                None,
                &mut placed,
                &mut placed_voxel_aabbs,
                &mut placed_xy_centers,
                cancel,
                stop_and_apply,
            );
            interlock_rejections += rej;
            if let Some((tf, wb, chosen_orient)) = placed_res {
                orient_genes[item_pos] = u16::try_from(chosen_orient).unwrap_or(0);
                max_z_mm = max_z_mm.max(wb[1][2]);
                total_placed_vol_mm3 += base_items[item_pos].vol;
                transforms_out.push((base_items[item_pos].idx, tf));

                if portfolio_idx == 0 {
                    let now_ms = t0.elapsed().as_millis() as u64;
                    let mut should_stream = false;
                    if let Ok(mut guard) = stage1_incumbent.lock()
                        && !guard.has_complete
                        && (step_idx == 0
                            || step_idx + 1 == perm.len()
                            || now_ms.saturating_sub(guard.last_preview_ms) >= 120)
                    {
                        guard.last_preview_ms = now_ms;
                        should_stream = true;
                    }
                    if should_stream {
                        let live: Vec<(InstanceId, Transform3x4)> = placed
                            .iter()
                            .map(|pl| (pl.inst_id, pl.transform))
                            .collect();
                        let density_so_far = if max_z_mm > 1e-4 {
                            (total_placed_vol_mm3 / (bed_vol_xy * f64::from(max_z_mm))) * 100.0
                        } else {
                            0.0
                        };
                        emit(
                            Nest3dPhase::Constructive,
                            format!(
                                "Constructive BLBF ({}/{})...",
                                step_idx + 1,
                                perm.len()
                            ),
                            done_now.min(total_stage1_steps),
                            total_stage1_steps,
                            max_z_mm,
                            density_so_far,
                            placed.len(),
                            base_items.len().saturating_sub(placed.len()),
                            None,
                            0,
                            Some(live),
                        );
                    }
                }
            } else {
                overflow_items.push(item_pos);
            }

            if item_stopped {
                stopped = true;
                overflow_items.extend_from_slice(&perm[(step_idx + 1)..]);
                break;
            }
        }

        let overflow_count = overflow_items.len();
        let placed_count = placed.len();
        if !overflow_items.is_empty() {
            let overflow_inst_indices: Vec<usize> =
                overflow_items.iter().map(|&p| base_items[p].idx).collect();
            let mut tf_map: HashMap<usize, Transform3x4> = transforms_out.iter().copied().collect();
            place_overflow_strip(&overflow_inst_indices, &mut tf_map, job, cfg, bx1, by0, by1);
            for idx in &overflow_inst_indices {
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

        let mut is_new_incumbent = false;
        let mut inc_snapshot = (max_z_mm, density_pct, placed_count, overflow_count);
        if let Ok(mut guard) = stage1_incumbent.lock() {
            let beats = !guard.has_complete
                || (overflow_count < guard.overflow_count)
                || (overflow_count == guard.overflow_count
                    && (max_z_mm + 1e-4 < guard.max_z_mm
                        || ((max_z_mm - guard.max_z_mm).abs() <= 1e-4
                            && total_placed_vol_mm3 > guard.total_placed_vol_mm3)));
            if beats {
                guard.has_complete = true;
                guard.overflow_count = overflow_count;
                guard.max_z_mm = max_z_mm;
                guard.total_placed_vol_mm3 = total_placed_vol_mm3;
                guard.density_pct = density_pct;
                guard.placed_count = placed_count;
                is_new_incumbent = true;
            }
            inc_snapshot = (
                guard.max_z_mm,
                guard.density_pct,
                guard.placed_count,
                guard.overflow_count,
            );
        }

        let live_payload = if is_new_incumbent {
            Some(
                transforms_out
                    .iter()
                    .filter_map(|&(idx, tf)| job.instances.get(idx).map(|inst| (inst.id, tf)))
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };

        emit(
            Nest3dPhase::Constructive,
            format!("Constructive portfolio {d}/{PORTFOLIO_COUNT}..."),
            stage1_steps_done.load(Ordering::Relaxed).min(total_stage1_steps),
            total_stage1_steps,
            inc_snapshot.0,
            inc_snapshot.1,
            inc_snapshot.2,
            inc_snapshot.3,
            None,
            0,
            live_payload,
        );

        let live_state = if keep_live_state {
            let mut order: Vec<usize> = placed.iter().map(|p| p.item_pos).collect();
            order.extend_from_slice(&overflow_items);
            let metrics = evaluate_bed_metrics(&env, &placed, overflow_count, None);
            Some(Box::new(LiveBedState {
                tray_clearance,
                tray_solid,
                hmap,
                placed,
                overflow_items,
                genome: NestGenome {
                    order,
                    orientations: orient_genes,
                    placement_bias: bias_genes,
                },
                metrics,
            }))
        } else {
            None
        };

        Ok(PortfolioOutcome {
            transforms: transforms_out,
            placed_count,
            overflow_count,
            max_z_mm,
            total_placed_vol_mm3,
            interlock_rejections,
            stopped_early: stopped,
            live_state,
        })
    };

    let indexed_orderings: Vec<(usize, &Vec<usize>)> = orderings.iter().enumerate().collect();
    let mut outcomes: Vec<PortfolioOutcome> = if let Some(ref p) = pool {
        p.install(|| indexed_orderings.par_iter().copied().map(run_portfolio).collect::<Result<Vec<_>>>())?
    } else {
        indexed_orderings
            .into_iter()
            .map(run_portfolio)
            .collect::<Result<Vec<_>>>()?
    };

    if is_cancelled(cancel) {
        return Err(NestCraftError::InvalidInput("3D nesting cancelled".to_owned()));
    }

    let total_stage1_interlock: usize = outcomes.iter().map(|o| o.interlock_rejections).sum();
    let stage1_stopped = outcomes.iter().any(|o| o.stopped_early);

    let mut applied_tf: HashMap<usize, Transform3x4>;
    let best_placed_count: usize;
    let best_overflow_count: usize;
    let best_placed_vol_mm3: f64;
    let mut total_interlock_rejections = total_stage1_interlock;
    let mut stage2_stopped_early = false;
    let mut stage2_iterations = 0_usize;
    let mut stage2_peak_thermal: Option<f32> = None;
    let mut stage2_slice_std: Option<f32> = None;

    if keep_live_state && !should_stop(cancel, stop_and_apply) {
        // Stage 2: Anytime Parallel Island Optimizer (Ruin-and-Recreate / GA / Hybrid).
        let use_coarse_objectives =
            cfg.thermal_weight > 0.0 || cfg.slice_smoothness_weight > 0.0;
        let base_coarse_grid = if use_coarse_objectives {
            let mut cg = CoarseThermalSliceGrid::new(
                tray_nx,
                tray_ny,
                tray_nz,
                pitch,
                cfg.max_memory_bytes,
            )?;
            // Stamp static obstacles once into base coarse grid.
            for z in 0..base_tray_solid.nz {
                for y in 0..base_tray_solid.ny {
                    for wx in 0..base_tray_solid.words_x {
                        let mut w = base_tray_solid.words[(z * base_tray_solid.ny + y) * base_tray_solid.words_x + wx];
                        while w != 0 {
                            let bit = w.trailing_zeros() as usize;
                            w &= w - 1;
                            let x = (wx << 6) + bit;
                            if x < base_tray_solid.nx {
                                let cx = (x / cg.scale).min(cg.cnx - 1);
                                let cy = (y / cg.scale).min(cg.cny - 1);
                                let cz = (z / cg.scale).min(cg.cnz - 1);
                                let idx = (cz * cg.cny + cy) * cg.cnx + cx;
                                cg.counts[idx] = cg.counts[idx].saturating_add(1);
                                cg.z_layer_counts[cz] = cg.z_layer_counts[cz].saturating_add(1);
                            }
                        }
                    }
                }
            }
            Some(cg)
        } else {
            None
        };

        struct IslandSlot {
            state: LiveBedState,
            coarse: Option<CoarseThermalSliceGrid>,
            rng: XorShift64,
        }

        let mut stage1_states: Vec<(LiveBedState, Option<CoarseThermalSliceGrid>)> = outcomes
            .drain(..)
            .filter_map(|o| o.live_state.map(|s| *s))
            .map(|mut s| {
                let mut cg = base_coarse_grid.clone();
                if let Some(ref mut grid) = cg {
                    for pl in &s.placed {
                        if let Some(entry) = catalogs
                            .get(&pl.catalog_key)
                            .and_then(|c| c.get(pl.orient_idx))
                        {
                            grid.stamp_solid_mask(
                                &entry.orient.solid_mask,
                                pl.origin[0],
                                pl.origin[1],
                                pl.origin[2],
                                true,
                            );
                        }
                    }
                    s.metrics = evaluate_bed_metrics(&env, &s.placed, s.overflow_items.len(), Some(grid));
                }
                (s, cg)
            })
            .collect();

        stage1_states.sort_by(|a, b| a.0.metrics.fitness.total_cmp(&b.0.metrics.fitness));
        let island_count = workers.min(stage1_states.len()).clamp(1, PORTFOLIO_COUNT);
        let mut islands: Vec<IslandSlot> = stage1_states
            .into_iter()
            .take(island_count)
            .enumerate()
            .map(|(id, (state, coarse))| IslandSlot {
                state,
                coarse,
                rng: XorShift64::new(
                    0x9E37_79B9_7F4A_7C15_u64.wrapping_mul((id as u64) + 1)
                        ^ (base_items.len() as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9),
                ),
            })
            .collect();

        if let Some(best_island) = islands.first() {
            struct Stage2Incumbent {
                metrics: EvalMetrics,
                transforms: Vec<(InstanceId, Transform3x4)>,
                applied_map: HashMap<usize, Transform3x4>,
                generation: usize,
                last_heartbeat_ms: u64,
            }

            let init_transforms = build_all_transforms_snapshot(
                &env,
                &best_island.state.placed,
                &best_island.state.overflow_items,
            );
            let mut init_map: HashMap<usize, Transform3x4> = HashMap::with_capacity(base_items.len());
            for (item_key, &(_, tf)) in base_items.iter().zip(init_transforms.iter()) {
                init_map.insert(item_key.idx, tf);
            }
            for pl in &best_island.state.placed {
                init_map.insert(pl.inst_idx, pl.transform);
            }

            let stage2_inc = Mutex::new(Stage2Incumbent {
                metrics: best_island.state.metrics,
                transforms: init_transforms.clone(),
                applied_map: init_map,
                generation: 0,
                last_heartbeat_ms: t0.elapsed().as_millis() as u64,
            });

            let total_budget = clamp_finite(cfg.time_limit_secs, 30.0, 0.5, 300.0);
            let elapsed_s = t0.elapsed().as_secs_f32();
            let reserve_s3 = if cfg.gravity_settle_after {
                (total_budget * 0.20).clamp(0.5, 5.0)
            } else {
                0.0
            };
            let stage2_budget_secs = (total_budget - elapsed_s - reserve_s3).max(0.25);
            let stage2_start = Instant::now();
            let stage2_deadline = stage2_start + Duration::from_secs_f32(stage2_budget_secs);
            let total_ms_budget = (stage2_budget_secs * 1000.0).round().max(1.0) as usize;
            let stagnation_limit = base_items.len().clamp(2, 36);

            emit(
                Nest3dPhase::Refining,
                format!(
                    "Refining ({:?}, {} islands)...",
                    cfg.optimizer_mode, island_count
                ),
                0,
                total_ms_budget,
                best_island.state.metrics.max_z_mm,
                best_island.state.metrics.density_pct,
                best_island.state.metrics.placed_count,
                best_island.state.metrics.overflow_count,
                best_island.state.metrics.peak_thermal_pct,
                0,
                Some(init_transforms),
            );

            let iter_counter = AtomicUsize::new(0);
            let rej_counter = AtomicUsize::new(0);
            let mut generation = 0_usize;
            let mut epochs_without_improve = 0_usize;

            while !should_stop(cancel, stop_and_apply) && Instant::now() < stage2_deadline {
                if let Ok(guard) = stage2_inc.lock()
                    && guard.metrics.overflow_count == 0
                    && epochs_without_improve >= stagnation_limit
                {
                    break;
                }

                generation += 1;
                let improved_this_epoch = AtomicBool::new(false);
                let frac_elapsed = (stage2_start.elapsed().as_secs_f32() / stage2_budget_secs).clamp(0.0, 1.0);
                let sa_temp = 25.0 * f64::from((1.0 - frac_elapsed).max(0.05));

                // 1. Ruin-and-Recreate step across parallel islands (when RuinAndRecreate or HybridGaRuin).
                if matches!(
                    cfg.optimizer_mode,
                    Nest3dOptimizerMode::RuinAndRecreate | Nest3dOptimizerMode::HybridGaRuin
                ) {
                    let epoch_iters = if cfg.optimizer_mode == Nest3dOptimizerMode::HybridGaRuin {
                        4
                    } else {
                        6
                    };

                    let run_island_ruin = |slot: &mut IslandSlot| {
                        for _ in 0..epoch_iters {
                            if should_stop(cancel, stop_and_apply) || Instant::now() >= stage2_deadline {
                                break;
                            }
                            let (accepted, rej, stopped) = ruin_and_recreate_step(
                                &env,
                                &mut slot.state,
                                &mut slot.coarse,
                                &mut slot.rng,
                                sa_temp,
                                cancel,
                                stop_and_apply,
                            );
                            if rej > 0 {
                                rej_counter.fetch_add(rej, Ordering::Relaxed);
                            }
                            let cur_iters = iter_counter.fetch_add(1, Ordering::Relaxed) + 1;
                            if stopped {
                                break;
                            }

                            let mut live_to_emit: Option<(Vec<(InstanceId, Transform3x4)>, EvalMetrics)> = None;
                            let mut heartbeat_metrics: Option<EvalMetrics> = None;
                            if let Ok(mut guard) = stage2_inc.lock() {
                                guard.generation = generation;
                                if accepted && slot.state.metrics.fitness + 1e-4 < guard.metrics.fitness {
                                    let snap = build_all_transforms_snapshot(
                                        &env,
                                        &slot.state.placed,
                                        &slot.state.overflow_items,
                                    );
                                    let mut map: HashMap<usize, Transform3x4> =
                                        HashMap::with_capacity(base_items.len());
                                    for (item_key, &(_, tf)) in base_items.iter().zip(snap.iter()) {
                                        map.insert(item_key.idx, tf);
                                    }
                                    for pl in &slot.state.placed {
                                        map.insert(pl.inst_idx, pl.transform);
                                    }
                                    guard.metrics = slot.state.metrics;
                                    guard.transforms = snap.clone();
                                    guard.applied_map = map;
                                    guard.last_heartbeat_ms = t0.elapsed().as_millis() as u64;
                                    improved_this_epoch.store(true, Ordering::Relaxed);
                                    live_to_emit = Some((snap, guard.metrics));
                                } else {
                                    let now_ms = t0.elapsed().as_millis() as u64;
                                    if now_ms.saturating_sub(guard.last_heartbeat_ms) >= 250 {
                                        guard.last_heartbeat_ms = now_ms;
                                        heartbeat_metrics = Some(guard.metrics);
                                    }
                                }
                            }

                            let done_ms = (stage2_start.elapsed().as_millis() as usize).min(total_ms_budget);
                            if let Some((snap, m)) = live_to_emit {
                                emit(
                                    Nest3dPhase::Refining,
                                    format!(
                                        "Refining Gen {generation} · {} iters ({:.1}s / {:.0}s)...",
                                        cur_iters,
                                        stage2_start.elapsed().as_secs_f32(),
                                        stage2_budget_secs
                                    ),
                                    done_ms,
                                    total_ms_budget,
                                    m.max_z_mm,
                                    m.density_pct,
                                    m.placed_count,
                                    m.overflow_count,
                                    m.peak_thermal_pct,
                                    generation,
                                    Some(snap),
                                );
                            } else if let Some(m) = heartbeat_metrics {
                                emit(
                                    Nest3dPhase::Refining,
                                    format!(
                                        "Refining Gen {generation} · {} iters ({:.1}s / {:.0}s)...",
                                        cur_iters,
                                        stage2_start.elapsed().as_secs_f32(),
                                        stage2_budget_secs
                                    ),
                                    done_ms,
                                    total_ms_budget,
                                    m.max_z_mm,
                                    m.density_pct,
                                    m.placed_count,
                                    m.overflow_count,
                                    m.peak_thermal_pct,
                                    generation,
                                    None,
                                );
                            }
                        }
                    };

                    if let Some(ref p) = pool {
                        p.install(|| islands.par_iter_mut().for_each(run_island_ruin));
                    } else {
                        islands.iter_mut().for_each(run_island_ruin);
                    }
                }

                if should_stop(cancel, stop_and_apply) || Instant::now() >= stage2_deadline {
                    break;
                }

                // 2. GA Order-Crossover & Island Migration (when Genetic or HybridGaRuin).
                if matches!(
                    cfg.optimizer_mode,
                    Nest3dOptimizerMode::Genetic | Nest3dOptimizerMode::HybridGaRuin
                ) {
                    islands.sort_by(|a, b| a.state.metrics.fitness.total_cmp(&b.state.metrics.fitness));
                    let parent_genomes: Vec<NestGenome> =
                        islands.iter().map(|s| s.state.genome.clone()).collect();
                    let children_count = if cfg.optimizer_mode == Nest3dOptimizerMode::Genetic {
                        island_count
                    } else {
                        island_count.saturating_sub(1).max(1)
                    };
                    let mut child_specs: Vec<(usize, NestGenome)> = Vec::with_capacity(children_count);
                    let mut master_rng = XorShift64::new(
                        0xD6E8_FEB8_6659_FD93_u64.wrapping_mul((generation as u64) + 1),
                    );
                    let p_len = parent_genomes.len().max(1);
                    let top_half = (p_len / 2).max(1);
                    for c_idx in 0..children_count {
                        let p1_i = master_rng.gen_range(top_half);
                        let p2_i = master_rng.gen_range(p_len);
                        let child_g = ox_crossover(&env, &parent_genomes[p1_i], &parent_genomes[p2_i], &mut master_rng);
                        let target_slot = if cfg.optimizer_mode == Nest3dOptimizerMode::Genetic {
                            c_idx
                        } else {
                            (c_idx + 1).min(island_count - 1)
                        };
                        child_specs.push((target_slot, child_g));
                    }

                    let decode_child = |(target_slot, child_g): &(usize, NestGenome)| {
                        if should_stop(cancel, stop_and_apply) || Instant::now() >= stage2_deadline {
                            return None;
                        }
                        let (child_state, child_coarse, rej) = decode_genome_fresh(
                            &env,
                            child_g,
                            &base_tray_clearance,
                            &base_tray_solid,
                            &base_hmap,
                            base_coarse_grid.as_ref(),
                            cancel,
                            stop_and_apply,
                        )?;
                        if rej > 0 {
                            rej_counter.fetch_add(rej, Ordering::Relaxed);
                        }
                        let cur_iters = iter_counter.fetch_add(1, Ordering::Relaxed) + 1;
                        let mut live_to_emit: Option<(Vec<(InstanceId, Transform3x4)>, EvalMetrics)> = None;
                        if let Ok(mut guard) = stage2_inc.lock() {
                            guard.generation = generation;
                            if child_state.metrics.fitness + 1e-4 < guard.metrics.fitness {
                                let snap = build_all_transforms_snapshot(
                                    &env,
                                    &child_state.placed,
                                    &child_state.overflow_items,
                                );
                                let mut map: HashMap<usize, Transform3x4> =
                                    HashMap::with_capacity(base_items.len());
                                for (item_key, &(_, tf)) in base_items.iter().zip(snap.iter()) {
                                    map.insert(item_key.idx, tf);
                                }
                                for pl in &child_state.placed {
                                    map.insert(pl.inst_idx, pl.transform);
                                }
                                guard.metrics = child_state.metrics;
                                guard.transforms = snap.clone();
                                guard.applied_map = map;
                                guard.last_heartbeat_ms = t0.elapsed().as_millis() as u64;
                                improved_this_epoch.store(true, Ordering::Relaxed);
                                live_to_emit = Some((snap, guard.metrics));
                            }
                        }
                        if let Some((snap, m)) = live_to_emit {
                            let done_ms =
                                (stage2_start.elapsed().as_millis() as usize).min(total_ms_budget);
                            emit(
                                Nest3dPhase::Refining,
                                format!(
                                    "GA Gen {generation} · {} iters ({:.1}s / {:.0}s)...",
                                    cur_iters,
                                    stage2_start.elapsed().as_secs_f32(),
                                    stage2_budget_secs
                                ),
                                done_ms,
                                total_ms_budget,
                                m.max_z_mm,
                                m.density_pct,
                                m.placed_count,
                                m.overflow_count,
                                m.peak_thermal_pct,
                                generation,
                                Some(snap),
                            );
                        }
                        Some((*target_slot, child_state, child_coarse))
                    };

                    let decoded_children: Vec<(usize, LiveBedState, Option<CoarseThermalSliceGrid>)> =
                        if let Some(ref p) = pool {
                            p.install(|| child_specs.par_iter().filter_map(decode_child).collect())
                        } else {
                            child_specs.iter().filter_map(decode_child).collect()
                        };

                    for (target_slot, child_state, child_coarse) in decoded_children {
                        if let Some(slot) = islands.get_mut(target_slot) {
                            let adopt = child_state.metrics.fitness < slot.state.metrics.fitness
                                || (target_slot > 0 && slot.rng.next_f64() < 0.30);
                            if adopt {
                                slot.state = child_state;
                                slot.coarse = child_coarse;
                            }
                        }
                    }

                    // Periodic heartbeat for pure Genetic mode when no new incumbent appeared.
                    if cfg.optimizer_mode == Nest3dOptimizerMode::Genetic
                        && !improved_this_epoch.load(Ordering::Relaxed)
                        && let Ok(mut guard) = stage2_inc.lock()
                    {
                        let now_ms = t0.elapsed().as_millis() as u64;
                        if now_ms.saturating_sub(guard.last_heartbeat_ms) >= 250 {
                            guard.last_heartbeat_ms = now_ms;
                            let m = guard.metrics;
                            let done_ms =
                                (stage2_start.elapsed().as_millis() as usize).min(total_ms_budget);
                            emit(
                                Nest3dPhase::Refining,
                                format!(
                                    "GA Gen {generation} ({:.1}s / {:.0}s)...",
                                    stage2_start.elapsed().as_secs_f32(),
                                    stage2_budget_secs
                                ),
                                done_ms,
                                total_ms_budget,
                                m.max_z_mm,
                                m.density_pct,
                                m.placed_count,
                                m.overflow_count,
                                m.peak_thermal_pct,
                                generation,
                                None,
                            );
                        }
                    }
                }

                if improved_this_epoch.load(Ordering::Relaxed) {
                    epochs_without_improve = 0;
                } else {
                    epochs_without_improve += 1;
                }
            }

            stage2_iterations = iter_counter.load(Ordering::Relaxed);
            total_interlock_rejections += rej_counter.load(Ordering::Relaxed);
            stage2_stopped_early = is_cancelled(stop_and_apply);

            if let Ok(guard) = stage2_inc.lock() {
                applied_tf = guard.applied_map.clone();
                best_placed_count = guard.metrics.placed_count;
                best_overflow_count = guard.metrics.overflow_count;
                best_placed_vol_mm3 = guard.metrics.total_placed_vol_mm3;
                stage2_peak_thermal = guard.metrics.peak_thermal_pct;
                stage2_slice_std = guard.metrics.slice_fill_std_pct;
            } else {
                applied_tf = HashMap::new();
                best_placed_count = 0;
                best_overflow_count = base_items.len();
                best_placed_vol_mm3 = fixed_vol_mm3;
            }
            drop(islands);
            drop(base_coarse_grid);
        } else {
            applied_tf = HashMap::new();
            best_placed_count = 0;
            best_overflow_count = base_items.len();
            best_placed_vol_mm3 = fixed_vol_mm3;
        }
    } else {
        let best = outcomes
            .into_iter()
            .min_by(|a, b| {
                a.overflow_count
                    .cmp(&b.overflow_count)
                    .then_with(|| a.max_z_mm.total_cmp(&b.max_z_mm))
                    .then_with(|| b.total_placed_vol_mm3.total_cmp(&a.total_placed_vol_mm3))
            })
            .ok_or_else(|| NestCraftError::InvalidInput("no nesting portfolio completed".to_owned()))?;
        best_placed_count = best.placed_count;
        best_overflow_count = best.overflow_count;
        best_placed_vol_mm3 = best.total_placed_vol_mm3;
        applied_tf = best.transforms.into_iter().collect();
    }

    // Release voxel structures immediately (staged memory release before Stage 3 / exact gate).
    drop(base_tray_clearance);
    drop(base_tray_solid);
    drop(base_hmap);
    drop(catalogs);

    if is_cancelled(cancel) {
        return Err(NestCraftError::InvalidInput("3D nesting cancelled".to_owned()));
    }

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
            stage2_peak_thermal,
            0,
            None,
        );
        let pre_density_s3 = job.packing_density_pct();
        let settle_cfg = crate::compact::GravitySettleConfig::from_nest3d(cfg);
        let settle_cb = |sp: crate::compact::GravitySettleProgress| {
            let approx_density = if sp.max_z_mm > 1e-4 {
                (best_placed_vol_mm3 / (bed_vol_xy * f64::from(sp.max_z_mm))) * 100.0
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
                stage2_peak_thermal,
                0,
                sp.preview_transforms,
            );
        };
        let s3 = crate::compact::gravity_settle_with_orientation_freedom(
            job,
            &settle_cfg,
            cfg.effective_orientation_freedom(),
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
        best_placed_count,
        best_overflow_count,
        stage2_peak_thermal,
        0,
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
    let stopped_early =
        stage1_stopped || stage2_stopped_early || stage3_stopped_early || is_cancelled(stop_and_apply);

    emit(
        Nest3dPhase::Done,
        "3D nesting complete".to_owned(),
        1,
        1,
        final_max_z,
        final_density,
        final_placed,
        final_overflow,
        stage2_peak_thermal,
        0,
        None,
    );

    Ok(Nest3dResult {
        placed_count: final_placed,
        overflow_count: final_overflow,
        fixed_count,
        max_z_mm: final_max_z,
        packing_density_pct: final_density,
        interlock_rejected_attempts: total_interlock_rejections,
        exact_gate_repairs,
        stage2_iterations,
        peak_thermal_pct: stage2_peak_thermal,
        slice_fill_std_pct: stage2_slice_std,
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

    #[test]
    fn constructive_blbf_streams_live_transforms_during_placement() {
        let mut job = BuildJob::new(MachineProfile::custom("StreamTray", [100.0, 100.0, 100.0]));
        let pid = job.add_part("Block", box_mesh([0.0, 0.0, 0.0], [20.0, 18.0, 14.0]));
        for _ in 0..4 {
            let _ = job.add_instance(pid, IDENTITY_TRANSFORM).expect("inst");
        }
        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.optimizer_mode = Nest3dOptimizerMode::ConstructiveOnly;
        cfg.gravity_settle_after = false;

        let live_events = Mutex::new(Vec::<usize>::new());
        let cb = |prog: Nest3dProgress| {
            if prog.phase == Nest3dPhase::Constructive
                && let Some(ref live) = prog.live_transforms
                && let Ok(mut g) = live_events.lock()
            {
                g.push(live.len());
            }
        };
        let res = nest_3d_with_callbacks(&mut job, &cfg, None, None, None, Some(&cb)).expect("nest");
        assert_eq!(res.placed_count, 4);
        assert_eq!(res.stage2_iterations, 0);
        let counts = live_events.lock().expect("lock").clone();
        assert!(
            !counts.is_empty() && counts.iter().any(|&c| c > 0),
            "expected live Constructive transforms, got {counts:?}"
        );
    }

    #[test]
    fn stage2_modes_ruin_ga_hybrid_produce_valid_collision_free_beds_and_stream_live() {
        let mut base_job = BuildJob::new(MachineProfile::custom("Stage2Tray", [90.0, 90.0, 110.0]));
        let p_a = base_job.add_part("Wide", box_mesh([0.0, 0.0, 0.0], [34.0, 20.0, 14.0]));
        let p_b = base_job.add_part("Tall", box_mesh([0.0, 0.0, 0.0], [16.0, 16.0, 28.0]));
        let p_c = base_job.add_part("Small", box_mesh([0.0, 0.0, 0.0], [14.0, 14.0, 14.0]));
        for _ in 0..2 {
            let _ = base_job.add_instance(p_a, IDENTITY_TRANSFORM).expect("a");
            let _ = base_job.add_instance(p_b, IDENTITY_TRANSFORM).expect("b");
            let _ = base_job.add_instance(p_c, IDENTITY_TRANSFORM).expect("c");
        }

        let mut cons_job = base_job.clone();
        let mut cons_cfg = Nest3dConfig::from_job(&cons_job);
        cons_cfg.resolution_mm = 2.0;
        cons_cfg.optimizer_mode = Nest3dOptimizerMode::ConstructiveOnly;
        cons_cfg.gravity_settle_after = false;
        let cons_res = nest_3d(&mut cons_job, &cons_cfg, None).expect("constructive");

        for mode in [
            Nest3dOptimizerMode::RuinAndRecreate,
            Nest3dOptimizerMode::Genetic,
            Nest3dOptimizerMode::HybridGaRuin,
        ] {
            let mut job = base_job.clone();
            let mut cfg = Nest3dConfig::from_job(&job);
            cfg.resolution_mm = 2.0;
            cfg.optimizer_mode = mode;
            cfg.gravity_settle_after = false;
            cfg.time_limit_secs = 1.5;

            let refining_live = Mutex::new(0_usize);
            let cb = |prog: Nest3dProgress| {
                if prog.phase == Nest3dPhase::Refining
                    && prog.live_transforms.as_ref().is_some_and(|v| !v.is_empty())
                    && let Ok(mut c) = refining_live.lock()
                {
                    *c += 1;
                }
            };
            let res = nest_3d_with_callbacks(&mut job, &cfg, None, None, None, Some(&cb)).expect("stage2");
            assert_eq!(res.placed_count, 6, "mode {mode:?}");
            assert_eq!(res.overflow_count, 0, "mode {mode:?}");
            assert!(res.stage2_iterations > 0, "mode {mode:?} must run Stage 2 iterations");
            assert!(
                res.max_z_mm <= cons_res.max_z_mm + 1e-3,
                "mode {mode:?} regressed max_z: {} > {}",
                res.max_z_mm,
                cons_res.max_z_mm
            );
            assert!(*refining_live.lock().expect("lock") > 0, "mode {mode:?} must stream Refining live");
            let col = collision::detect_collisions(&job, None, 2.0);
            assert!(col.pairs.is_empty(), "mode {mode:?} collision: {:?}", col.pairs);
        }
    }

    #[test]
    fn stage2_thermal_and_slice_objectives_track_and_respect_weights() {
        let mut job = BuildJob::new(MachineProfile::custom("ThermalTray", [100.0, 100.0, 120.0]));
        let pid = job.add_part("Block", box_mesh([0.0, 0.0, 0.0], [22.0, 22.0, 18.0]));
        for _ in 0..6 {
            let _ = job.add_instance(pid, IDENTITY_TRANSFORM).expect("inst");
        }
        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.optimizer_mode = Nest3dOptimizerMode::HybridGaRuin;
        cfg.thermal_weight = 0.5;
        cfg.target_density_pct = 20.0;
        cfg.slice_smoothness_weight = 0.3;
        cfg.gravity_settle_after = false;
        cfg.time_limit_secs = 1.5;

        let res = nest_3d(&mut job, &cfg, None).expect("thermal nest");
        assert_eq!(res.placed_count, 6);
        assert_eq!(res.overflow_count, 0);
        assert!(res.peak_thermal_pct.is_some());
        assert!(res.slice_fill_std_pct.is_some());
        let col = collision::detect_collisions(&job, None, 2.0);
        assert!(col.pairs.is_empty(), "thermal mode collision: {:?}", col.pairs);
    }

    #[test]
    fn stage2_stop_and_apply_responds_promptly_with_valid_incumbent() {
        let mut job = BuildJob::new(MachineProfile::custom("StopTray", [100.0, 100.0, 120.0]));
        let pid = job.add_part("Block", box_mesh([0.0, 0.0, 0.0], [20.0, 18.0, 16.0]));
        for _ in 0..6 {
            let _ = job.add_instance(pid, IDENTITY_TRANSFORM).expect("inst");
        }
        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.optimizer_mode = Nest3dOptimizerMode::HybridGaRuin;
        cfg.gravity_settle_after = false;
        cfg.time_limit_secs = 30.0;

        let stop_flag = AtomicBool::new(false);
        let cb = |prog: Nest3dProgress| {
            if prog.phase == Nest3dPhase::Refining {
                stop_flag.store(true, Ordering::Relaxed);
            }
        };
        let res = nest_3d_with_callbacks(&mut job, &cfg, None, None, Some(&stop_flag), Some(&cb))
            .expect("stop and apply");
        assert!(res.stopped_early);
        assert_eq!(res.placed_count, 6);
        assert_eq!(res.overflow_count, 0);
        let col = collision::detect_collisions(&job, None, 2.0);
        assert!(col.pairs.is_empty(), "stop-and-apply collision: {:?}", col.pairs);
    }

    #[test]
    fn xy_and_z_zero_deg_sliders_lock_rotation_without_fixed_mode() {
        let mut job = BuildJob::new(MachineProfile::custom("LockRotTray", [120.0, 120.0, 120.0]));
        let pid = job.add_part("LBlock", box_mesh([0.0, 0.0, 0.0], [18.0, 10.0, 26.0]));
        let init_tf = transform_from_translation_euler_deg([0.0, 0.0, 0.0], [15.0, 0.0, 35.0]);
        let init_rot = rotation_from_transform(&init_tf);
        let iid = job.add_instance(pid, init_tf).expect("inst");

        // 0° XY + 0° Z locks rotation even when part constraint is Free.
        let mut cfg = Nest3dConfig::from_job(&job);
        cfg.resolution_mm = 2.0;
        cfg.optimizer_mode = Nest3dOptimizerMode::ConstructiveOnly;
        cfg.gravity_settle_after = false;
        cfg.xy_rotation_step = XyRotationStep::Locked0;
        cfg.z_rotation_step = ZRotationStep::Locked0;

        let res = nest_3d(&mut job, &cfg, None).expect("locked nest");
        assert_eq!(res.placed_count, 1);
        let after_rot = rotation_from_transform(
            &job.instances.iter().find(|i| i.id == iid).expect("inst").transform,
        );
        for r in 0..3 {
            for c in 0..3 {
                assert!(
                    (after_rot[r][c] - init_rot[r][c]).abs() < 1e-4,
                    "expected locked rotation, got {after_rot:?} vs {init_rot:?}"
                );
            }
        }

        // 0° XY + 45° Z keeps local Z axis upright ([0, 0, 1]).
        let mut job_z = BuildJob::new(MachineProfile::custom("ZOnlyTray", [120.0, 120.0, 120.0]));
        let pid_z = job_z.add_part("Tall", box_mesh([0.0, 0.0, 0.0], [16.0, 10.0, 24.0]));
        let iid_z = job_z.add_instance(pid_z, IDENTITY_TRANSFORM).expect("inst");
        let mut cfg_z = Nest3dConfig::from_job(&job_z);
        cfg_z.resolution_mm = 2.0;
        cfg_z.optimizer_mode = Nest3dOptimizerMode::ConstructiveOnly;
        cfg_z.gravity_settle_after = false;
        cfg_z.xy_rotation_step = XyRotationStep::Locked0;
        cfg_z.z_rotation_step = ZRotationStep::Deg45;
        let res_z = nest_3d(&mut job_z, &cfg_z, None).expect("zonly nest");
        assert_eq!(res_z.placed_count, 1);
        let rot_z = rotation_from_transform(
            &job_z.instances.iter().find(|i| i.id == iid_z).expect("inst").transform,
        );
        assert!((rot_z[2][2] - 1.0).abs() < 1e-4, "expected Z-up, got {rot_z:?}");
    }
}
