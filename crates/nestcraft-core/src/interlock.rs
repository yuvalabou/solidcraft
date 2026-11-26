//! Topological interlocking detection & sequential peeling verification for 3D SLS builds.
//!
//! Open-cavity nesting (e.g. placing a small part inside an open bracket/cup) is **allowed**,
//! while topologically trapped configurations (e.g. linked rings or enclosed cages where no
//! rigid translational escape path exists) are rejected or reported.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::AtomicBool;

use crate::model::{Aabb, BuildJob, InstanceId, aabbs_overlap};
use crate::voxel::{BitGrid3D, is_cancelled, rotation_from_transform, voxelize_mesh_flood_fill};
use crate::{NestCraftError, Result};

/// Integer voxel-space bounding box `[min_inclusive, max_exclusive]`.
pub type VoxelAabb = [[usize; 3]; 2];

/// Configuration for translational escape / interlocking checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterlockConfig {
    /// Maximum states explored by the bounded 6-connected voxel BFS fallback.
    pub max_bfs_states: usize,
    /// Include 8 diagonal `(±1, ±1, ±1)` straight sweeps alongside the 6 cardinal axes.
    pub use_diagonals: bool,
}

impl Default for InterlockConfig {
    fn default() -> Self {
        Self {
            max_bfs_states: 4096,
            use_diagonals: true,
        }
    }
}

/// Diagnostic summary returned by [`detect_interlocking`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterlockReport {
    /// Instances that could not be peeled/extracted without colliding.
    pub trapped_ids: HashSet<InstanceId>,
    /// Groups of mutually interlocking instance IDs (one entry per trapped component).
    pub trapped_clusters: Vec<Vec<InstanceId>>,
    /// Valid sequential disassembly/depowdering order for all non-trapped instances.
    pub peeling_order: Vec<InstanceId>,
    /// Number of multi-part AABB-overlapping components voxelized and verified.
    pub checked_components: usize,
}

/// One placed solid voxel mask for sequential peeling verification.
#[derive(Debug, Clone)]
pub struct PlacedSolidVoxel {
    pub id: InstanceId,
    pub solid: BitGrid3D,
    pub origin: [usize; 3],
    pub world_aabb: VoxelAabb,
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    if is_cancelled(cancel) {
        Err(NestCraftError::InvalidInput("interlocking check cancelled".to_owned()))
    } else {
        Ok(())
    }
}

/// Return the tight occupied `[min_inclusive, max_exclusive]` voxel bounds inside `mask`.
pub fn tight_solid_bounds(mask: &BitGrid3D) -> Option<VoxelAabb> {
    let mut lo = [usize::MAX; 3];
    let mut hi = [0_usize; 3];
    let mut any = false;
    for iz in 0..mask.nz {
        for iy in 0..mask.ny {
            let row = (iz * mask.ny + iy) * mask.words_x;
            for wx in 0..mask.words_x {
                let w = mask.words[row + wx];
                if w == 0 {
                    continue;
                }
                any = true;
                lo[1] = lo[1].min(iy);
                hi[1] = hi[1].max(iy + 1);
                lo[2] = lo[2].min(iz);
                hi[2] = hi[2].max(iz + 1);
                let first_bit = w.trailing_zeros() as usize;
                let last_bit = 63 - (w.leading_zeros() as usize);
                lo[0] = lo[0].min(wx * 64 + first_bit);
                hi[0] = hi[0].max((wx * 64 + last_bit + 1).min(mask.nx));
            }
        }
    }
    any.then_some([lo, hi])
}

/// Crop `mask` tightly to `bounds = [min_inclusive, max_exclusive]`.
pub fn crop_bitgrid(mask: &BitGrid3D, bounds: VoxelAabb, max_bytes: Option<usize>) -> Result<BitGrid3D> {
    let [lo, hi] = bounds;
    let nx = hi[0].saturating_sub(lo[0]).max(1);
    let ny = hi[1].saturating_sub(lo[1]).max(1);
    let nz = hi[2].saturating_sub(lo[2]).max(1);
    let mut out = BitGrid3D::new(nx, ny, nz, max_bytes)?;
    for iz in 0..nz {
        for iy in 0..ny {
            for ix in 0..nx {
                if mask.get(lo[0] + ix, lo[1] + iy, lo[2] + iz) {
                    out.set(ix, iy, iz);
                }
            }
        }
    }
    Ok(out)
}

