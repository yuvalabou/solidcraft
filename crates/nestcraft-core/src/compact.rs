//! Physics-based 3D gravity compaction (`rapier3d` + `parry3d` V-HACD) for SLS build beds.
//!
//! Used both as `Stage 3` after constructive 3D auto-nesting (`nest3d.rs`) and as a
//! standalone **Gravity Settle** tool in NestCraft.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use rapier3d::glamx::{Mat3, Quat};
use rapier3d::parry::transformation::vhacd::{VHACD, VHACDParameters};
use rapier3d::parry::transformation::voxelization::FillMode;
use rapier3d::prelude::{
    BroadPhaseBvh, CCDSolver, ColliderBuilder, ColliderSet, ImpulseJointSet, IntegrationParameters,
    IslandManager, LockedAxes, MultibodyJointSet, NarrowPhase, PhysicsPipeline, Pose,
    RigidBodyBuilder, RigidBodyHandle, RigidBodySet, Rotation, SharedShape, Vector,
};

use crate::interlock::{InterlockConfig, detect_interlocking};
use crate::model::{
    Aabb, BuildJob, InstanceId, PartId, RotationConstraint, Transform3x4, TriMesh, clamp_finite,
};
use crate::nest3d::{
    ClearanceMode, Nest3dConfig, OrientationFreedom, instance_inside_raw_machine, nbz_violated,
    pair_violates_clearance,
};
use crate::voxel::{
    MeshBvh, adaptive_part_half_clearance_mm, is_cancelled, rotation_from_transform,
};
use crate::{NestCraftError, Result};

/// Scale from millimetre job coordinates into metres for numerically crisp Rapier contacts
/// (`100 mm == 1.0 m`).
const PHYS_SCALE: f32 = 0.01;

/// Small skin safety bias (mm) so multi-part stacks under gravity compress cleanly onto exact clearance.
const CONTACT_SKIN_BIAS_MM: f32 = 0.22;

/// Target physics pull direction during gravity compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettleDirection {
    /// Pure `-Z` gravity downward toward `min_z_mm` / the build platform.
    #[default]
    DownZ,
    /// `-Z` gravity paired with a gentle inward `(X, Y)` pull toward the chamber centre.
    DownZPlusInwardXY,
}

/// Configuration for physics-based gravity settling / compaction.
#[derive(Debug, Clone, PartialEq)]
pub struct GravitySettleConfig {
    pub selected_only: bool,
    pub keep_in_bounds_fixed: bool,
    pub direction: SettleDirection,
    pub min_distance_mm: f32,
    pub clearance_mode: ClearanceMode,
    pub adaptive_max_distance_mm: f32,
    pub separate_z_distance: bool,
    pub min_z_distance_mm: f32,
    pub wall_margin_mm: f32,
    pub min_z_mm: f32,
    pub max_z_limit_mm: Option<f32>,
    pub allow_micro_rotations: bool,
    pub max_tilt_deg: f32,
    pub max_steps: u32,
    pub time_limit_secs: f32,
    pub vhacd_resolution: u32,
    pub max_convex_hulls: u32,
    pub prevent_interlocking: bool,
    pub avoid_no_build_zone: bool,
}

impl Default for GravitySettleConfig {
    fn default() -> Self {
        Self {
            selected_only: false,
            keep_in_bounds_fixed: false,
            direction: SettleDirection::DownZ,
            min_distance_mm: 2.5,
            clearance_mode: ClearanceMode::Fixed,
            adaptive_max_distance_mm: 5.0,
            separate_z_distance: false,
            min_z_distance_mm: 2.5,
            wall_margin_mm: 2.0,
            min_z_mm: 0.0,
            max_z_limit_mm: None,
            allow_micro_rotations: false,
            max_tilt_deg: 10.0,
            max_steps: 240,
            time_limit_secs: 6.0,
            vhacd_resolution: 32,
            max_convex_hulls: 16,
            prevent_interlocking: true,
            avoid_no_build_zone: true,
        }
    }
}

impl GravitySettleConfig {
    /// Seed standalone settle settings from `job` margins and clearance.
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