/// Bitwise overlap test allowing signed offsets (voxels outside `tray` bounds count as free air).
pub fn overlaps_at_signed(tray: &BitGrid3D, mask: &BitGrid3D, ox: isize, oy: isize, oz: isize) -> bool {
    let z_start = (-oz).max(0) as usize;
    let z_end = ((tray.nz as isize) - oz).clamp(0, mask.nz as isize) as usize;
    let y_start = (-oy).max(0) as usize;
    let y_end = ((tray.ny as isize) - oy).clamp(0, mask.ny as isize) as usize;
    if z_start >= z_end || y_start >= y_end {
        return false;
    }

    if ox >= 0 {
        let oxu = ox as usize;
        if oxu >= tray.nx {
            return false;
        }
        let word_off = oxu >> 6;
        let bit_shift = (oxu & 63) as u32;
        for iz in z_start..z_end {
            let tz = (oz + iz as isize) as usize;
            let sz_off = tz * tray.ny;
            let mz_off = iz * mask.ny;
            for iy in y_start..y_end {
                let ty = (oy + iy as isize) as usize;
                let s_row = (sz_off + ty) * tray.words_x;
                let m_row = (mz_off + iy) * mask.words_x;
                for wx in 0..mask.words_x {
                    let mw = mask.words[m_row + wx];
                    if mw == 0 {
                        continue;
                    }
                    let dst = word_off + wx;
                    if dst >= tray.words_x {
                        break;
                    }
                    if (tray.words[s_row + dst] & (mw << bit_shift)) != 0 {
                        return true;
                    }
                    if bit_shift != 0 {
                        let hi = mw >> (64 - bit_shift);
                        if hi != 0 && dst + 1 < tray.words_x && (tray.words[s_row + dst + 1] & hi) != 0 {
                            return true;
                        }
                    }
                }
            }
        }
    } else {
        let neg = (-ox) as usize;
        if neg >= mask.nx {
            return false;
        }
        let src_skip = neg >> 6;
        let rshift = (neg & 63) as u32;
        for iz in z_start..z_end {
            let tz = (oz + iz as isize) as usize;
            let sz_off = tz * tray.ny;
            let mz_off = iz * mask.ny;
            for iy in y_start..y_end {
                let ty = (oy + iy as isize) as usize;
                let s_row = (sz_off + ty) * tray.words_x;
                let m_row = (mz_off + iy) * mask.words_x;
                for wx in src_skip..mask.words_x {
                    let dst = wx - src_skip;
                    if dst >= tray.words_x {
                        break;
                    }
                    let mut combined = mask.words[m_row + wx] >> rshift;
                    if rshift != 0 && wx + 1 < mask.words_x {
                        combined |= mask.words[m_row + wx + 1] << (64 - rshift);
                    }
                    if combined != 0 && (tray.words[s_row + dst] & combined) != 0 {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[inline]
fn voxel_aabbs_overlap(a: VoxelAabb, b: VoxelAabb) -> bool {
    a[0][0] < b[1][0]
        && a[1][0] > b[0][0]
        && a[0][1] < b[1][1]
        && a[1][1] > b[0][1]
        && a[0][2] < b[1][2]
        && a[1][2] > b[0][2]
}

#[inline]
fn shifted_aabb_outside_box(part: VoxelAabb, delta: [isize; 3], escape_box: VoxelAabb) -> bool {
    for k in 0..3 {
        let lo = part[0][k] as isize + delta[k];
        let hi = part[1][k] as isize + delta[k];
        if hi <= escape_box[0][k] as isize || lo >= escape_box[1][k] as isize {
            return true;
        }
    }
    false
}

/// Check if `part_solid` placed at `origin` inside `tray_solid` has a collision-free
/// translational escape path away from overlapping neighbors (`existing_aabbs`).
///
/// Open-cavity placements (like sitting inside an open cup) return `Ok(true)`, while
/// topologically trapped placements (linked rings or enclosed cages) return `Ok(false)`.
pub fn can_part_escape_tray(
    tray_solid: &BitGrid3D,
    part_solid: &BitGrid3D,
    origin: [usize; 3],
    existing_aabbs: &[VoxelAabb],
    cfg: &InterlockConfig,
    cancel: Option<&AtomicBool>,
) -> Result<bool> {
    let Some(tight) = tight_solid_bounds(part_solid) else {
        return Ok(true);
    };
    let part_world: VoxelAabb = [
        [origin[0] + tight[0][0], origin[1] + tight[0][1], origin[2] + tight[0][2]],
        [origin[0] + tight[1][0], origin[1] + tight[1][1], origin[2] + tight[1][2]],
    ];

    // Tier 0: AABB overlap pre-filter.
    let escape_box = if existing_aabbs.is_empty() {
        [[0, 0, 0], [tray_solid.nx, tray_solid.ny, tray_solid.nz]]
    } else {
        let mut any_overlap = false;
        let mut union_lo = [usize::MAX; 3];
        let mut union_hi = [0_usize; 3];
        for &nb in existing_aabbs {
            if voxel_aabbs_overlap(part_world, nb) {
                any_overlap = true;
                for k in 0..3 {
                    union_lo[k] = union_lo[k].min(nb[0][k]);
                    union_hi[k] = union_hi[k].max(nb[1][k]);
                }
            }
        }
        if !any_overlap {
            return Ok(true);
        }
        [union_lo, union_hi]
    };

    // Tier 1: Straight-line ray sweeps (+Z first, then cardinals, then optional 8 diagonals).
    let cardinals: [[isize; 3]; 6] = [
        [0, 0, 1],
        [1, 0, 0],
        [-1, 0, 0],
        [0, 1, 0],
        [0, -1, 0],
        [0, 0, -1],
    ];
    for dir in cardinals {
        if straight_sweep_escapes(tray_solid, part_solid, origin, part_world, escape_box, dir) {
            return Ok(true);
        }
    }
    if cfg.use_diagonals {
        let diagonals: [[isize; 3]; 8] = [
            [1, 1, 1],
            [-1, 1, 1],
            [1, -1, 1],
            [-1, -1, 1],
            [1, 1, -1],
            [-1, 1, -1],
            [1, -1, -1],
            [-1, -1, -1],
        ];
        for dir in diagonals {
            if straight_sweep_escapes(tray_solid, part_solid, origin, part_world, escape_box, dir) {
                return Ok(true);
            }
        }
    }

    // Tier 2: Bounded 6-connected translational BFS (handles L-shaped / curved pockets).
    if cfg.max_bfs_states == 0 {
        return Ok(false);
    }
    let mut visited: HashSet<[i32; 3]> = HashSet::with_capacity(cfg.max_bfs_states.min(1024));
    let mut queue: VecDeque<[i32; 3]> = VecDeque::with_capacity(256);
    visited.insert([0, 0, 0]);
    queue.push_back([0, 0, 0]);

    let mut pops = 0_usize;
    while let Some(cur) = queue.pop_front() {
        pops += 1;
        if (pops & 255) == 0 {
            check_cancel(cancel)?;
        }
        if pops > cfg.max_bfs_states {
            break;
        }

        for dir in cardinals {
            let next = [
                cur[0] + dir[0] as i32,
                cur[1] + dir[1] as i32,
                cur[2] + dir[2] as i32,
            ];
            if visited.contains(&next) {
                continue;
            }
            let delta = [next[0] as isize, next[1] as isize, next[2] as isize];
            let ox = origin[0] as isize + delta[0];
            let oy = origin[1] as isize + delta[1];
            let oz = origin[2] as isize + delta[2];
            if overlaps_at_signed(tray_solid, part_solid, ox, oy, oz) {
                continue;
            }
            if shifted_aabb_outside_box(part_world, delta, escape_box) {
                return Ok(true);
            }
            if visited.len() < cfg.max_bfs_states {
                visited.insert(next);
                queue.push_back(next);
            }
        }
    }

    Ok(false)
}

fn straight_sweep_escapes(
    tray_solid: &BitGrid3D,
    part_solid: &BitGrid3D,
    origin: [usize; 3],
    part_world: VoxelAabb,
    escape_box: VoxelAabb,
    dir: [isize; 3],
) -> bool {
    let mut step = 1_isize;
    loop {
        let delta = [dir[0] * step, dir[1] * step, dir[2] * step];
        let ox = origin[0] as isize + delta[0];
        let oy = origin[1] as isize + delta[1];
        let oz = origin[2] as isize + delta[2];
        if overlaps_at_signed(tray_solid, part_solid, ox, oy, oz) {
            return false;
        }
        if shifted_aabb_outside_box(part_world, delta, escape_box) {
            return true;
        }
        step += 1;
    }
}

/// Verify whether a full placement admits a valid sequential translational peeling order.
///
/// Returns `(peeling_order, trapped_ids)`.
pub fn verify_peeling_order(
    tray_dims: [usize; 3],
    placed: &[PlacedSolidVoxel],
    cfg: &InterlockConfig,
    max_bytes: Option<usize>,
    cancel: Option<&AtomicBool>,
) -> Result<(Vec<InstanceId>, Vec<InstanceId>)> {
    if placed.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut tray_solid = BitGrid3D::new(tray_dims[0], tray_dims[1], tray_dims[2], max_bytes)?;
    for p in placed {
        tray_solid.stamp_at(&p.solid, p.origin[0], p.origin[1], p.origin[2]);
    }

    // Peel top-down (`max_z` descending) so upper caps un-nest before lower parts.
    let mut remaining: Vec<usize> = (0..placed.len()).collect();
    remaining.sort_by(|&a, &b| placed[b].world_aabb[1][2].cmp(&placed[a].world_aabb[1][2]));

    let mut peeling_order = Vec::with_capacity(placed.len());
    let mut neighbor_aabbs = Vec::with_capacity(placed.len());

    loop {
        check_cancel(cancel)?;
        let mut peeled_any = false;
        let mut idx = 0_usize;
        while idx < remaining.len() {
            check_cancel(cancel)?;
            let item_i = remaining[idx];
            let p = &placed[item_i];

            neighbor_aabbs.clear();
            for &other_i in &remaining {
                if other_i != item_i {
                    neighbor_aabbs.push(placed[other_i].world_aabb);
                }
            }

            tray_solid.unstamp_at(&p.solid, p.origin[0], p.origin[1], p.origin[2]);
            let escaped = can_part_escape_tray(
                &tray_solid,
                &p.solid,
                p.origin,
                &neighbor_aabbs,
                cfg,
                cancel,
            )?;
            if escaped {
                peeling_order.push(p.id);
                remaining.remove(idx);
                peeled_any = true;
            } else {
                tray_solid.stamp_at(&p.solid, p.origin[0], p.origin[1], p.origin[2]);
                idx += 1;
            }
        }
        if !peeled_any || remaining.is_empty() {
            break;
        }
    }

    let trapped_ids = remaining.into_iter().map(|i| placed[i].id).collect();
    Ok((peeling_order, trapped_ids))
}

/// Run a standalone interlocking diagnostic across all visible instances of a [`BuildJob`].
///
/// Groups instances into AABB-overlapping connected components first so isolated parts
/// pass in $O(1)$ without voxelization.
pub fn detect_interlocking(
    job: &BuildJob,
    pitch_mm: f32,
    cfg: &InterlockConfig,
    max_bytes: Option<usize>,
    cancel: Option<&AtomicBool>,
) -> Result<InterlockReport> {
    let pitch = pitch_mm.clamp(0.25, 10.0);
    struct ActiveInst<'a> {
        id: InstanceId,
        mesh: &'a crate::model::TriMesh,
        transform: crate::model::Transform3x4,
        world_bounds: Aabb,
    }

    let mut active = Vec::new();
    for inst in &job.instances {
        if !job.is_instance_visible(inst) {
            continue;
        }
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        active.push(ActiveInst {
            id: inst.id,
            mesh: &proto.mesh,
            transform: inst.transform,
            world_bounds: inst.world_bounds(proto.mesh.bounds),
        });
    }

    let n = active.len();
    let mut visited = vec![false; n];
    let mut trapped_ids = HashSet::new();
    let mut trapped_clusters = Vec::new();
    let mut peeling_order = Vec::with_capacity(n);
    let mut checked_components = 0_usize;

    for start in 0..n {
        if visited[start] {
            continue;
        }
        check_cancel(cancel)?;
        let mut comp = Vec::new();
        let mut queue = VecDeque::new();
        visited[start] = true;
        queue.push_back(start);
        while let Some(u) = queue.pop_front() {
            comp.push(u);
            for v in 0..n {
                if !visited[v] && aabbs_overlap(active[u].world_bounds, active[v].world_bounds) {
                    visited[v] = true;
                    queue.push_back(v);
                }
            }
        }

        if comp.len() == 1 {
            peeling_order.push(active[comp[0]].id);
            continue;
        }

        checked_components += 1;
        let mut comp_min = [f32::INFINITY; 3];
        for &idx in &comp {
            for (k, min_k) in comp_min.iter_mut().enumerate() {
                *min_k = (*min_k).min(active[idx].world_bounds[0][k]);
            }
        }

        let mut placed = Vec::with_capacity(comp.len());
        let mut tray_dims = [1_usize; 3];
        for &idx in &comp {
            check_cancel(cancel)?;
            let item = &active[idx];
            let rot = rotation_from_transform(&item.transform);
            let (solid, grid_origin, _) =
                voxelize_mesh_flood_fill(item.mesh, &rot, pitch, [0, 0, 0], false, max_bytes, cancel)?;
            let world_origin = [
                grid_origin[0] + item.transform[0][3],
                grid_origin[1] + item.transform[1][3],
                grid_origin[2] + item.transform[2][3],
            ];
            let origin = [
                ((world_origin[0] - comp_min[0]) / pitch).round().max(0.0) as usize,
                ((world_origin[1] - comp_min[1]) / pitch).round().max(0.0) as usize,
                ((world_origin[2] - comp_min[2]) / pitch).round().max(0.0) as usize,
            ];
            let bounds = tight_solid_bounds(&solid).unwrap_or([[0, 0, 0], [solid.nx, solid.ny, solid.nz]]);
            let world_aabb = [
                [origin[0] + bounds[0][0], origin[1] + bounds[0][1], origin[2] + bounds[0][2]],
                [origin[0] + bounds[1][0], origin[1] + bounds[1][1], origin[2] + bounds[1][2]],
            ];
            for k in 0..3 {
                tray_dims[k] = tray_dims[k].max(origin[k] + [solid.nx, solid.ny, solid.nz][k]);
            }
            placed.push(PlacedSolidVoxel {
                id: item.id,
                solid,
                origin,
                world_aabb,
            });
        }

        let (comp_peel, comp_trapped) =
            verify_peeling_order(tray_dims, &placed, cfg, max_bytes, cancel)?;
        peeling_order.extend(comp_peel);
        if !comp_trapped.is_empty() {
            for &tid in &comp_trapped {
                trapped_ids.insert(tid);
            }
            trapped_clusters.push(comp_trapped);
        }
    }

    Ok(InterlockReport {
        trapped_ids,
        trapped_clusters,
        peeling_order,
        checked_components,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::model::{IDENTITY_TRANSFORM, MachineProfile, TriMesh, transform_from_translation_euler_deg};

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
            [0, 2, 1], [0, 3, 2],
            [4, 5, 6], [4, 6, 7],
            [0, 1, 5], [0, 5, 4],
            [1, 2, 6], [1, 6, 5],
            [2, 3, 7], [2, 7, 6],
            [3, 0, 4], [3, 4, 7],
        ];
        Arc::new(TriMesh::from_indexed(positions, indices).expect("valid box"))
    }

    fn open_cup_mesh() -> Arc<TriMesh> {
        // Outer [0..24, 0..24, 0..20], inner cavity [4..20, 4..20, 4..20] open at +Z.
        let mut pos = vec![
            [0.0, 0.0, 0.0], [24.0, 0.0, 0.0], [24.0, 24.0, 0.0], [0.0, 24.0, 0.0],
            [0.0, 0.0, 20.0], [24.0, 0.0, 20.0], [24.0, 24.0, 20.0], [0.0, 24.0, 20.0],
            [4.0, 4.0, 4.0], [20.0, 4.0, 4.0], [20.0, 20.0, 4.0], [4.0, 20.0, 4.0],
            [4.0, 4.0, 20.0], [20.0, 4.0, 20.0], [20.0, 20.0, 20.0], [4.0, 20.0, 20.0],
        ];
        let idx = vec![
            [0, 2, 1], [0, 3, 2],
            [0, 1, 5], [0, 5, 4],
            [1, 2, 6], [1, 6, 5],
            [2, 3, 7], [2, 7, 6],
            [3, 0, 4], [3, 4, 7],
            [8, 9, 10], [8, 10, 11],
            [8, 13, 9], [8, 12, 13],
            [9, 14, 10], [9, 13, 14],
            [10, 15, 11], [10, 14, 15],
            [11, 12, 8], [11, 15, 12],
            [4, 5, 13], [4, 13, 12],
            [5, 6, 14], [5, 14, 13],
            [6, 7, 15], [6, 15, 14],
            [7, 4, 12], [7, 12, 15],
        ];
        pos.shrink_to_fit();
        Arc::new(TriMesh::from_indexed(pos, idx).expect("valid cup"))
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
    fn open_cup_with_small_part_inside_is_not_interlocked() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let cup_pid = job.add_part("Cup", open_cup_mesh());
        let inner_pid = job.add_part("InnerCube", box_mesh([0.0, 0.0, 0.0], [8.0, 8.0, 8.0]));
        let _ = job.add_instance(cup_pid, IDENTITY_TRANSFORM).expect("cup");
        let _ = job
            .add_instance(inner_pid, transform_from_translation_euler_deg([8.0, 8.0, 7.0], [0.0, 0.0, 0.0]))
            .expect("inner");

        let report = detect_interlocking(&job, 1.5, &InterlockConfig::default(), None, None).expect("report");
        assert!(report.trapped_ids.is_empty(), "open-cup cavity nesting must be allowed");
        assert_eq!(report.checked_components, 1);
        assert_eq!(report.peeling_order.len(), 2);
    }

    #[test]
    fn linked_tori_rings_are_detected_as_interlocked_while_unlinked_pass() {
        let ring = torus_mesh(20.0, 3.0, 28, 12);

        // Chain-linked: ring B rotated 90° around X and centered on ring A's tube cross-section.
        let mut linked = BuildJob::new(MachineProfile::FormigaP110);
        let p_ring = linked.add_part("Ring", ring.clone());
        let a = linked
            .add_instance(p_ring, transform_from_translation_euler_deg([50.0, 50.0, 30.0], [0.0, 0.0, 0.0]))
            .expect("ring a");
        let b = linked
            .add_instance(p_ring, transform_from_translation_euler_deg([70.0, 50.0, 30.0], [90.0, 0.0, 0.0]))
            .expect("ring b");
        let rep_linked =
            detect_interlocking(&linked, 1.5, &InterlockConfig::default(), None, None).expect("linked");
        assert!(rep_linked.trapped_ids.contains(&a) && rep_linked.trapped_ids.contains(&b));
        assert_eq!(rep_linked.trapped_clusters.len(), 1);

        // Unlinked: ring B translated past A's outer radius so AABBs still overlap slightly or nearby.
        let mut unlinked = BuildJob::new(MachineProfile::FormigaP110);
        let p_ring2 = unlinked.add_part("Ring", ring);
        let _ = unlinked
            .add_instance(p_ring2, transform_from_translation_euler_deg([50.0, 50.0, 30.0], [0.0, 0.0, 0.0]))
            .expect("a");
        let _ = unlinked
            .add_instance(p_ring2, transform_from_translation_euler_deg([96.0, 50.0, 30.0], [90.0, 0.0, 0.0]))
            .expect("b");
        let rep_unlinked =
            detect_interlocking(&unlinked, 1.5, &InterlockConfig::default(), None, None).expect("unlinked");
        assert!(rep_unlinked.trapped_ids.is_empty());
    }

    #[test]
    fn cube_trapped_inside_narrow_window_cage_is_detected_as_interlocked() {
        // Hollow box [0..20, 0..20, 0..20] built out of 6 outer walls with a small 4x4 window on +Z.
        let mut tray = BitGrid3D::new(24, 24, 24, None).expect("tray");
        let mut cage_solid = BitGrid3D::new(20, 20, 20, None).expect("cage");
        for z in 0..20 {
            for y in 0..20 {
                for x in 0..20 {
                    let on_wall = !(2..18).contains(&x) || !(2..18).contains(&y) || !(2..18).contains(&z);
                    let is_small_window = z >= 18 && (8..12).contains(&x) && (8..12).contains(&y);
                    if on_wall && !is_small_window {
                        cage_solid.set(x, y, z);
                    }
                }
            }
        }
        tray.stamp_at(&cage_solid, 2, 2, 2);

        // Inner 8x8x8 cube cannot fit through the 4x4 window -> trapped!
        let mut inner = BitGrid3D::new(8, 8, 8, None).expect("inner");
        for z in 0..8 {
            for y in 0..8 {
                for x in 0..8 {
                    inner.set(x, y, z);
                }
            }
        }
        let cage_aabb = [[2, 2, 2], [22, 22, 22]];
        assert!(
            !can_part_escape_tray(&tray, &inner, [8, 8, 8], &[cage_aabb], &InterlockConfig::default(), None)
                .expect("checked"),
            "8x8x8 cube inside 4x4-window cage must be detected as interlocked"
        );
    }

    #[test]
    fn l_pocket_part_escapes_via_multi_step_bfs() {
        // Channel that blocks +Z directly above the part, requiring sideways -X slide first then +Z lift!
        let mut tray = BitGrid3D::new(28, 16, 20, None).expect("tray");
        // Floor + overhanging roof blocking x >= 10 at z = 8..10, while x < 10 is open above!
        for x in 0..24 {
            for y in 2..14 {
                tray.set(x, y, 1);
            }
        }
        for x in 10..24 {
            for y in 2..14 {
                tray.set(x, y, 8);
                tray.set(x, y, 9);
            }
        }
        for y in 2..14 {
            for z in 2..8 {
                tray.set(22, y, z);
            }
        }
        // Part at x=12..16, y=5..9, z=3..6: blocked +Z (roof), +X (wall), -Z (floor), ±Y (assume side walls).
        for x in 10..24 {
            for z in 2..8 {
                tray.set(x, 2, z);
                tray.set(x, 13, z);
            }
        }

        let mut part = BitGrid3D::new(4, 4, 4, None).expect("part");
        for z in 0..4 {
            for y in 0..4 {
                for x in 0..4 {
                    part.set(x, y, z);
                }
            }
        }
        let blocker = [[0, 2, 1], [24, 14, 10]];
        assert!(
            can_part_escape_tray(&tray, &part, [12, 6, 3], &[blocker], &InterlockConfig::default(), None)
                .expect("bfs escape"),
            "part should slide left in -X out from under overhang and escape +Z"
        );
    }

    #[test]
    fn sequential_peeling_handles_stacked_cap_and_cancellation() {
        // Open cup (id 1) + inner block (id 2) + top lid cap (id 3) resting above the cup.
        // Inner block cannot escape until top lid cap is peeled first!
        let mut cup = BitGrid3D::new(12, 12, 10, None).expect("cup");
        for z in 0..10 {
            for y in 0..12 {
                for x in 0..12 {
                    if !(2..10).contains(&x) || !(2..10).contains(&y) || z < 2 {
                        cup.set(x, y, z);
                    }
                }
            }
        }
        let mut inner = BitGrid3D::new(4, 4, 4, None).expect("inner");
        let mut lid = BitGrid3D::new(12, 12, 2, None).expect("lid");
        for z in 0..4 {
            for y in 0..4 {
                for x in 0..4 {
                    inner.set(x, y, z);
                }
            }
        }
        for z in 0..2 {
            for y in 0..12 {
                for x in 0..12 {
                    lid.set(x, y, z);
                }
            }
        }

        let placed = vec![
            PlacedSolidVoxel {
                id: InstanceId(1),
                solid: cup,
                origin: [2, 2, 0],
                world_aabb: [[2, 2, 0], [14, 14, 10]],
            },
            PlacedSolidVoxel {
                id: InstanceId(2),
                solid: inner,
                origin: [6, 6, 3],
                world_aabb: [[6, 6, 3], [10, 10, 7]],
            },
            PlacedSolidVoxel {
                id: InstanceId(3),
                solid: lid,
                origin: [2, 2, 10],
                world_aabb: [[2, 2, 10], [14, 14, 12]],
            },
        ];

        let (order, trapped) =
            verify_peeling_order([16, 16, 16], &placed, &InterlockConfig::default(), None, None).expect("peel");
        assert!(trapped.is_empty());
        assert_eq!(order.len(), 3);
        // Lid (3) must peel before Inner (2).
        let pos_lid = order.iter().position(|&id| id == InstanceId(3)).expect("lid");
        let pos_inner = order.iter().position(|&id| id == InstanceId(2)).expect("inner");
        assert!(pos_lid < pos_inner, "lid must peel before inner block: {order:?}");

        // Cooperative cancellation aborts cleanly.
        let cancel = AtomicBool::new(true);
        assert!(verify_peeling_order([16, 16, 16], &placed, &InterlockConfig::default(), None, Some(&cancel)).is_err());
    }
}