    /// Derive Stage 3 post-nest compaction config directly from [`Nest3dConfig`].
    pub fn from_nest3d(cfg: &Nest3dConfig) -> Self {
        Self {
            selected_only: cfg.selected_only,
            keep_in_bounds_fixed: cfg.keep_in_bounds_fixed,
            direction: SettleDirection::DownZ,
            min_distance_mm: cfg.min_distance_mm,
            clearance_mode: cfg.clearance_mode,
            adaptive_max_distance_mm: cfg.adaptive_max_distance_mm,
            separate_z_distance: cfg.separate_z_distance,
            min_z_distance_mm: cfg.min_z_distance_mm,
            wall_margin_mm: cfg.wall_margin_mm,
            min_z_mm: cfg.min_z_mm,
            max_z_limit_mm: cfg.max_z_limit_mm,
            allow_micro_rotations: cfg.allow_micro_rotations,
            max_tilt_deg: cfg.max_tilt_deg,
            max_steps: 180,
            time_limit_secs: clamp_finite(cfg.time_limit_secs * 0.25, 3.0, 1.5, 6.0),
            vhacd_resolution: 32,
            max_convex_hulls: 16,
            prevent_interlocking: cfg.prevent_interlocking,
            avoid_no_build_zone: cfg.avoid_no_build_zone,
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

/// Live progress snapshot emitted during physics compaction.
#[derive(Debug, Clone)]
pub struct GravitySettleProgress {
    pub step: u32,
    pub total_steps: u32,
    pub max_z_mm: f32,
    pub z_reduction_mm: f32,
    pub sleeping_count: usize,
    pub dynamic_count: usize,
    pub preview_transforms: Option<Vec<(InstanceId, Transform3x4)>>,
}

/// Final summary returned by [`gravity_settle`] and [`gravity_settle_with_callbacks`].
#[derive(Debug, Clone, PartialEq)]
pub struct GravitySettleResult {
    pub settled_count: usize,
    pub moved_count: usize,
    pub rolled_back_count: usize,
    pub max_z_before_mm: f32,
    pub max_z_after_mm: f32,
    pub z_reduction_mm: f32,
    pub steps_run: u32,
    pub stopped_early: bool,
    pub elapsed_ms: u64,
}

#[derive(Hash, PartialEq, Eq)]
struct ShapeCacheKey {
    part_id: u64,
    pre_rot_bits: [u32; 4],
    is_static: bool,
}

fn mat3_to_quat(r: [[f32; 3]; 3]) -> Quat {
    let m = Mat3::from_cols(
        Vector::new(r[0][0], r[1][0], r[2][0]),
        Vector::new(r[0][1], r[1][1], r[2][1]),
        Vector::new(r[0][2], r[1][2], r[2][2]),
    );
    let q = Quat::from_mat3(&m);
    if q.is_finite() && q.length_squared() > 1e-8 {
        q.normalize()
    } else {
        Quat::IDENTITY
    }
}

fn quat_to_transform(q: Quat, t_mm: [f32; 3]) -> Transform3x4 {
    let m = Mat3::from_quat(q.normalize());
    let cx = m.x_axis;
    let cy = m.y_axis;
    let cz = m.z_axis;
    [
        [cx.x, cy.x, cz.x, t_mm[0]],
        [cx.y, cy.y, cz.y, t_mm[1]],
        [cx.z, cy.z, cz.z, t_mm[2]],
    ]
}

fn quantize_quat_bits(q: Quat) -> [u32; 4] {
    let qn = q.normalize();
    let sign = if qn.w < 0.0 { -1.0 } else { 1.0 };
    [
        ((qn.x * sign) * 10_000.0).round() as i32 as u32,
        ((qn.y * sign) * 10_000.0).round() as i32 as u32,
        ((qn.z * sign) * 10_000.0).round() as i32 as u32,
        ((qn.w * sign) * 10_000.0).round() as i32 as u32,
    ]
}

fn collect_scaled_mesh_data(
    mesh: &TriMesh,
    pre_rot: Quat,
    scale_xyz: [f32; 3],
) -> (Vec<Vector>, Vec<[u32; 3]>, [f32; 3], [f32; 3]) {
    let mut verts: Vec<Vector> = Vec::with_capacity(mesh.positions.len());
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for &p in &mesh.positions {
        let rp = pre_rot * Vector::new(p[0], p[1], p[2]);
        let v = Vector::new(rp.x * scale_xyz[0], rp.y * scale_xyz[1], rp.z * scale_xyz[2]);
        if !v.is_finite() {
            continue;
        }
        lo[0] = lo[0].min(v.x);
        lo[1] = lo[1].min(v.y);
        lo[2] = lo[2].min(v.z);
        hi[0] = hi[0].max(v.x);
        hi[1] = hi[1].max(v.y);
        hi[2] = hi[2].max(v.z);
        verts.push(v);
    }
    let valid_indices: Vec<[u32; 3]> = mesh
        .indices
        .iter()
        .copied()
        .filter(|[a, b, c]| {
            (*a as usize) < verts.len()
                && (*b as usize) < verts.len()
                && (*c as usize) < verts.len()
                && a != b
                && b != c
                && c != a
        })
        .collect();
    (verts, valid_indices, lo, hi)
}

fn build_static_shape(
    mesh: &TriMesh,
    pre_rot: Quat,
    scale_xyz: [f32; 3],
    resolution: u32,
    max_convex_hulls: u32,
) -> SharedShape {
    let (verts, valid_indices, lo, hi) = collect_scaled_mesh_data(mesh, pre_rot, scale_xyz);
    if verts.len() >= 3 && !valid_indices.is_empty() && lo[0].is_finite() && hi[0].is_finite() {
        let min_span = (hi[0] - lo[0]).min(hi[1] - lo[1]).min(hi[2] - lo[2]);
        if min_span > 1e-4
            && let Ok(trimesh) = SharedShape::trimesh(verts, valid_indices)
        {
            return trimesh;
        }
    }
    build_vhacd_shape(mesh, pre_rot, scale_xyz, resolution, max_convex_hulls)
}

fn build_vhacd_shape(
    mesh: &TriMesh,
    pre_rot: Quat,
    scale_xyz: [f32; 3],
    resolution: u32,
    max_convex_hulls: u32,
) -> SharedShape {
    let (verts, valid_indices, lo, hi) = collect_scaled_mesh_data(mesh, pre_rot, scale_xyz);

    if verts.len() < 4 || mesh.indices.is_empty() || !lo[0].is_finite() {
        return SharedShape::cuboid(0.01, 0.01, 0.01);
    }

    let hx = ((hi[0] - lo[0]) * 0.5).max(1e-4);
    let hy = ((hi[1] - lo[1]) * 0.5).max(1e-4);
    let hz = ((hi[2] - lo[2]) * 0.5).max(1e-4);
    let center = Vector::new((lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5, (lo[2] + hi[2]) * 0.5);
    let fallback_box = || SharedShape::compound(vec![(Pose::from_translation(center), SharedShape::cuboid(hx, hy, hz))]);

    let min_span = (hi[0] - lo[0]).min(hi[1] - lo[1]).min(hi[2] - lo[2]);
    if min_span <= 1e-4 {
        return SharedShape::convex_hull(&verts).unwrap_or_else(fallback_box);
    }

    // Simple 12-triangle boxes/tetrahedra are already convex; skip voxel V-HACD overhead.
    if mesh.indices.len() <= 12
        && let Some(hull) = SharedShape::convex_hull(&verts)
    {
        return hull;
    }

    if valid_indices.is_empty() {
        return SharedShape::convex_hull(&verts).unwrap_or_else(fallback_box);
    }

    let params = VHACDParameters {
        resolution: resolution.clamp(12, 64),
        concavity: 0.005,
        plane_downsampling: 2,
        convex_hull_downsampling: 2,
        convex_hull_approximation: false,
        max_convex_hulls: max_convex_hulls.clamp(1, 32),
        fill_mode: FillMode::FloodFill {
            detect_cavities: false,
        },
        ..VHACDParameters::default()
    };

    let decomp = VHACD::decompose(&params, &verts, &valid_indices, true);
    let mut parts: Vec<(Pose, SharedShape)> = Vec::new();
    for (hull_verts, hull_indices) in decomp.compute_exact_convex_hulls(&verts, &valid_indices) {
        if hull_verts.len() >= 4
            && let Some(convex) = (!hull_indices.is_empty())
                .then(|| SharedShape::convex_mesh(hull_verts.clone(), &hull_indices))
                .flatten()
                .or_else(|| SharedShape::convex_hull(&hull_verts))
        {
            parts.push((Pose::IDENTITY, convex));
        }
    }

    if parts.len() == 1 {
        if let Some((_, single)) = parts.pop() {
            return single;
        }
    } else if parts.len() > 1 {
        return SharedShape::compound(parts);
    }

    SharedShape::convex_hull(&verts).unwrap_or_else(fallback_box)
}

fn effective_constraint_for_orientation_freedom(
    inst_constraint: RotationConstraint,
    freedom: OrientationFreedom,
) -> RotationConstraint {
    match freedom {
        OrientationFreedom::RespectPartConstraints => inst_constraint,
        OrientationFreedom::ForceFixed => RotationConstraint::Fixed,
        OrientationFreedom::ForceZOnly => RotationConstraint::ZOnly,
        OrientationFreedom::ForceFree => RotationConstraint::Free,
    }
}

fn clamp_micro_rotation(q_cur: Quat, q_init: Quat, max_tilt_rad: f32) -> (Quat, bool) {
    let qn = if q_cur.dot(q_init) < 0.0 { -q_cur } else { q_cur };
    let angle = q_init.angle_between(qn);
    if angle > max_tilt_rad && angle > 1e-5 {
        let clamped = q_init.slerp(qn, (max_tilt_rad / angle).clamp(0.0, 1.0)).normalize();
        (clamped, true)
    } else {
        (qn, false)
    }
}

fn aabb_within_chamber(
    wb: Aabb,
    bx0: f32,
    bx1: f32,
    by0: f32,
    by1: f32,
    min_z: f32,
    ceiling_z: f32,
    tol: f32,
) -> bool {
    wb[0][0] >= bx0 - tol
        && wb[1][0] <= bx1 + tol
        && wb[0][1] >= by0 - tol
        && wb[1][1] <= by1 + tol
        && wb[0][2] >= min_z - tol
        && wb[1][2] <= ceiling_z + tol
}

/// Run physics-based gravity settling / compaction on `job`.
pub fn gravity_settle(
    job: &mut BuildJob,
    cfg: &GravitySettleConfig,
    selected_ids: Option<&HashSet<InstanceId>>,
    exclude_ids: Option<&HashSet<InstanceId>>,
) -> Result<GravitySettleResult> {
    gravity_settle_with_callbacks(job, cfg, selected_ids, exclude_ids, None, None, None)
}

/// Run physics-based gravity settling / compaction with cooperative cancellation,
/// early stop-and-apply, and live transform streaming callbacks.
pub fn gravity_settle_with_callbacks(
    job: &mut BuildJob,
    cfg: &GravitySettleConfig,
    selected_ids: Option<&HashSet<InstanceId>>,
    exclude_ids: Option<&HashSet<InstanceId>>,
    cancel: Option<&AtomicBool>,
    stop_and_apply: Option<&AtomicBool>,
    progress: Option<&(dyn Fn(GravitySettleProgress) + Send + Sync)>,
) -> Result<GravitySettleResult> {
    gravity_settle_with_orientation_freedom(
        job,
        cfg,
        OrientationFreedom::RespectPartConstraints,
        selected_ids,
        exclude_ids,
        cancel,
        stop_and_apply,
        progress,
    )
}

struct DynamicPart {
    inst_idx: usize,
    inst_id: InstanceId,
    part_id: PartId,
    handle: RigidBodyHandle,
    initial_tf: Transform3x4,
    pre_rot: Quat,
    initial_rot_for_clamp: Quat,
    clamp_tilt: bool,
    half_xy_z: (f32, f32),
    current_tf: Transform3x4,
}

#[allow(clippy::too_many_arguments)]
fn run_exact_clearance_rollback_pass(
    job: &mut BuildJob,
    cfg: &GravitySettleConfig,
    dynamic_parts: &[DynamicPart],
    fixed_indices: &[usize],
    bvh_map: &HashMap<PartId, MeshBvh>,
    nbz_exact: Option<&(std::sync::Arc<TriMesh>, MeshBvh, Transform3x4, Aabb, f32)>,
    chamber_bounds: [f32; 6],
    accepted: &mut [bool],
    rolled_back: &mut [bool],
) {
    const BOUND_TOL: f32 = 0.05;
    let [bx0, bx1, by0, by1, min_z, ceiling_z] = chamber_bounds;

    for _pass in 0..=dynamic_parts.len() {
        let mut changed = false;

        for pos in 0..dynamic_parts.len() {
            if !accepted[pos] {
                continue;
            }
            let d = &dynamic_parts[pos];
            let inst = &job.instances[d.inst_idx];
            let Some(proto_a) = job.part(inst.part_id) else {
                continue;
            };
            let Some(bvh_a) = bvh_map.get(&proto_a.id) else {
                continue;
            };
            let wb_a = inst.world_bounds(proto_a.mesh.bounds);

            let mut bad = !aabb_within_chamber(wb_a, bx0, bx1, by0, by1, min_z, ceiling_z, BOUND_TOL);

            if !bad
                && let Some((nbz_mesh, nbz_bvh, nbz_tf, nbz_wb, nbz_cl)) = nbz_exact
                && nbz_violated(
                    &proto_a.mesh,
                    bvh_a,
                    &inst.transform,
                    wb_a,
                    nbz_mesh,
                    nbz_bvh,
                    nbz_tf,
                    *nbz_wb,
                    *nbz_cl,
                )
            {
                bad = true;
            }

            if !bad {
                for &fidx in fixed_indices {
                    let fx = &job.instances[fidx];
                    let Some(proto_f) = job.part(fx.part_id) else {
                        continue;
                    };
                    let Some(bvh_f) = bvh_map.get(&proto_f.id) else {
                        continue;
                    };
                    let half_f = cfg.part_half_clearance_xy_z(proto_f.mesh.volume_mm3);
                    if pair_violates_clearance(
                        &proto_a.mesh,
                        bvh_a,
                        &inst.transform,
                        wb_a,
                        (d.half_xy_z.0 + 1e-3, d.half_xy_z.1 + 1e-3),
                        &proto_f.mesh,
                        bvh_f,
                        &fx.transform,
                        fx.world_bounds(proto_f.mesh.bounds),
                        (half_f.0 + 1e-3, half_f.1 + 1e-3),
                    ) {
                        bad = true;
                        break;
                    }
                }
            }

            if !bad {
                for (other_pos, d_other) in dynamic_parts.iter().enumerate() {
                    if other_pos == pos {
                        continue;
                    }
                    let inst_b = &job.instances[d_other.inst_idx];
                    let Some(proto_b) = job.part(inst_b.part_id) else {
                        continue;
                    };
                    let Some(bvh_b) = bvh_map.get(&proto_b.id) else {
                        continue;
                    };
                    if pair_violates_clearance(
                        &proto_a.mesh,
                        bvh_a,
                        &inst.transform,
                        wb_a,
                        (d.half_xy_z.0 + 1e-3, d.half_xy_z.1 + 1e-3),
                        &proto_b.mesh,
                        bvh_b,
                        &inst_b.transform,
                        inst_b.world_bounds(proto_b.mesh.bounds),
                        (d_other.half_xy_z.0 + 1e-3, d_other.half_xy_z.1 + 1e-3),
                    ) {
                        bad = true;
                        break;
                    }
                }
            }

            if bad {
                accepted[pos] = false;
                rolled_back[pos] = true;
                if let Some(inst_mut) = job.instances.get_mut(d.inst_idx) {
                    inst_mut.transform = d.initial_tf;
                }
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn gravity_settle_with_orientation_freedom(
    job: &mut BuildJob,
    cfg: &GravitySettleConfig,
    freedom: OrientationFreedom,
    selected_ids: Option<&HashSet<InstanceId>>,
    exclude_ids: Option<&HashSet<InstanceId>>,
    cancel: Option<&AtomicBool>,
    stop_and_apply: Option<&AtomicBool>,
    progress: Option<&(dyn Fn(GravitySettleProgress) + Send + Sync)>,
) -> Result<GravitySettleResult> {
    let t0 = Instant::now();
    let max_z_before_mm = job.max_z_mm();

    let sel_filter = cfg
        .selected_only
        .then_some(selected_ids)
        .flatten()
        .filter(|s| !s.is_empty());

    let ([raw_bx0, raw_by0], [raw_bx1, raw_by1]) = job.bed_xy_bounds();
    let wm = cfg.wall_margin_mm.max(0.0);
    let bx0 = raw_bx0 + wm;
    let bx1 = (raw_bx1 - wm).max(bx0 + 1.0);
    let by0 = raw_by0 + wm;
    let by1 = (raw_by1 - wm).max(by0 + 1.0);
    let min_z = cfg.min_z_mm.max(0.0);
    let ceiling_z = cfg.effective_max_z(job);
    let chamber_center_x = 0.5 * (bx0 + bx1);
    let chamber_center_y = 0.5 * (by0 + by1);

    let mut movable_indices: Vec<usize> = Vec::new();
    let mut fixed_indices: Vec<usize> = Vec::new();

    for (idx, inst) in job.instances.iter().enumerate() {
        if !job.is_instance_visible(inst) {
            continue;
        }
        if exclude_ids.is_some_and(|ex| ex.contains(&inst.id)) {
            continue;
        }
        // Any part sitting outside the raw bed envelope in +X (e.g. overflow strip) stays untouched.
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let wb = inst.world_bounds(proto.mesh.bounds);
        if wb[0][0] > raw_bx1 + 5.0 {
            continue;
        }
        let selected_ok = sel_filter.is_none_or(|s| s.contains(&inst.id));
        let fixed_by_bounds = cfg.keep_in_bounds_fixed && instance_inside_raw_machine(job, inst);
        if !inst.locked && selected_ok && !fixed_by_bounds {
            movable_indices.push(idx);
        } else {
            fixed_indices.push(idx);
        }
    }

    if movable_indices.is_empty() {
        return Ok(GravitySettleResult {
            settled_count: 0,
            moved_count: 0,
            rolled_back_count: 0,
            max_z_before_mm,
            max_z_after_mm: max_z_before_mm,
            z_reduction_mm: 0.0,
            steps_run: 0,
            stopped_early: false,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        });
    }

    let anisotropic_z =
        cfg.separate_z_distance && !cfg.allow_micro_rotations && cfg.min_distance_mm > 1e-3 && cfg.min_z_distance_mm > 1e-3;
    let z_scale: f32 = if anisotropic_z {
        (cfg.min_distance_mm / cfg.min_z_distance_mm).clamp(0.1, 10.0)
    } else {
        1.0
    };
    let use_pre_rot = (z_scale - 1.0).abs() > 1e-4;
    let scale_xyz = [PHYS_SCALE, PHYS_SCALE, PHYS_SCALE * z_scale];

    // Precompute BVHs & shapes per prototype.
    let mut bvh_map: HashMap<PartId, MeshBvh> = HashMap::new();
    let mut shape_cache: HashMap<ShapeCacheKey, SharedShape> = HashMap::new();

    for &idx in movable_indices.iter().chain(fixed_indices.iter()) {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        bvh_map.entry(proto.id).or_insert_with(|| MeshBvh::build(&proto.mesh));
    }

    let mut bodies = RigidBodySet::new();
    let mut colliders = ColliderSet::new();
    let mut broad_phase = BroadPhaseBvh::new();
    let mut narrow_phase = NarrowPhase::new();
    let mut islands = IslandManager::new();
    let mut impulse_joints = ImpulseJointSet::new();
    let mut multibody_joints = MultibodyJointSet::new();
    let mut ccd_solver = CCDSolver::new();
    let mut pipeline = PhysicsPipeline::new();

    // Nominal minimum part half-clearance (used to offset static chamber walls outward so parts
    // settle flush against min_z_mm / wall_margin_mm rather than hovering inside wall skin).
    let (base_half_xy, base_half_z) = if anisotropic_z || !cfg.separate_z_distance {
        (0.5 * cfg.min_distance_mm.max(0.0), 0.5 * cfg.min_distance_mm.max(0.0))
    } else {
        let h = 0.5 * cfg.min_distance_mm.max(0.0).max(cfg.min_z_distance_mm.max(0.0));
        (h, h)
    };
    let wall_skin_offset_xy = base_half_xy + CONTACT_SKIN_BIAS_MM;
    let wall_skin_offset_z = base_half_z;

    // Static chamber floor, 4 walls, and ceiling.
    let wall_thick = 50.0_f32 * PHYS_SCALE;
    let hx_cham = ((bx1 - bx0) * 0.5 + wall_skin_offset_xy + 10.0) * PHYS_SCALE;
    let hy_cham = ((by1 - by0) * 0.5 + wall_skin_offset_xy + 10.0) * PHYS_SCALE;
    let hz_cham = ((ceiling_z - min_z) * 0.5 + wall_skin_offset_z + 10.0) * scale_xyz[2];
    let cx_p = chamber_center_x * scale_xyz[0];
    let cy_p = chamber_center_y * scale_xyz[1];
    let cz_p = 0.5 * (min_z + ceiling_z) * scale_xyz[2];

    let floor_top_p = (min_z - wall_skin_offset_z) * scale_xyz[2];
    let ceil_bot_p = (ceiling_z + wall_skin_offset_z) * scale_xyz[2];
    let left_x_p = (bx0 - wall_skin_offset_xy) * scale_xyz[0];
    let right_x_p = (bx1 + wall_skin_offset_xy) * scale_xyz[0];
    let front_y_p = (by0 - wall_skin_offset_xy) * scale_xyz[1];
    let back_y_p = (by1 + wall_skin_offset_xy) * scale_xyz[1];

    let static_boxes: [(Vector, Vector); 6] = [
        (
            Vector::new(cx_p, cy_p, floor_top_p - wall_thick),
            Vector::new(hx_cham, hy_cham, wall_thick),
        ),
        (
            Vector::new(cx_p, cy_p, ceil_bot_p + wall_thick),
            Vector::new(hx_cham, hy_cham, wall_thick),
        ),
        (
            Vector::new(left_x_p - wall_thick, cy_p, cz_p),
            Vector::new(wall_thick, hy_cham, hz_cham),
        ),
        (
            Vector::new(right_x_p + wall_thick, cy_p, cz_p),
            Vector::new(wall_thick, hy_cham, hz_cham),
        ),
        (
            Vector::new(cx_p, front_y_p - wall_thick, cz_p),
            Vector::new(hx_cham, wall_thick, hz_cham),
        ),
        (
            Vector::new(cx_p, back_y_p + wall_thick, cz_p),
            Vector::new(hx_cham, wall_thick, hz_cham),
        ),
    ];
    for (pos, half) in static_boxes {
        let rb = bodies.insert(RigidBodyBuilder::fixed().pose(Pose::from_translation(pos)).build());
        let col = ColliderBuilder::cuboid(half.x, half.y, half.z)
            .friction(0.15)
            .restitution(0.0)
            .contact_skin(0.0)
            .build();
        colliders.insert_with_parent(col, rb, &mut bodies);
    }

    // No-build zone fixed obstacle (if active).
    if cfg.avoid_no_build_zone
        && let Some(ref nbz) = job.no_build_zone
        && nbz.enabled
    {
        let tf = nbz.transform();
        let rot = mat3_to_quat(rotation_from_transform(&tf));
        let pre = if use_pre_rot { rot } else { Quat::IDENTITY };
        let body_rot = if use_pre_rot { Quat::IDENTITY } else { rot };
        let key = ShapeCacheKey {
            part_id: u64::MAX,
            pre_rot_bits: quantize_quat_bits(pre),
            is_static: true,
        };
        let shape = shape_cache
            .entry(key)
            .or_insert_with(|| {
                build_static_shape(&nbz.mesh, pre, scale_xyz, cfg.vhacd_resolution, cfg.max_convex_hulls)
            })
            .clone();
        let t_phys = Vector::new(
            tf[0][3] * scale_xyz[0],
            tf[1][3] * scale_xyz[1],
            tf[2][3] * scale_xyz[2],
        );
        let rb = bodies.insert(
            RigidBodyBuilder::fixed()
                .pose(Pose::from_parts(t_phys, body_rot))
                .build(),
        );
        let col = ColliderBuilder::new(shape)
            .friction(0.2)
            .restitution(0.0)
            .contact_skin(nbz.clearance_mm.max(0.0) * PHYS_SCALE)
            .build();
        colliders.insert_with_parent(col, rb, &mut bodies);
    }

    // Fixed parts act as static rigid bodies with their configured half-clearance skin.
    for &idx in &fixed_indices {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let (hx, hz) = cfg.part_half_clearance_xy_z(proto.mesh.volume_mm3);
        let skin_mm = if anisotropic_z || !cfg.separate_z_distance {
            hx
        } else {
            hx.max(hz)
        };
        let rot = mat3_to_quat(rotation_from_transform(&inst.transform));
        let pre = if use_pre_rot { rot } else { Quat::IDENTITY };
        let body_rot = if use_pre_rot { Quat::IDENTITY } else { rot };
        let key = ShapeCacheKey {
            part_id: u64::from(proto.id.0),
            pre_rot_bits: quantize_quat_bits(pre),
            is_static: true,
        };
        let shape = shape_cache
            .entry(key)
            .or_insert_with(|| {
                build_static_shape(
                    &proto.mesh,
                    pre,
                    scale_xyz,
                    cfg.vhacd_resolution,
                    cfg.max_convex_hulls,
                )
            })
            .clone();
        let t_phys = Vector::new(
            inst.transform[0][3] * scale_xyz[0],
            inst.transform[1][3] * scale_xyz[1],
            inst.transform[2][3] * scale_xyz[2],
        );
        let rb = bodies.insert(
            RigidBodyBuilder::fixed()
                .pose(Pose::from_parts(t_phys, body_rot))
                .build(),
        );
        let col = ColliderBuilder::new(shape)
            .friction(0.2)
            .restitution(0.0)
            .contact_skin((skin_mm + CONTACT_SKIN_BIAS_MM) * PHYS_SCALE)
            .build();
        colliders.insert_with_parent(col, rb, &mut bodies);
    }

    // Dynamic bodies for movable parts.
    let mut dynamic_parts: Vec<DynamicPart> = Vec::with_capacity(movable_indices.len());

    for &idx in &movable_indices {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let half_xy_z = cfg.part_half_clearance_xy_z(proto.mesh.volume_mm3);
        let skin_mm = if anisotropic_z || !cfg.separate_z_distance {
            half_xy_z.0
        } else {
            half_xy_z.0.max(half_xy_z.1)
        };

        let init_rot = mat3_to_quat(rotation_from_transform(&inst.transform));
        let pre_rot = if use_pre_rot { init_rot } else { Quat::IDENTITY };
        let body_init_rot = if use_pre_rot { Quat::IDENTITY } else { init_rot };
        let key = ShapeCacheKey {
            part_id: u64::from(proto.id.0),
            pre_rot_bits: quantize_quat_bits(pre_rot),
            is_static: false,
        };
        let shape = shape_cache
            .entry(key)
            .or_insert_with(|| {
                build_vhacd_shape(
                    &proto.mesh,
                    pre_rot,
                    scale_xyz,
                    cfg.vhacd_resolution,
                    cfg.max_convex_hulls,
                )
            })
            .clone();

        let eff_constraint = effective_constraint_for_orientation_freedom(inst.constraint, freedom);
        let (locked_axes, clamp_tilt) = match eff_constraint {
            RotationConstraint::Fixed => (LockedAxes::ROTATION_LOCKED, false),
            RotationConstraint::ZOnly => (LockedAxes::ROTATION_LOCKED_X | LockedAxes::ROTATION_LOCKED_Y, false),
            RotationConstraint::Free => {
                if cfg.allow_micro_rotations {
                    (LockedAxes::empty(), true)
                } else {
                    (LockedAxes::ROTATION_LOCKED_X | LockedAxes::ROTATION_LOCKED_Y, false)
                }
            }
        };

        let t_phys = Vector::new(
            inst.transform[0][3] * scale_xyz[0],
            inst.transform[1][3] * scale_xyz[1],
            inst.transform[2][3] * scale_xyz[2],
        );
        let rb = bodies.insert(
            RigidBodyBuilder::dynamic()
                .pose(Pose::from_parts(t_phys, body_init_rot))
                .linear_damping(6.0)
                .angular_damping(8.0)
                .can_sleep(true)
                .locked_axes(locked_axes)
                .build(),
        );
        let col = ColliderBuilder::new(shape)
            .friction(0.22)
            .restitution(0.0)
            .contact_skin((skin_mm + CONTACT_SKIN_BIAS_MM) * PHYS_SCALE)
            .build();
        colliders.insert_with_parent(col, rb, &mut bodies);

        dynamic_parts.push(DynamicPart {
            inst_idx: idx,
            inst_id: inst.id,
            part_id: inst.part_id,
            handle: rb,
            initial_tf: inst.transform,
            pre_rot,
            initial_rot_for_clamp: body_init_rot,
            clamp_tilt,
            half_xy_z,
            current_tf: inst.transform,
        });
    }

    let integration_parameters = IntegrationParameters {
        dt: 1.0 / 60.0,
        normalized_allowed_linear_error: 0.0001,
        contact_softness: rapier3d::dynamics::SpringCoefficients::new(150.0, 2.0),
        num_solver_iterations: 8,
        ..IntegrationParameters::default()
    };

    let gravity = Vector::new(0.0, 0.0, -3.5);
    let max_steps = cfg.max_steps.clamp(1, 2000);
    let max_tilt_rad = clamp_finite(cfg.max_tilt_deg, 10.0, 0.5, 45.0).to_radians();
    let time_limit_secs = clamp_finite(cfg.time_limit_secs, 10.0, 0.25, 120.0);

    let mut best_snapshot: Vec<Transform3x4> = dynamic_parts.iter().map(|d| d.initial_tf).collect();
    let mut best_max_z = max_z_before_mm;
    let mut best_sum_min_z: f64 = dynamic_parts
        .iter()
        .filter_map(|d| {
            job.part(d.part_id)
                .map(|proto| f64::from(job.instances[d.inst_idx].world_bounds(proto.mesh.bounds)[0][2]))
        })
        .sum();
    let mut steps_run = 0_u32;
    let mut stopped_early = false;
    let mut consecutive_no_improve = 0_u32;

    for step in 1..=max_steps {
        if is_cancelled(cancel) {
            return Err(NestCraftError::InvalidInput("gravity settle cancelled".to_owned()));
        }
        if is_cancelled(stop_and_apply) || t0.elapsed().as_secs_f32() >= time_limit_secs {
            stopped_early = true;
            break;
        }

        if cfg.direction == SettleDirection::DownZPlusInwardXY {
            for d in &dynamic_parts {
                if let Some(rb) = bodies.get_mut(d.handle) {
                    let pos = rb.translation();
                    let mass = rb.mass();
                    let dx = cx_p - pos.x;
                    let dy = cy_p - pos.y;
                    let pull = Vector::new(dx * mass * 2.5, dy * mass * 2.5, 0.0);
                    rb.reset_forces(false);
                    rb.add_force(pull, false);
                }
            }
        }

        pipeline.step(
            gravity,
            &integration_parameters,
            &mut islands,
            &mut broad_phase,
            &mut narrow_phase,
            &mut bodies,
            &mut colliders,
            &mut impulse_joints,
            &mut multibody_joints,
            &mut ccd_solver,
            &(),
            &(),
        );
        steps_run = step;

        let mut sleeping_count = 0_usize;
        let mut step_max_z = 0.0_f32;
        let mut sum_min_z = 0.0_f64;

        // Read back poses + clamp micro-rotations when unlocked.
        for d in &mut dynamic_parts {
            if let Some(rb) = bodies.get_mut(d.handle) {
                let mut rot: Rotation = *rb.rotation();
                if d.clamp_tilt {
                    let (clamped, changed) =
                        clamp_micro_rotation(rot, d.initial_rot_for_clamp, max_tilt_rad);
                    if changed {
                        rot = clamped;
                        rb.set_rotation(rot, false);
                        rb.set_angvel(Vector::ZERO, false);
                    }
                }
                if rb.is_sleeping() {
                    sleeping_count += 1;
                }
                let tr = rb.translation();
                let t_mm = [tr.x / scale_xyz[0], tr.y / scale_xyz[1], tr.z / scale_xyz[2]];
                let final_rot = (rot * d.pre_rot).normalize();
                d.current_tf = quat_to_transform(final_rot, t_mm);
            }
        }

        // Compute current chamber max Z and total base elevation across dynamic parts.
        for &idx in &fixed_indices {
            if let Some(inst) = job.instances.get(idx)
                && let Some(proto) = job.part(inst.part_id)
            {
                step_max_z = step_max_z.max(inst.world_bounds(proto.mesh.bounds)[1][2]);
            }
        }
        for d in &dynamic_parts {
            if let Some(proto) = job.part(d.part_id) {
                let mut probe = job.instances[d.inst_idx].clone();
                probe.transform = d.current_tf;
                let wb = probe.world_bounds(proto.mesh.bounds);
                step_max_z = step_max_z.max(wb[1][2]);
                sum_min_z += f64::from(wb[0][2]);
            }
        }

        let max_z_improved = step_max_z + 1e-3 < best_max_z;
        let sum_z_improved = sum_min_z + 1e-2 < best_sum_min_z;
        if step == 1 || max_z_improved || ((step_max_z - best_max_z).abs() <= 1e-2 && sum_z_improved) {
            best_max_z = if step == 1 { step_max_z } else { best_max_z.min(step_max_z) };
            best_sum_min_z = best_sum_min_z.min(sum_min_z);
            for (i, d) in dynamic_parts.iter().enumerate() {
                best_snapshot[i] = d.current_tf;
            }
        }

        if max_z_improved || sum_z_improved || step == 1 {
            consecutive_no_improve = 0;
        } else {
            consecutive_no_improve += 1;
        }

        if let Some(cb) = progress
            && (step == 1 || step % 15 == 0 || sleeping_count == dynamic_parts.len())
        {
            let preview = dynamic_parts
                .iter()
                .map(|d| (d.inst_id, d.current_tf))
                .collect::<Vec<_>>();
            cb(GravitySettleProgress {
                step,
                total_steps: max_steps,
                max_z_mm: step_max_z,
                z_reduction_mm: (max_z_before_mm - step_max_z).max(0.0),
                sleeping_count,
                dynamic_count: dynamic_parts.len(),
                preview_transforms: Some(preview),
            });
        }

        if sleeping_count == dynamic_parts.len() || (step >= 45 && consecutive_no_improve >= 45) {
            break;
        }
    }

    // Apply latest (or best if current regressed in max_z) physics poses.
    let mut cur_max_z = 0.0_f32;
    for &idx in &fixed_indices {
        if let Some(inst) = job.instances.get(idx)
            && let Some(proto) = job.part(inst.part_id)
        {
            cur_max_z = cur_max_z.max(inst.world_bounds(proto.mesh.bounds)[1][2]);
        }
    }
    for d in &dynamic_parts {
        if let Some(proto) = job.part(d.part_id) {
            let mut probe = job.instances[d.inst_idx].clone();
            probe.transform = d.current_tf;
            let wb = probe.world_bounds(proto.mesh.bounds);
            cur_max_z = cur_max_z.max(wb[1][2]);
        }
    }
    let use_best_snap = cur_max_z > best_max_z + 0.5;

    for (i, d) in dynamic_parts.iter_mut().enumerate() {
        let tf = if use_best_snap { best_snapshot[i] } else { d.current_tf };
        d.current_tf = tf;
        if let Some(inst) = job.instances.get_mut(d.inst_idx) {
            inst.transform = tf;
        }
    }

    // Release Rapier physics world immediately before exact verification gates.
    drop(colliders);
    drop(bodies);
    drop(broad_phase);
    drop(narrow_phase);
    drop(islands);
    drop(impulse_joints);
    drop(multibody_joints);
    let _ = ccd_solver;
    drop(shape_cache);

    // Exact Bounds + Clearance + NBZ rollback fixed-point gate.
    let nbz_exact = if cfg.avoid_no_build_zone {
        job.no_build_zone.as_ref().filter(|n| n.enabled).map(|nbz| {
            (
                nbz.mesh.clone(),
                MeshBvh::build(&nbz.mesh),
                nbz.transform(),
                nbz.world_bounds(),
                nbz.clearance_mm,
            )
        })
    } else {
        None
    };

    let chamber_bounds = [bx0, bx1, by0, by1, min_z, ceiling_z];
    let mut accepted = vec![true; dynamic_parts.len()];
    let mut rolled_back = vec![false; dynamic_parts.len()];

    run_exact_clearance_rollback_pass(
        job,
        cfg,
        &dynamic_parts,
        &fixed_indices,
        &bvh_map,
        nbz_exact.as_ref(),
        chamber_bounds,
        &mut accepted,
        &mut rolled_back,
    );

    // Topological interlocking peel gate: rollback any moved instance trapped by settling,
    // then re-run the bounds/clearance fixed-point pass so reverts cannot overlap active movers.
    if cfg.prevent_interlocking && accepted.iter().any(|&a| a) {
        let il_cfg = InterlockConfig::default();
        if let Ok(il_rep) = detect_interlocking(job, 1.5, &il_cfg, None, cancel)
            && !il_rep.trapped_ids.is_empty()
        {
            let mut any_reverted = false;
            for (pos, d) in dynamic_parts.iter().enumerate() {
                if accepted[pos] && il_rep.trapped_ids.contains(&d.inst_id) {
                    accepted[pos] = false;
                    rolled_back[pos] = true;
                    if let Some(inst_mut) = job.instances.get_mut(d.inst_idx) {
                        inst_mut.transform = d.initial_tf;
                    }
                    any_reverted = true;
                }
            }
            if any_reverted {
                run_exact_clearance_rollback_pass(
                    job,
                    cfg,
                    &dynamic_parts,
                    &fixed_indices,
                    &bvh_map,
                    nbz_exact.as_ref(),
                    chamber_bounds,
                    &mut accepted,
                    &mut rolled_back,
                );
            }
        }
    }

    let mut moved_count = 0_usize;
    let mut rolled_back_count = 0_usize;
    for (pos, d) in dynamic_parts.iter().enumerate() {
        if rolled_back[pos] {
            rolled_back_count += 1;
            continue;
        }
        let tf_after = job.instances[d.inst_idx].transform;
        let dx = tf_after[0][3] - d.initial_tf[0][3];
        let dy = tf_after[1][3] - d.initial_tf[1][3];
        let dz = tf_after[2][3] - d.initial_tf[2][3];
        if (dx * dx + dy * dy + dz * dz) > 1e-4
            || (tf_after[2][2] - d.initial_tf[2][2]).abs() > 1e-4
        {
            moved_count += 1;
        }
    }

    let max_z_after_mm = job.max_z_mm();
    let z_reduction_mm = (max_z_before_mm - max_z_after_mm).max(0.0);

    Ok(GravitySettleResult {
        settled_count: dynamic_parts.len().saturating_sub(rolled_back_count),
        moved_count,
        rolled_back_count,
        max_z_before_mm,
        max_z_after_mm,
        z_reduction_mm,
        steps_run,
        stopped_early,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::collision;
    use crate::model::{
        MachineProfile, NoBuildZone, PartInstance, transform_from_translation_euler_deg,
    };

    fn inst_ref(job: &BuildJob, id: InstanceId) -> Option<&PartInstance> {
        job.instances.iter().find(|i| i.id == id)
    }

    fn inst_mut(job: &mut BuildJob, id: InstanceId) -> Option<&mut PartInstance> {
        job.instances.iter_mut().find(|i| i.id == id)
    }

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
        let pos = vec![
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
        Arc::new(TriMesh::from_indexed(pos, idx).expect("valid cup"))
    }

    #[test]
    fn floating_parts_settle_downward_and_preserve_clearance() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.clearance_mm = 3.0;
        let b = box_mesh([0.0, 0.0, 0.0], [20.0, 20.0, 15.0]);
        let pid = job.add_part("Box", b);
        let id1 = job
            .add_instance(pid, transform_from_translation_euler_deg([40.0, 40.0, 20.0], [0.0, 0.0, 0.0]))
            .expect("i1");
        let id2 = job
            .add_instance(pid, transform_from_translation_euler_deg([40.0, 40.0, 55.0], [0.0, 0.0, 0.0]))
            .expect("i2");

        let cfg = GravitySettleConfig {
            min_distance_mm: 3.0,
            wall_margin_mm: 2.0,
            min_z_mm: 2.0,
            ..GravitySettleConfig::default()
        };
        let res = gravity_settle(&mut job, &cfg, None, None).expect("settle");
        assert!(res.z_reduction_mm > 15.0, "expected large Z drop: {res:?}");
        assert!(inst_ref(&job, id1).expect("i1").transform[2][3] < 4.0);
        assert!(inst_ref(&job, id2).expect("i2").transform[2][3] >= 17.5);
        let col = collision::detect_collisions(&job, None, 3.0);
        assert!(col.pairs.is_empty(), "must preserve >= 3mm clearance: {:?}", col.pairs);
    }

    #[test]
    fn vhacd_allows_small_part_to_drop_into_open_cup_cavity() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.clearance_mm = 2.0;
        let cup_pid = job.add_part("Cup", open_cup_mesh([50.0, 50.0, 35.0], 5.0));
        let peg_pid = job.add_part("Peg", box_mesh([0.0, 0.0, 0.0], [16.0, 16.0, 14.0]));
        let cup_id = job
            .add_instance(cup_pid, transform_from_translation_euler_deg([25.0, 25.0, 0.0], [0.0, 0.0, 0.0]))
            .expect("cup");
        // Lock the cup so only the peg drops into its hollow cavity.
        inst_mut(&mut job, cup_id).expect("cup").locked = true;
        let peg_id = job
            .add_instance(peg_pid, transform_from_translation_euler_deg([42.0, 42.0, 50.0], [0.0, 0.0, 0.0]))
            .expect("peg");

        let cfg = GravitySettleConfig {
            min_distance_mm: 2.0,
            wall_margin_mm: 2.0,
            min_z_mm: 0.0,
            max_steps: 240,
            ..GravitySettleConfig::default()
        };
        let res = gravity_settle(&mut job, &cfg, None, None).expect("settle");
        let peg_z = inst_ref(&job, peg_id).expect("peg").transform[2][3];
        // Cup top rim is Z = 35mm, floor inside is Z = 5mm + 2mm clearance = 7mm; peg must drop well below Z = 35mm.
        assert!(
            peg_z < 20.0,
            "peg should fall inside open cup cavity, got z={peg_z}, res={res:?}"
        );
        let col = collision::detect_collisions(&job, None, 2.0);
        assert!(col.pairs.is_empty(), "peg in cavity must keep 2mm clearance: {:?}", col.pairs);
    }

    #[test]
    fn micro_rotation_tilt_cap_clamps_within_max_tilt_deg() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.clearance_mm = 1.5;
        let tall = box_mesh([-5.0, -5.0, 0.0], [5.0, 5.0, 30.0]);
        let pid = job.add_part("Tall", tall);
        let id = job
            .add_instance(pid, transform_from_translation_euler_deg([40.0, 40.0, 20.0], [0.0, 0.0, 0.0]))
            .expect("tall");
        // Nudge off-balance on top of another box edge so torque tries to tip it over.
        let step_pid = job.add_part("Step", box_mesh([0.0, 0.0, 0.0], [30.0, 30.0, 12.0]));
        let base_id = job
            .add_instance(
                step_pid,
                transform_from_translation_euler_deg([13.0, 25.0, 0.0], [0.0, 0.0, 0.0]),
            )
            .expect("step");
        inst_mut(&mut job, base_id).expect("step").locked = true;
        inst_mut(&mut job, id).expect("tall").constraint = RotationConstraint::Free;

        let max_tilt = 8.0_f32;
        let cfg = GravitySettleConfig {
            min_distance_mm: 1.5,
            allow_micro_rotations: true,
            max_tilt_deg: max_tilt,
            max_steps: 180,
            ..GravitySettleConfig::default()
        };
        let _ = gravity_settle(&mut job, &cfg, None, None).expect("settle");
        let q_after = mat3_to_quat(rotation_from_transform(&inst_ref(&job, id).expect("tall").transform));
        let angle_deg = Quat::IDENTITY.angle_between(q_after).to_degrees();
        assert!(
            angle_deg <= max_tilt + 0.5,
            "tilt {angle_deg} deg exceeded cap {max_tilt} deg"
        );
    }

    #[test]
    fn fixed_and_no_build_zone_obstacles_are_respected() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.clearance_mm = 2.0;
        let nbz_mesh = box_mesh([25.0, 25.0, 0.0], [65.0, 65.0, 20.0]);
        let mut nbz = NoBuildZone::new("Zone", nbz_mesh);
        nbz.clearance_mm = 3.0;
        job.no_build_zone = Some(nbz);

        let part_pid = job.add_part("Part", box_mesh([0.0, 0.0, 0.0], [16.0, 16.0, 16.0]));
        let free_id = job
            .add_instance(part_pid, transform_from_translation_euler_deg([37.0, 37.0, 45.0], [0.0, 0.0, 0.0]))
            .expect("free");
        let fixed_id = job
            .add_instance(part_pid, transform_from_translation_euler_deg([80.0, 80.0, 25.0], [0.0, 0.0, 0.0]))
            .expect("fixed");
        inst_mut(&mut job, fixed_id).expect("fixed").locked = true;

        let cfg = GravitySettleConfig {
            min_distance_mm: 2.0,
            avoid_no_build_zone: true,
            ..GravitySettleConfig::default()
        };
        let _ = gravity_settle(&mut job, &cfg, None, None).expect("settle");
        assert_eq!(
            inst_ref(&job, fixed_id).expect("fixed").transform[2][3],
            25.0,
            "locked part must not move"
        );
        let free_z = inst_ref(&job, free_id).expect("free").transform[2][3];
        assert!(
            free_z >= 22.5,
            "free part must rest above NoBuildZone + clearance (expected >= 22.5, got {free_z})"
        );
    }

    #[test]
    fn cancel_and_stop_and_apply_behave_cleanly() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("Box", box_mesh([0.0, 0.0, 0.0], [20.0, 20.0, 20.0]));
        let _ = job
            .add_instance(pid, transform_from_translation_euler_deg([40.0, 40.0, 60.0], [0.0, 0.0, 0.0]))
            .expect("b");

        let stop = AtomicBool::new(true);
        let res_stop = gravity_settle_with_callbacks(
            &mut job,
            &GravitySettleConfig::default(),
            None,
            None,
            None,
            Some(&stop),
            None,
        )
        .expect("stop_and_apply returns Ok");
        assert!(res_stop.stopped_early);
        assert_eq!(res_stop.steps_run, 0);

        let cancel = AtomicBool::new(true);
        let res_cancel = gravity_settle_with_callbacks(
            &mut job,
            &GravitySettleConfig::default(),
            None,
            None,
            Some(&cancel),
            None,
            None,
        );
        assert!(res_cancel.is_err());
    }
}
