//! Pure-Rust 3D voxelization, Felzenszwalb-Huttenlocher 3D Euclidean Distance Transform (EDT),
//! `u64` bitpacked clearance masks, coarse-first rotation symmetry pruning, local-space `MeshBvh`,
//! and RAM/CPU resource guardrails for 3D nesting.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::model::{Aabb, RotationConstraint, Transform3x4, TriMesh, transform_point, tri_tri_closest_points};
use crate::{NestCraftError, Result};

const EDT_INF: f32 = 1e15;
const BVH_LEAF_MAX_TRIS: usize = 8;

/// Cooperative cancellation helper for worker loops.
#[inline]
pub fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    if is_cancelled(cancel) {
        Err(NestCraftError::InvalidInput("operation cancelled".to_owned()))
    } else {
        Ok(())
    }
}

/// CPU worker thread budget preset (spares cores so OS & 60 FPS viewport never hang).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CpuBudget {
    /// Use 50% of logical cores (minimum 1).
    Quiet,
    /// Use 75% of logical cores, always leaving at least 2 logical cores free (minimum 1).
    #[default]
    Balanced,
    /// Use all logical cores except 1 (minimum 1).
    Max,
    /// Explicit worker thread count (clamped to `1..=64`).
    Custom(usize),
}

impl CpuBudget {
    /// Resolve the concrete number of worker threads given host logical parallelism.
    pub fn resolve_workers(self) -> usize {
        let cores = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .max(1);
        self.resolve_for_cores(cores)
    }

    /// Resolve worker threads for a specified core count (deterministic helper for tests).
    pub fn resolve_for_cores(self, cores: usize) -> usize {
        let cores = cores.max(1);
        match self {
            Self::Quiet => (cores / 2).max(1),
            Self::Balanced => ((cores * 3) / 4).min(cores.saturating_sub(2)).max(1),
            Self::Max => cores.saturating_sub(1).max(1),
            Self::Custom(n) => n.clamp(1, 64),
        }
    }
}

/// Estimated peak memory breakdown (bytes) before allocating voxel/EDT structures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryEstimate {
    /// Bitgrid + heightmap memory across worker tray instances.
    pub tray_bytes: usize,
    /// Reusable `f32` EDT scratch memory across preprocessing workers.
    pub edt_scratch_bytes: usize,
    /// Packed `u64` orientation mask cache across unique part prototypes.
    pub mask_cache_bytes: usize,
    /// Estimated total peak RAM in bytes.
    pub total_bytes: usize,
}

/// Fallible zero-initialized allocation guarded by `max_bytes` and `Vec::try_reserve_exact`.
pub fn try_alloc_zeroed<T: Copy + Default>(len: usize, max_bytes: Option<usize>) -> Result<Vec<T>> {
    let elem_bytes = std::mem::size_of::<T>();
    let total_bytes = len
        .checked_mul(elem_bytes)
        .ok_or_else(|| NestCraftError::InvalidInput("voxel buffer size overflow".to_owned()))?;
    if let Some(limit) = max_bytes
        && total_bytes > limit
    {
        return Err(NestCraftError::InvalidInput(format!(
            "voxel allocation ({total_bytes} bytes) exceeds memory limit ({limit} bytes)"
        )));
    }
    let mut vec = Vec::new();
    vec.try_reserve_exact(len).map_err(|_| {
        NestCraftError::InvalidInput(format!(
            "out of memory allocating {total_bytes} bytes for voxel buffer"
        ))
    })?;
    vec.resize(len, T::default());
    Ok(vec)
}

/// Estimate peak memory (bytes) for 3D voxel nesting before allocating.
///
/// `unique_part_boxes` contains `(part_local_dims_mm, orientation_count)` for each unique `PartId`.
pub fn estimate_voxel_memory_bytes(
    tray_dims: [usize; 3],
    unique_part_boxes: &[([f32; 3], usize)],
    resolution_mm: f32,
    max_clearance_mm: f32,
    workers: usize,
) -> MemoryEstimate {
    let pitch = resolution_mm.clamp(0.25, 10.0);
    let workers = workers.max(1);

    let nx = tray_dims[0].max(1);
    let ny = tray_dims[1].max(1);
    let nz = tray_dims[2].max(1);
    let words_x = nx.div_ceil(64);
    let single_tray_bytes = words_x
        .saturating_mul(ny)
        .saturating_mul(nz)
        .saturating_mul(8);
    let heightmap_bytes = nx.saturating_mul(ny).saturating_mul(2);
    // Solid + clearance tray per worker + 1 shared base_tray.
    let tray_bytes = single_tray_bytes
        .saturating_mul(2 * workers + 1)
        .saturating_add(heightmap_bytes.saturating_mul(workers + 1));

    let pad = (max_clearance_mm.max(0.0) / pitch).ceil() as usize + 2;
    let mut max_part_voxels: usize = 0;
    let mut mask_cache_bytes: usize = 0;

    for &(dims, orient_count) in unique_part_boxes {
        let px = ((dims[0].max(pitch) / pitch).ceil() as usize).saturating_add(2 * pad);
        let py = ((dims[1].max(pitch) / pitch).ceil() as usize).saturating_add(2 * pad);
        let pz = ((dims[2].max(pitch) / pitch).ceil() as usize).saturating_add(2 * pad);
        let voxels = px.saturating_mul(py).saturating_mul(pz);
        max_part_voxels = max_part_voxels.max(voxels);

        let mask_words = px.div_ceil(64).saturating_mul(py).saturating_mul(pz);
        // solid_mask + clearance_mask per orientation.
        let one_orient_bytes = mask_words.saturating_mul(16);
        mask_cache_bytes = mask_cache_bytes.saturating_add(one_orient_bytes.saturating_mul(orient_count.max(1)));
    }

    // f32 distance field + work arrays per worker during preprocessing (released after packing).
    let edt_workers = workers.min(unique_part_boxes.len().max(1));
    let edt_scratch_bytes = max_part_voxels
        .saturating_mul(std::mem::size_of::<f32>())
        .saturating_mul(edt_workers);

    let total_bytes = tray_bytes.saturating_add(edt_scratch_bytes.max(mask_cache_bytes));
    MemoryEstimate {
        tray_bytes,
        edt_scratch_bytes,
        mask_cache_bytes,
        total_bytes,
    }
}

/// Auto-fit the finest `resolution_mm >= requested_res_mm` (up to `4.0 mm`) whose estimated
/// peak RAM stays within `max_memory_bytes`.
pub fn auto_fit_resolution_mm(
    tray_span_mm: [f32; 3],
    unique_part_boxes: &[([f32; 3], usize)],
    requested_res_mm: f32,
    max_clearance_mm: f32,
    workers: usize,
    max_memory_bytes: usize,
) -> f32 {
    let mut res = requested_res_mm.clamp(0.5, 4.0);
    for _ in 0..32 {
        let dims = [
            (tray_span_mm[0].max(1.0) / res).ceil() as usize,
            (tray_span_mm[1].max(1.0) / res).ceil() as usize,
            (tray_span_mm[2].max(1.0) / res).ceil() as usize,
        ];
        let est = estimate_voxel_memory_bytes(dims, unique_part_boxes, res, max_clearance_mm, workers);
        if est.total_bytes <= max_memory_bytes || res >= 4.0 {
            return res;
        }
        res = (res + 0.1).min(4.0);
    }
    res
}

/// Bitpacked 3D binary voxel grid (`u64` words along X for fast bitwise `&` / `|` probing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitGrid3D {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub words_x: usize,
    pub words: Vec<u64>,
}

impl BitGrid3D {
    /// Allocate an empty `BitGrid3D` with optional memory-budget check.
    pub fn new(nx: usize, ny: usize, nz: usize, max_bytes: Option<usize>) -> Result<Self> {
        let nx = nx.max(1);
        let ny = ny.max(1);
        let nz = nz.max(1);
        let words_x = nx.div_ceil(64);
        let total_words = words_x
            .checked_mul(ny)
            .and_then(|v| v.checked_mul(nz))
            .ok_or_else(|| NestCraftError::InvalidInput("3D bitgrid dimension overflow".to_owned()))?;
        let words = try_alloc_zeroed::<u64>(total_words, max_bytes)?;
        Ok(Self {
            nx,
            ny,
            nz,
            words_x,
            words,
        })
    }

    /// Memory consumed by the `words` backing buffer in bytes.
    pub fn byte_len(&self) -> usize {
        self.words.len().saturating_mul(std::mem::size_of::<u64>())
    }

    /// Clear all voxels to `0`.
    pub fn clear_all(&mut self) {
        self.words.fill(0);
    }

    /// Read voxel occupancy at `(x, y, z)`. Returns `false` if out of bounds.
    #[inline]
    pub fn get(&self, x: usize, y: usize, z: usize) -> bool {
        if x >= self.nx || y >= self.ny || z >= self.nz {
            return false;
        }
        let idx = (z * self.ny + y) * self.words_x + (x >> 6);
        self.words.get(idx).is_some_and(|w| (w >> (x & 63)) & 1 != 0)
    }

    /// Set voxel `(x, y, z)` to `1`. No-op if out of bounds.
    #[inline]
    pub fn set(&mut self, x: usize, y: usize, z: usize) {
        if x >= self.nx || y >= self.ny || z >= self.nz {
            return;
        }
        let idx = (z * self.ny + y) * self.words_x + (x >> 6);
        if let Some(w) = self.words.get_mut(idx) {
            *w |= 1_u64 << (x & 63);
        }
    }

    /// Clear voxel `(x, y, z)` to `0`. No-op if out of bounds.
    #[inline]
    pub fn clear(&mut self, x: usize, y: usize, z: usize) {
        if x >= self.nx || y >= self.ny || z >= self.nz {
            return;
        }
        let idx = (z * self.ny + y) * self.words_x + (x >> 6);
        if let Some(w) = self.words.get_mut(idx) {
            *w &= !(1_u64 << (x & 63));
        }
    }

    /// Total number of `1` bits in the grid.
    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Fast 64-bit FNV-1a hash of dimensions and packed bits (for symmetry pruning).
    pub fn hash64(&self) -> u64 {
        let mut h = 0xcbf29ce484222325_u64;
        for v in [self.nx as u64, self.ny as u64, self.nz as u64] {
            h ^= v;
            h = h.wrapping_mul(0x100000001b3);
        }
        for &w in &self.words {
            h ^= w;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Return `true` if `mask` placed at `(ox, oy, oz)` goes out of bounds or shares any `1` bit with `self`.
    pub fn overlaps_at(&self, mask: &BitGrid3D, ox: usize, oy: usize, oz: usize) -> bool {
        if ox + mask.nx > self.nx || oy + mask.ny > self.ny || oz + mask.nz > self.nz {
            return true;
        }
        let word_off = ox >> 6;
        let bit_shift = (ox & 63) as u32;
        for iz in 0..mask.nz {
            let sz_off = (oz + iz) * self.ny;
            let mz_off = iz * mask.ny;
            for iy in 0..mask.ny {
                let s_row = (sz_off + oy + iy) * self.words_x + word_off;
                let m_row = (mz_off + iy) * mask.words_x;
                for wx in 0..mask.words_x {
                    let mw = mask.words[m_row + wx];
                    if mw == 0 {
                        continue;
                    }
                    let lo = mw << bit_shift;
                    if (self.words[s_row + wx] & lo) != 0 {
                        return true;
                    }
                    if bit_shift != 0 {
                        let hi = mw >> (64 - bit_shift);
                        if hi != 0 && (s_row + wx + 1) < self.words.len() && (self.words[s_row + wx + 1] & hi) != 0 {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// OR-stamp `mask` into `self` at `(ox, oy, oz)`.
    pub fn stamp_at(&mut self, mask: &BitGrid3D, ox: usize, oy: usize, oz: usize) {
        if ox + mask.nx > self.nx || oy + mask.ny > self.ny || oz + mask.nz > self.nz {
            return;
        }
        let word_off = ox >> 6;
        let bit_shift = (ox & 63) as u32;
        for iz in 0..mask.nz {
            let sz_off = (oz + iz) * self.ny;
            let mz_off = iz * mask.ny;
            for iy in 0..mask.ny {
                let s_row = (sz_off + oy + iy) * self.words_x + word_off;
                let m_row = (mz_off + iy) * mask.words_x;
                for wx in 0..mask.words_x {
                    let mw = mask.words[m_row + wx];
                    if mw == 0 {
                        continue;
                    }
                    self.words[s_row + wx] |= mw << bit_shift;
                    if bit_shift != 0 {
                        let hi = mw >> (64 - bit_shift);
                        if hi != 0 && word_off + wx + 1 < self.words_x {
                            self.words[s_row + wx + 1] |= hi;
                        }
                    }
                }
            }
        }
    }

    /// Clear `mask`'s `1` bits from `self` at `(ox, oy, oz)` (for Ruin-and-Recreate un-placement).
    pub fn unstamp_at(&mut self, mask: &BitGrid3D, ox: usize, oy: usize, oz: usize) {
        if ox + mask.nx > self.nx || oy + mask.ny > self.ny || oz + mask.nz > self.nz {
            return;
        }
        let word_off = ox >> 6;
        let bit_shift = (ox & 63) as u32;
        for iz in 0..mask.nz {
            let sz_off = (oz + iz) * self.ny;
            let mz_off = iz * mask.ny;
            for iy in 0..mask.ny {
                let s_row = (sz_off + oy + iy) * self.words_x + word_off;
                let m_row = (mz_off + iy) * mask.words_x;
                for wx in 0..mask.words_x {
                    let mw = mask.words[m_row + wx];
                    if mw == 0 {
                        continue;
                    }
                    self.words[s_row + wx] &= !(mw << bit_shift);
                    if bit_shift != 0 {
                        let hi = mw >> (64 - bit_shift);
                        if hi != 0 && word_off + wx + 1 < self.words_x {
                            self.words[s_row + wx + 1] &= !hi;
                        }
                    }
                }
            }
        }
    }

    /// Score floor, wall, and neighbor-voxel contact for `mask` placed at `(ox, oy, oz)`.
    pub fn contact_score_at(&self, mask: &BitGrid3D, ox: usize, oy: usize, oz: usize) -> u32 {
        if ox + mask.nx > self.nx || oy + mask.ny > self.ny || oz + mask.nz > self.nz {
            return 0;
        }
        let mut score = 0_u32;
        if ox == 0 || ox + mask.nx == self.nx {
            score += (mask.ny * mask.nz) as u32;
        }
        if oy == 0 || oy + mask.ny == self.ny {
            score += (mask.nx * mask.nz) as u32;
        }
        let word_off = ox >> 6;
        let bit_shift = (ox & 63) as u32;

        for iz in 0..mask.nz {
            let z = oz + iz;
            let mz_off = iz * mask.ny;
            for iy in 0..mask.ny {
                let y = oy + iy;
                let m_row = (mz_off + iy) * mask.words_x;
                for wx in 0..mask.words_x {
                    let mw = mask.words[m_row + wx];
                    if mw == 0 {
                        continue;
                    }
                    if z == 0 {
                        score += mw.count_ones() * 2;
                    } else {
                        let below_row = ((z - 1) * self.ny + y) * self.words_x + word_off + wx;
                        score += (self.words[below_row] & (mw << bit_shift)).count_ones() * 2;
                        if bit_shift != 0 {
                            let hi = mw >> (64 - bit_shift);
                            if hi != 0 && word_off + wx + 1 < self.words_x {
                                score += (self.words[below_row + 1] & hi).count_ones() * 2;
                            }
                        }
                    }
                    if y > 0 {
                        let ym_row = (z * self.ny + (y - 1)) * self.words_x + word_off + wx;
                        score += (self.words[ym_row] & (mw << bit_shift)).count_ones();
                    }
                    if y + 1 < self.ny {
                        let yp_row = (z * self.ny + (y + 1)) * self.words_x + word_off + wx;
                        score += (self.words[yp_row] & (mw << bit_shift)).count_ones();
                    }
                }
            }
        }
        score
    }
}

/// 2D XY heightmap (`heights[y * nx + x] = max_occupied_z + 1`, `0` = empty column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeightMap2D {
    pub nx: usize,
    pub ny: usize,
    pub heights: Vec<u16>,
}

impl HeightMap2D {
    /// Allocate an empty heightmap.
    pub fn new(nx: usize, ny: usize, max_bytes: Option<usize>) -> Result<Self> {
        let nx = nx.max(1);
        let ny = ny.max(1);
        let len = nx
            .checked_mul(ny)
            .ok_or_else(|| NestCraftError::InvalidInput("heightmap size overflow".to_owned()))?;
        let heights = try_alloc_zeroed::<u16>(len, max_bytes)?;
        Ok(Self { nx, ny, heights })
    }

    /// Reset all column heights to `0`.
    pub fn clear(&mut self) {
        self.heights.fill(0);
    }

    /// Update column heights after stamping `mask` at `(ox, oy, oz)`.
    pub fn update_from_stamp(&mut self, mask: &BitGrid3D, ox: usize, oy: usize, oz: usize) {
        let x_end = mask.nx.min(self.nx.saturating_sub(ox));
        let y_end = mask.ny.min(self.ny.saturating_sub(oy));
        for iy in 0..y_end {
            let row_off = (oy + iy) * self.nx + ox;
            for ix in 0..x_end {
                let mut col_top = 0_usize;
                for iz in (0..mask.nz).rev() {
                    if mask.get(ix, iy, iz) {
                        col_top = oz + iz + 1;
                        break;
                    }
                }
                let h = u16::try_from(col_top).unwrap_or(u16::MAX);
                if h > self.heights[row_off + ix] {
                    self.heights[row_off + ix] = h;
                }
            }
        }
    }

    /// Recompute exact column heights from a `BitGrid3D` (used after Ruin-and-Recreate un-stamping).
    pub fn rebuild_from_grid(&mut self, grid: &BitGrid3D) {
        self.heights.fill(0);
        let x_end = self.nx.min(grid.nx);
        let y_end = self.ny.min(grid.ny);
        for y in 0..y_end {
            let row = y * self.nx;
            for x in 0..x_end {
                for z in (0..grid.nz).rev() {
                    if grid.get(x, y, z) {
                        self.heights[row + x] = u16::try_from(z + 1).unwrap_or(u16::MAX);
                        break;
                    }
                }
            }
        }
    }

    /// Return the maximum column height inside rectangle `[ox..ox+w, oy..oy+d]`.
    pub fn max_height_in_rect(&self, ox: usize, oy: usize, w: usize, d: usize) -> usize {
        let x_end = ox.saturating_add(w).min(self.nx);
        let y_end = oy.saturating_add(d).min(self.ny);
        let mut best = 0_u16;
        for y in oy..y_end {
            let row = y * self.nx;
            for x in ox..x_end {
                best = best.max(self.heights[row + x]);
            }
        }
        usize::from(best)
    }
}

/// Exact 13-axis SAT triangle-vs-axis-aligned-box overlap test (`center`, `half_extent`).
pub fn tri_overlaps_voxel_box(a: [f32; 3], b: [f32; 3], c: [f32; 3], center: [f32; 3], half: f32) -> bool {
    let v0 = [a[0] - center[0], a[1] - center[1], a[2] - center[2]];
    let v1 = [b[0] - center[0], b[1] - center[1], b[2] - center[2]];
    let v2 = [c[0] - center[0], c[1] - center[1], c[2] - center[2]];

    // 1. Box face normals (X, Y, Z)
    const SAT_EPS: f32 = 1e-5;
    for k in 0..3 {
        let lo = v0[k].min(v1[k]).min(v2[k]);
        let hi = v0[k].max(v1[k]).max(v2[k]);
        if lo > half + SAT_EPS || hi < -half - SAT_EPS {
            return false;
        }
    }

    let e0 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
    let e1 = [v2[0] - v1[0], v2[1] - v1[1], v2[2] - v1[2]];
    let e2 = [v0[0] - v2[0], v0[1] - v2[1], v0[2] - v2[2]];

    // 2. 9 edge cross-product axes
    let edges = [e0, e1, e2];
    for e in &edges {
        // Cross with X = (0, -e[2], e[1])
        if !sat_axis_test(0.0, -e[2], e[1], v0, v1, v2, half) {
            return false;
        }
        // Cross with Y = (e[2], 0, -e[0])
        if !sat_axis_test(e[2], 0.0, -e[0], v0, v1, v2, half) {
            return false;
        }
        // Cross with Z = (-e[1], e[0], 0)
        if !sat_axis_test(-e[1], e[0], 0.0, v0, v1, v2, half) {
            return false;
        }
    }

    // 3. Triangle plane normal
    let n = [
        e0[1] * e1[2] - e0[2] * e1[1],
        e0[2] * e1[0] - e0[0] * e1[2],
        e0[0] * e1[1] - e0[1] * e1[0],
    ];
    let d = n[0] * v0[0] + n[1] * v0[1] + n[2] * v0[2];
    let r = half * (n[0].abs() + n[1].abs() + n[2].abs());
    d.abs() <= r + 1e-6
}

#[inline]
fn sat_axis_test(ax: f32, ay: f32, az: f32, v0: [f32; 3], v1: [f32; 3], v2: [f32; 3], half: f32) -> bool {
    let p0 = ax * v0[0] + ay * v0[1] + az * v0[2];
    let p1 = ax * v1[0] + ay * v1[1] + az * v1[2];
    let p2 = ax * v2[0] + ay * v2[1] + az * v2[2];
    let r = half * (ax.abs() + ay.abs() + az.abs()) + 1e-6;
    let lo = p0.min(p1).min(p2);
    let hi = p0.max(p1).max(p2);
    lo <= r && hi >= -r
}

#[inline]
fn rotate_vec3(rot: &[[f32; 3]; 3], p: [f32; 3]) -> [f32; 3] {
    [
        rot[0][0] * p[0] + rot[0][1] * p[1] + rot[0][2] * p[2],
        rot[1][0] * p[0] + rot[1][1] * p[1] + rot[1][2] * p[2],
        rot[2][0] * p[0] + rot[2][1] * p[1] + rot[2][2] * p[2],
    ]
}

/// Conservatively voxelize `mesh` under rotation `rot` using surface triangle-AABB rasterization
/// + bounded 6-connected exterior flood-fill (safe on flipped normals and non-manifold/leaky STLs).
///
/// Returns `(solid_mask_with_pad, grid_origin_mm, rotated_dims_mm)`.
pub fn voxelize_mesh_flood_fill(
    mesh: &TriMesh,
    rot: &[[f32; 3]; 3],
    pitch: f32,
    pad_voxels: [usize; 3],
    fill_open_cavities: bool,
    max_bytes: Option<usize>,
    cancel: Option<&AtomicBool>,
) -> Result<(BitGrid3D, [f32; 3], [f32; 3])> {
    let pitch = pitch.clamp(0.1, 20.0);
    let mut rmin = [f32::INFINITY; 3];
    let mut rmax = [f32::NEG_INFINITY; 3];
    let mut rot_verts = Vec::new();
    rot_verts
        .try_reserve_exact(mesh.positions.len())
        .map_err(|_| NestCraftError::InvalidInput("out of memory rotating mesh vertices".to_owned()))?;
    for &p in &mesh.positions {
        let rp = rotate_vec3(rot, p);
        for k in 0..3 {
            rmin[k] = rmin[k].min(rp[k]);
            rmax[k] = rmax[k].max(rp[k]);
        }
        rot_verts.push(rp);
    }

    let dims_mm = [
        (rmax[0] - rmin[0]).max(0.0),
        (rmax[1] - rmin[1]).max(0.0),
        (rmax[2] - rmin[2]).max(0.0),
    ];
    let inner_nx = ((dims_mm[0] / pitch - 1e-4).ceil().max(1.0)) as usize;
    let inner_ny = ((dims_mm[1] / pitch - 1e-4).ceil().max(1.0)) as usize;
    let inner_nz = ((dims_mm[2] / pitch - 1e-4).ceil().max(1.0)) as usize;

    // +1 exterior air shell on every side of the inner grid for flood-fill.
    let wnx = inner_nx + 2;
    let wny = inner_ny + 2;
    let wnz = inner_nz + 2;
    let mut surface = BitGrid3D::new(wnx, wny, wnz, max_bytes)?;
    let half = pitch * 0.5;

    for (tri_idx, &[i0, i1, i2]) in mesh.indices.iter().enumerate() {
        if (tri_idx & 1023) == 0 {
            check_cancel(cancel)?;
        }
        let a = rot_verts[i0 as usize];
        let b = rot_verts[i1 as usize];
        let c = rot_verts[i2 as usize];
        let tmin = [
            a[0].min(b[0]).min(c[0]),
            a[1].min(b[1]).min(c[1]),
            a[2].min(b[2]).min(c[2]),
        ];
        let tmax = [
            a[0].max(b[0]).max(c[0]),
            a[1].max(b[1]).max(c[1]),
            a[2].max(b[2]).max(c[2]),
        ];
        let x0 = (((tmin[0] - rmin[0]) / pitch).floor().max(0.0) as usize).min(inner_nx - 1);
        let y0 = (((tmin[1] - rmin[1]) / pitch).floor().max(0.0) as usize).min(inner_ny - 1);
        let z0 = (((tmin[2] - rmin[2]) / pitch).floor().max(0.0) as usize).min(inner_nz - 1);
        let x1 = (((tmax[0] - rmin[0]) / pitch).floor().max(0.0) as usize).min(inner_nx - 1);
        let y1 = (((tmax[1] - rmin[1]) / pitch).floor().max(0.0) as usize).min(inner_ny - 1);
        let z1 = (((tmax[2] - rmin[2]) / pitch).floor().max(0.0) as usize).min(inner_nz - 1);

        for iz in z0..=z1 {
            let cz = rmin[2] + (iz as f32 + 0.5) * pitch;
            for iy in y0..=y1 {
                let cy = rmin[1] + (iy as f32 + 0.5) * pitch;
                for ix in x0..=x1 {
                    let cx = rmin[0] + (ix as f32 + 0.5) * pitch;
                    if tri_overlaps_voxel_box(a, b, c, [cx, cy, cz], half) {
                        surface.set(ix + 1, iy + 1, iz + 1);
                    }
                }
            }
        }
    }

    // 6-connected exterior BFS flood-fill starting from (0, 0, 0).
    let mut visited = BitGrid3D::new(wnx, wny, wnz, max_bytes)?;
    let mut queue = VecDeque::new();
    visited.set(0, 0, 0);
    queue.push_back((0_u32, 0_u32, 0_u32));

    let mut pops = 0_usize;
    while let Some((x, y, z)) = queue.pop_front() {
        pops += 1;
        if (pops & 4095) == 0 {
            check_cancel(cancel)?;
        }
        let xu = x as usize;
        let yu = y as usize;
        let zu = z as usize;
        let neighbors = [
            (xu.checked_sub(1), Some(yu), Some(zu)),
            ((xu + 1 < wnx).then_some(xu + 1), Some(yu), Some(zu)),
            (Some(xu), yu.checked_sub(1), Some(zu)),
            (Some(xu), (yu + 1 < wny).then_some(yu + 1), Some(zu)),
            (Some(xu), Some(yu), zu.checked_sub(1)),
            (Some(xu), Some(yu), (zu + 1 < wnz).then_some(zu + 1)),
        ];
        for (nx_opt, ny_opt, nz_opt) in neighbors {
            if let (Some(nx), Some(ny), Some(nz)) = (nx_opt, ny_opt, nz_opt)
                && !surface.get(nx, ny, nz)
                && !visited.get(nx, ny, nz)
            {
                visited.set(nx, ny, nz);
                queue.push_back((nx as u32, ny as u32, nz as u32));
            }
        }
    }

    let solid_nx = inner_nx + 2 * pad_voxels[0];
    let solid_ny = inner_ny + 2 * pad_voxels[1];
    let solid_nz = inner_nz + 2 * pad_voxels[2];
    let mut solid = BitGrid3D::new(solid_nx, solid_ny, solid_nz, max_bytes)?;

    for iz in 0..inner_nz {
        for iy in 0..inner_ny {
            for ix in 0..inner_nx {
                if surface.get(ix + 1, iy + 1, iz + 1) || !visited.get(ix + 1, iy + 1, iz + 1) {
                    solid.set(pad_voxels[0] + ix, pad_voxels[1] + iy, pad_voxels[2] + iz);
                }
            }
        }
    }

    if fill_open_cavities {
        fill_pocket_spans(&mut solid, pad_voxels, [inner_nx, inner_ny, inner_nz], max_bytes)?;
    }

    let origin = [
        rmin[0] - (pad_voxels[0] as f32) * pitch,
        rmin[1] - (pad_voxels[1] as f32) * pitch,
        rmin[2] - (pad_voxels[2] as f32) * pitch,
    ];
    Ok((solid, origin, dims_mm))
}

/// Close open cavities/pockets/through-holes (voxels enclosed on >= 2 of 3 cardinal axes).
fn fill_pocket_spans(
    solid: &mut BitGrid3D,
    pad: [usize; 3],
    inner: [usize; 3],
    max_bytes: Option<usize>,
) -> Result<()> {
    let [inx, iny, inz] = inner;
    let total = inx
        .checked_mul(iny)
        .and_then(|v| v.checked_mul(inz))
        .ok_or_else(|| NestCraftError::InvalidInput("cavity closure size overflow".to_owned()))?;
    let mut votes = try_alloc_zeroed::<u8>(total, max_bytes)?;

    // X-spans
    for iz in 0..inz {
        for iy in 0..iny {
            let mut first = None;
            let mut last = 0_usize;
            for ix in 0..inx {
                if solid.get(pad[0] + ix, pad[1] + iy, pad[2] + iz) {
                    if first.is_none() {
                        first = Some(ix);
                    }
                    last = ix;
                }
            }
            if let Some(lo) = first {
                for ix in lo..=last {
                    votes[(iz * iny + iy) * inx + ix] += 1;
                }
            }
        }
    }
    // Y-spans
    for iz in 0..inz {
        for ix in 0..inx {
            let mut first = None;
            let mut last = 0_usize;
            for iy in 0..iny {
                if solid.get(pad[0] + ix, pad[1] + iy, pad[2] + iz) {
                    if first.is_none() {
                        first = Some(iy);
                    }
                    last = iy;
                }
            }
            if let Some(lo) = first {
                for iy in lo..=last {
                    votes[(iz * iny + iy) * inx + ix] += 1;
                }
            }
        }
    }
    // Z-spans
    for iy in 0..iny {
        for ix in 0..inx {
            let mut first = None;
            let mut last = 0_usize;
            for iz in 0..inz {
                if solid.get(pad[0] + ix, pad[1] + iy, pad[2] + iz) {
                    if first.is_none() {
                        first = Some(iz);
                    }
                    last = iz;
                }
            }
            if let Some(lo) = first {
                for iz in lo..=last {
                    votes[(iz * iny + iy) * inx + ix] += 1;
                }
            }
        }
    }

    for iz in 0..inz {
        for iy in 0..iny {
            for ix in 0..inx {
                if votes[(iz * iny + iy) * inx + ix] >= 2 {
                    solid.set(pad[0] + ix, pad[1] + iy, pad[2] + iz);
                }
            }
        }
    }
    Ok(())
}

/// Reusable thread-local scratch buffers for $O(N)$ Felzenszwalb-Huttenlocher 3D EDT.
#[derive(Debug, Default)]
pub struct EdtScratch {
    pub dist: Vec<f32>,
    f: Vec<f32>,
    v: Vec<usize>,
    z: Vec<f32>,
}

impl EdtScratch {
    fn prepare(&mut self, total_voxels: usize, max_axis: usize, max_bytes: Option<usize>) -> Result<()> {
        let total_bytes = total_voxels
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| NestCraftError::InvalidInput("EDT scratch overflow".to_owned()))?;
        if let Some(limit) = max_bytes
            && total_bytes > limit
        {
            return Err(NestCraftError::InvalidInput(format!(
                "EDT scratch ({total_bytes} bytes) exceeds memory limit ({limit} bytes)"
            )));
        }
        if self.dist.len() < total_voxels {
            self.dist
                .try_reserve_exact(total_voxels - self.dist.len())
                .map_err(|_| NestCraftError::InvalidInput("out of memory for EDT field".to_owned()))?;
        }
        self.dist.resize(total_voxels, EDT_INF);

        if self.f.len() < max_axis {
            self.f.resize(max_axis, 0.0);
        }
        if self.v.len() < max_axis {
            self.v.resize(max_axis, 0);
        }
        if self.z.len() < max_axis + 1 {
            self.z.resize(max_axis + 1, 0.0);
        }
        Ok(())
    }
}

/// 1D squared Euclidean distance transform via Felzenszwalb-Huttenlocher parabolic lower envelope.
fn edt_1d_into(f: &[f32], n: usize, scale_sq: f32, v: &mut [usize], z: &mut [f32], out_fn: impl FnMut(usize, f32)) {
    let mut write_out = out_fn;
    if n == 0 {
        return;
    }
    let scale = scale_sq.max(1e-8);
    let mut k = 0_usize;
    v[0] = 0;
    z[0] = -EDT_INF;
    z[1] = EDT_INF;

    for q in 1..n {
        let fq = f[q];
        if fq >= EDT_INF * 0.5 {
            continue;
        }
        let qf = q as f32;
        loop {
            let vk = v[k];
            let fvk = f[vk];
            if fvk >= EDT_INF * 0.5 {
                v[k] = q;
                break;
            }
            let vkf = vk as f32;
            let s = ((fq + scale * qf * qf) - (fvk + scale * vkf * vkf)) / (2.0 * scale * (qf - vkf));
            if s <= z[k] {
                if k == 0 {
                    v[0] = q;
                    break;
                }
                k -= 1;
            } else {
                k += 1;
                v[k] = q;
                z[k] = s;
                z[k + 1] = EDT_INF;
                break;
            }
        }
    }

    if f[v[0]] >= EDT_INF * 0.5 {
        for q in 0..n {
            write_out(q, EDT_INF);
        }
        return;
    }

    let mut idx = 0_usize;
    for q in 0..n {
        let qf = q as f32;
        while z[idx + 1] < qf {
            idx += 1;
        }
        let dq = qf - (v[idx] as f32);
        let val = scale * dq * dq + f[v[idx]];
        write_out(q, val.min(EDT_INF));
    }
}

/// Compute squared anisotropic 3D Euclidean distance transform of `solid` into `scratch.dist`.
///
/// Pass `scale_sq = [1.0, 1.0, (r_xy / r_z).powi(2)]` to measure distance normalized to XY voxel units.
pub fn edt_3d_squared(
    solid: &BitGrid3D,
    scale_sq: [f32; 3],
    scratch: &mut EdtScratch,
    max_bytes: Option<usize>,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let nx = solid.nx;
    let ny = solid.ny;
    let nz = solid.nz;
    let total = nx
        .checked_mul(ny)
        .and_then(|v| v.checked_mul(nz))
        .ok_or_else(|| NestCraftError::InvalidInput("3D EDT size overflow".to_owned()))?;
    let max_axis = nx.max(ny).max(nz);
    scratch.prepare(total, max_axis, max_bytes)?;

    // Initialize binary grid & run X pass
    for z in 0..nz {
        if (z & 15) == 0 {
            check_cancel(cancel)?;
        }
        for y in 0..ny {
            let row = (z * ny + y) * nx;
            for x in 0..nx {
                scratch.f[x] = if solid.get(x, y, z) { 0.0 } else { EDT_INF };
            }
            edt_1d_into(&scratch.f, nx, scale_sq[0], &mut scratch.v, &mut scratch.z, |x, val| {
                scratch.dist[row + x] = val;
            });
        }
    }

    // Y pass
    for z in 0..nz {
        if (z & 15) == 0 {
            check_cancel(cancel)?;
        }
        for x in 0..nx {
            let z_off = z * ny * nx + x;
            for y in 0..ny {
                scratch.f[y] = scratch.dist[z_off + y * nx];
            }
            edt_1d_into(&scratch.f, ny, scale_sq[1], &mut scratch.v, &mut scratch.z, |y, val| {
                scratch.dist[z_off + y * nx] = val;
            });
        }
    }

    // Z pass
    let plane = nx * ny;
    for y in 0..ny {
        if (y & 15) == 0 {
            check_cancel(cancel)?;
        }
        for x in 0..nx {
            let xy_off = y * nx + x;
            for z in 0..nz {
                scratch.f[z] = scratch.dist[z * plane + xy_off];
            }
            edt_1d_into(&scratch.f, nz, scale_sq[2], &mut scratch.v, &mut scratch.z, |z, val| {
                scratch.dist[z * plane + xy_off] = val;
            });
        }
    }

    Ok(())
}

/// Dilate `solid` into an ellipsoidal/spherical clearance mask (`radius_xy_voxels`, `radius_z_voxels`).
pub fn dilate_clearance_mask(
    solid: &BitGrid3D,
    radius_xy_voxels: f32,
    radius_z_voxels: f32,
    scratch: &mut EdtScratch,
    max_bytes: Option<usize>,
    cancel: Option<&AtomicBool>,
) -> Result<BitGrid3D> {
    if radius_xy_voxels <= 0.0 || radius_z_voxels <= 0.0 {
        return Ok(solid.clone());
    }
    let z_ratio = radius_xy_voxels / radius_z_voxels;
    edt_3d_squared(
        solid,
        [1.0, 1.0, z_ratio * z_ratio],
        scratch,
        max_bytes,
        cancel,
    )?;

    let thresh = radius_xy_voxels * radius_xy_voxels + 1e-4;
    let mut out = BitGrid3D::new(solid.nx, solid.ny, solid.nz, max_bytes)?;
    for z in 0..solid.nz {
        for y in 0..solid.ny {
            let row = (z * solid.ny + y) * solid.nx;
            for x in 0..solid.nx {
                if scratch.dist[row + x] <= thresh {
                    out.set(x, y, z);
                }
            }
        }
    }
    Ok(out)
}

/// Smoothly compute half-clearance `r_p` (mm) from part solid volume (`min_gap/2 ..= max_gap/2`).
pub fn adaptive_part_half_clearance_mm(volume_mm3: f64, min_gap_mm: f32, max_gap_mm: f32) -> f32 {
    let lo = min_gap_mm.max(0.0);
    let hi = max_gap_mm.max(lo);
    const VOL_SMALL: f64 = 5_000.0;
    const VOL_LARGE: f64 = 100_000.0;
    let t = ((volume_mm3 - VOL_SMALL) / (VOL_LARGE - VOL_SMALL)).clamp(0.0, 1.0) as f32;
    0.5 * (lo + t * (hi - lo))
}

/// Candidate orientation density for `RotationConstraint::Free`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreeAngleSet {
    /// 24 octahedral orientations (6 face-down directions × 4 Z-rotations).
    #[default]
    Octahedral24,
    /// 48 orientations (24 octahedral + 24 45°-tilted orientations).
    Tilted48,
    /// 64 dense orientations (octahedral + multi-axis tilted candidates).
    Dense64,
}

/// Z-rotation angular step for `RotationConstraint::ZOnly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZOnlyStep {
    /// 90° steps (4 candidates).
    Deg90,
    /// 45° steps (8 candidates).
    #[default]
    Deg45,
    /// 30° steps (12 candidates).
    Deg30,
}

fn mat3_mul(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let mut out = [[0.0_f32; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            out[r][c] = a[r][0] * b[0][c] + a[r][1] * b[1][c] + a[r][2] * b[2][c];
        }
    }
    out
}

#[inline]
fn sin_cos_deg_clean(deg: f32) -> (f32, f32) {
    let (mut s, mut c) = deg.to_radians().sin_cos();
    for target in [-1.0_f32, 0.0, 1.0] {
        if (s - target).abs() < 1e-6 {
            s = target;
        }
        if (c - target).abs() < 1e-6 {
            c = target;
        }
    }
    (s, c)
}

fn rot_z_deg(deg: f32) -> [[f32; 3]; 3] {
    let (s, c) = sin_cos_deg_clean(deg);
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

fn rot_x_deg(deg: f32) -> [[f32; 3]; 3] {
    let (s, c) = sin_cos_deg_clean(deg);
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}

fn rot_y_deg(deg: f32) -> [[f32; 3]; 3] {
    let (s, c) = sin_cos_deg_clean(deg);
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

/// Extract the orthonormal 3×3 rotation matrix from a rigid `Transform3x4` (normalizing columns).
pub fn rotation_from_transform(tf: &Transform3x4) -> [[f32; 3]; 3] {
    let mut c0 = [tf[0][0], tf[1][0], tf[2][0]];
    let mut c1 = [tf[0][1], tf[1][1], tf[2][1]];
    let len0 = (c0[0] * c0[0] + c0[1] * c0[1] + c0[2] * c0[2]).sqrt();
    if len0 > 1e-8 {
        c0 = [c0[0] / len0, c0[1] / len0, c0[2] / len0];
    } else {
        c0 = [1.0, 0.0, 0.0];
    }
    let dot01 = c0[0] * c1[0] + c0[1] * c1[1] + c0[2] * c1[2];
    c1 = [c1[0] - dot01 * c0[0], c1[1] - dot01 * c0[1], c1[2] - dot01 * c0[2]];
    let len1 = (c1[0] * c1[0] + c1[1] * c1[1] + c1[2] * c1[2]).sqrt();
    if len1 > 1e-8 {
        c1 = [c1[0] / len1, c1[1] / len1, c1[2] / len1];
    } else {
        c1 = [0.0, 1.0, 0.0];
    }
    let c2 = [
        c0[1] * c1[2] - c0[2] * c1[1],
        c0[2] * c1[0] - c0[0] * c1[2],
        c0[0] * c1[1] - c0[1] * c1[0],
    ];
    [
        [c0[0], c1[0], c2[0]],
        [c0[1], c1[1], c2[1]],
        [c0[2], c1[2], c2[2]],
    ]
}

/// Compute a right-handed PCA principal-axis alignment rotation for `mesh` (3×3 Jacobi eigensolver).
pub fn compute_pca_rotation(mesh: &TriMesh) -> [[f32; 3]; 3] {
    if mesh.positions.len() < 3 {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    let inv_n = 1.0 / (mesh.positions.len() as f64);
    let mut mean = [0.0_f64; 3];
    for p in &mesh.positions {
        mean[0] += f64::from(p[0]);
        mean[1] += f64::from(p[1]);
        mean[2] += f64::from(p[2]);
    }
    mean[0] *= inv_n;
    mean[1] *= inv_n;
    mean[2] *= inv_n;

    let mut cov = [[0.0_f64; 3]; 3];
    for p in &mesh.positions {
        let d = [
            f64::from(p[0]) - mean[0],
            f64::from(p[1]) - mean[1],
            f64::from(p[2]) - mean[2],
        ];
        for r in 0..3 {
            for c in 0..3 {
                cov[r][c] += d[r] * d[c];
            }
        }
    }

    let mut v = [[1.0_f64, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..24 {
        let mut p_idx = 0;
        let mut q_idx = 1;
        let mut max_off = cov[0][1].abs();
        for (a, b) in [(0, 2), (1, 2)] {
            if cov[a][b].abs() > max_off {
                max_off = cov[a][b].abs();
                p_idx = a;
                q_idx = b;
            }
        }
        if max_off < 1e-12 {
            break;
        }
        let app = cov[p_idx][p_idx];
        let aqq = cov[q_idx][q_idx];
        let apq = cov[p_idx][q_idx];
        let tau = (aqq - app) / (2.0 * apq);
        let t = if tau >= 0.0 {
            1.0 / (tau + (1.0 + tau * tau).sqrt())
        } else {
            -1.0 / (-tau + (1.0 + tau * tau).sqrt())
        };
        let c = 1.0 / (1.0 + t * t).sqrt();
        let s = t * c;

        cov[p_idx][p_idx] = app - t * apq;
        cov[q_idx][q_idx] = aqq + t * apq;
        cov[p_idx][q_idx] = 0.0;
        cov[q_idx][p_idx] = 0.0;
        let r = 3 - p_idx - q_idx;
        let arp = cov[r][p_idx];
        let arq = cov[r][q_idx];
        cov[r][p_idx] = c * arp - s * arq;
        cov[p_idx][r] = cov[r][p_idx];
        cov[r][q_idx] = s * arp + c * arq;
        cov[q_idx][r] = cov[r][q_idx];
        for row in &mut v {
            let vrp = row[p_idx];
            let vrq = row[q_idx];
            row[p_idx] = c * vrp - s * vrq;
            row[q_idx] = s * vrp + c * vrq;
        }
    }

    // Sort eigenvectors descending by eigenvalue so principal axis aligns with X, smallest with Z.
    let mut order = [0_usize, 1, 2];
    order.sort_by(|&a, &b| cov[b][b].total_cmp(&cov[a][a]));
    let e0 = [v[0][order[0]], v[1][order[0]], v[2][order[0]]];
    let e1 = [v[0][order[1]], v[1][order[1]], v[2][order[1]]];
    let e2 = [
        e0[1] * e1[2] - e0[2] * e1[1],
        e0[2] * e1[0] - e0[0] * e1[2],
        e0[0] * e1[1] - e0[1] * e1[0],
    ];
    // Transpose (rows = principal axes) rotates mesh into principal frame.
    [
        [e0[0] as f32, e0[1] as f32, e0[2] as f32],
        [e1[0] as f32, e1[1] as f32, e1[2] as f32],
        [e2[0] as f32, e2[1] as f32, e2[2] as f32],
    ]
}

fn octahedral_rotations() -> Vec<[[f32; 3]; 3]> {
    let base_up = [
        [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        rot_x_deg(180.0),
        rot_x_deg(90.0),
        rot_x_deg(-90.0),
        rot_y_deg(90.0),
        rot_y_deg(-90.0),
    ];
    let mut out = Vec::with_capacity(24);
    for b in &base_up {
        for k in 0..4 {
            out.push(mat3_mul(&rot_z_deg((k as f32) * 90.0), b));
        }
    }
    out
}

/// Generate candidate 3×3 orientation matrices respecting `constraint` and optional PCA pre-alignment.
pub fn generate_candidate_rotations(
    mesh: &TriMesh,
    base_rot: [[f32; 3]; 3],
    constraint: RotationConstraint,
    free_set: FreeAngleSet,
    z_step: ZOnlyStep,
    use_pca_prealign: bool,
) -> Vec<[[f32; 3]; 3]> {
    match constraint {
        RotationConstraint::Fixed => vec![base_rot],
        RotationConstraint::ZOnly => {
            let count = match z_step {
                ZOnlyStep::Deg90 => 4,
                ZOnlyStep::Deg45 => 8,
                ZOnlyStep::Deg30 => 12,
            };
            let step = 360.0 / (count as f32);
            (0..count)
                .map(|i| mat3_mul(&rot_z_deg((i as f32) * step), &base_rot))
                .collect()
        }
        RotationConstraint::Free => {
            let mut rots = octahedral_rotations();
            if matches!(free_set, FreeAngleSet::Tilted48 | FreeAngleSet::Dense64) {
                for base_tilt in [rot_x_deg(45.0), rot_y_deg(45.0), rot_x_deg(-45.0), rot_y_deg(-45.0)] {
                    for k in 0..6 {
                        rots.push(mat3_mul(&rot_z_deg((k as f32) * 60.0), &base_tilt));
                    }
                }
            }
            if free_set == FreeAngleSet::Dense64 {
                let extra_tilts = [
                    mat3_mul(&rot_x_deg(45.0), &rot_y_deg(45.0)),
                    mat3_mul(&rot_x_deg(45.0), &rot_y_deg(-45.0)),
                    mat3_mul(&rot_x_deg(-45.0), &rot_y_deg(45.0)),
                    mat3_mul(&rot_x_deg(-45.0), &rot_y_deg(-45.0)),
                ];
                for t in &extra_tilts {
                    for k in 0..4 {
                        rots.push(mat3_mul(&rot_z_deg((k as f32) * 90.0), t));
                    }
                }
            }
            if use_pca_prealign {
                let pca = compute_pca_rotation(mesh);
                let mut pca_oct = octahedral_rotations()
                    .into_iter()
                    .map(|r| mat3_mul(&r, &pca))
                    .collect::<Vec<_>>();
                pca_oct.extend(rots);
                pca_oct
            } else {
                rots
            }
        }
    }
}

fn rotated_mesh_dims(mesh: &TriMesh, rot: &[[f32; 3]; 3]) -> [f32; 3] {
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for &p in &mesh.positions {
        let rp = rotate_vec3(rot, p);
        for k in 0..3 {
            lo[k] = lo[k].min(rp[k]);
            hi[k] = hi[k].max(rp[k]);
        }
    }
    [
        (hi[0] - lo[0]).max(0.0),
        (hi[1] - lo[1]).max(0.0),
        (hi[2] - lo[2]).max(0.0),
    ]
}

fn coarse_surface_signature(
    mesh: &TriMesh,
    rot: &[[f32; 3]; 3],
    coarse_pitch_mm: f32,
    max_bytes: Option<usize>,
) -> Result<(BitGrid3D, [f32; 3])> {
    let pitch = coarse_pitch_mm.clamp(1.0, 12.0);
    let mut rmin = [f32::INFINITY; 3];
    let mut rmax = [f32::NEG_INFINITY; 3];
    let mut rot_verts = Vec::with_capacity(mesh.positions.len());
    for &p in &mesh.positions {
        let rp = rotate_vec3(rot, p);
        for k in 0..3 {
            rmin[k] = rmin[k].min(rp[k]);
            rmax[k] = rmax[k].max(rp[k]);
        }
        rot_verts.push(rp);
    }
    let dims = [
        (rmax[0] - rmin[0]).max(0.0),
        (rmax[1] - rmin[1]).max(0.0),
        (rmax[2] - rmin[2]).max(0.0),
    ];
    let nx = ((dims[0] / pitch - 1e-4).ceil().max(1.0)) as usize;
    let ny = ((dims[1] / pitch - 1e-4).ceil().max(1.0)) as usize;
    let nz = ((dims[2] / pitch - 1e-4).ceil().max(1.0)) as usize;
    let mut grid = BitGrid3D::new(nx, ny, nz, max_bytes)?;
    let half = pitch * 0.5;
    for &[i0, i1, i2] in &mesh.indices {
        let a = rot_verts[i0 as usize];
        let b = rot_verts[i1 as usize];
        let c = rot_verts[i2 as usize];
        let tmin = [
            a[0].min(b[0]).min(c[0]),
            a[1].min(b[1]).min(c[1]),
            a[2].min(b[2]).min(c[2]),
        ];
        let tmax = [
            a[0].max(b[0]).max(c[0]),
            a[1].max(b[1]).max(c[1]),
            a[2].max(b[2]).max(c[2]),
        ];
        let x0 = (((tmin[0] - rmin[0]) / pitch).floor().max(0.0) as usize).min(nx - 1);
        let y0 = (((tmin[1] - rmin[1]) / pitch).floor().max(0.0) as usize).min(ny - 1);
        let z0 = (((tmin[2] - rmin[2]) / pitch).floor().max(0.0) as usize).min(nz - 1);
        let x1 = (((tmax[0] - rmin[0]) / pitch).floor().max(0.0) as usize).min(nx - 1);
        let y1 = (((tmax[1] - rmin[1]) / pitch).floor().max(0.0) as usize).min(ny - 1);
        let z1 = (((tmax[2] - rmin[2]) / pitch).floor().max(0.0) as usize).min(nz - 1);
        for iz in z0..=z1 {
            let cz = rmin[2] + (iz as f32 + 0.5) * pitch;
            for iy in y0..=y1 {
                let cy = rmin[1] + (iy as f32 + 0.5) * pitch;
                for ix in x0..=x1 {
                    let cx = rmin[0] + (ix as f32 + 0.5) * pitch;
                    if tri_overlaps_voxel_box(a, b, c, [cx, cy, cz], half) {
                        grid.set(ix, iy, iz);
                    }
                }
            }
        }
    }
    Ok((grid, dims))
}

/// Deduplicate candidate rotations by coarse voxel bitpattern (`3–4 mm` pitch) and sort
/// surviving orientations by `(height_z, footprint_xy)` ascending.
pub fn prune_symmetric_rotations(
    mesh: &TriMesh,
    candidates: &[[[f32; 3]; 3]],
    coarse_pitch_mm: f32,
    max_bytes: Option<usize>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<[[f32; 3]; 3]>> {
    let mut seen: HashMap<u64, Vec<BitGrid3D>> = HashMap::new();
    let mut surviving: Vec<([[f32; 3]; 3], [f32; 3])> = Vec::new();

    for (idx, rot) in candidates.iter().enumerate() {
        if (idx & 7) == 0 {
            check_cancel(cancel)?;
        }
        let (sig_grid, dims) = coarse_surface_signature(mesh, rot, coarse_pitch_mm, max_bytes)?;
        let h = sig_grid.hash64();
        let bucket = seen.entry(h).or_default();
        if bucket.iter().any(|existing| existing == &sig_grid) {
            continue;
        }
        bucket.push(sig_grid);
        surviving.push((*rot, dims));
    }

    surviving.sort_by(|a, b| {
        let za = a.1[2];
        let zb = b.1[2];
        za.total_cmp(&zb)
            .then_with(|| (a.1[0] * a.1[1]).total_cmp(&(b.1[0] * b.1[1])))
    });
    Ok(surviving.into_iter().map(|(rot, _)| rot).collect())
}

/// Precomputed voxelized representation for one `(PartPrototype, orientation)`.
#[derive(Debug, Clone)]
pub struct PartOrientationMask {
    pub rotation: [[f32; 3]; 3],
    pub grid_origin_mm: [f32; 3],
    pub rotated_dims_mm: [f32; 3],
    pub solid_mask: BitGrid3D,
    pub clearance_mask: BitGrid3D,
}

/// Configuration for building a prototype's orientation mask catalog.
#[derive(Debug, Clone, Copy)]
pub struct PartVoxelConfig {
    pub resolution_mm: f32,
    pub half_clearance_xy_mm: f32,
    pub half_clearance_z_mm: f32,
    pub fill_open_cavities: bool,
    pub coarse_prune_pitch_mm: f32,
    pub max_memory_bytes: Option<usize>,
}

/// Build the symmetry-pruned orientation mask catalog for a single prototype using one reusable `EdtScratch`.
pub fn build_part_orientation_catalog(
    mesh: &TriMesh,
    candidates: &[[[f32; 3]; 3]],
    cfg: PartVoxelConfig,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<PartOrientationMask>> {
    let pitch = cfg.resolution_mm.clamp(0.25, 10.0);
    let pruned = prune_symmetric_rotations(
        mesh,
        candidates,
        cfg.coarse_prune_pitch_mm.max(pitch * 2.0),
        cfg.max_memory_bytes,
        cancel,
    )?;

    // Tight dilation keeps narrow cavities accessible; Stage 3/collision.rs verify exact clearance.
    let rad_xy_vox = if cfg.half_clearance_xy_mm > 0.0 {
        cfg.half_clearance_xy_mm / pitch + 0.35
    } else {
        0.0
    };
    let rad_z_vox = if cfg.half_clearance_z_mm > 0.0 {
        cfg.half_clearance_z_mm / pitch + 0.35
    } else {
        0.0
    };
    let pad_xy = rad_xy_vox.ceil() as usize;
    let pad_z = rad_z_vox.ceil() as usize;
    let pad = [pad_xy, pad_xy, pad_z];

    let mut scratch = EdtScratch::default();
    let mut catalog = Vec::with_capacity(pruned.len());
    for rot in pruned {
        check_cancel(cancel)?;
        let (solid_mask, grid_origin_mm, rotated_dims_mm) = voxelize_mesh_flood_fill(
            mesh,
            &rot,
            pitch,
            pad,
            cfg.fill_open_cavities,
            cfg.max_memory_bytes,
            cancel,
        )?;
        let clearance_mask = dilate_clearance_mask(
            &solid_mask,
            rad_xy_vox,
            rad_z_vox,
            &mut scratch,
            cfg.max_memory_bytes,
            cancel,
        )?;
        catalog.push(PartOrientationMask {
            rotation: rot,
            grid_origin_mm,
            rotated_dims_mm: if rotated_dims_mm == [0.0; 3] {
                rotated_mesh_dims(mesh, &rot)
            } else {
                rotated_dims_mm
            },
            solid_mask,
            clearance_mask,
        });
    }
    Ok(catalog)
}

#[derive(Debug, Clone)]
struct BvhNode {
    bounds: Aabb,
    left_or_first: u32,
    count_or_right: u32,
    is_leaf: bool,
}

/// Pure-Rust local-space AABB-BVH built once per unique `PartPrototype` for fast Stage 3 compaction.
#[derive(Debug, Clone)]
pub struct MeshBvh {
    nodes: Vec<BvhNode>,
    tri_order: Vec<u32>,
}

impl MeshBvh {
    /// Build a local-space AABB-BVH over `mesh.indices`.
    pub fn build(mesh: &TriMesh) -> Self {
        let n = mesh.indices.len();
        let mut tri_order: Vec<u32> = (0..n as u32).collect();
        let mut tri_bounds = Vec::with_capacity(n);
        let mut centroids = Vec::with_capacity(n);
        for &[i0, i1, i2] in &mesh.indices {
            let a = mesh.positions[i0 as usize];
            let b = mesh.positions[i1 as usize];
            let c = mesh.positions[i2 as usize];
            let lo = [a[0].min(b[0]).min(c[0]), a[1].min(b[1]).min(c[1]), a[2].min(b[2]).min(c[2])];
            let hi = [a[0].max(b[0]).max(c[0]), a[1].max(b[1]).max(c[1]), a[2].max(b[2]).max(c[2])];
            tri_bounds.push([lo, hi]);
            centroids.push([
                (a[0] + b[0] + c[0]) / 3.0,
                (a[1] + b[1] + c[1]) / 3.0,
                (a[2] + b[2] + c[2]) / 3.0,
            ]);
        }

        let mut nodes = Vec::with_capacity((n / 4).max(1) * 2);
        if !tri_order.is_empty() {
            build_bvh_rec(&mut nodes, &mut tri_order, 0, n, &tri_bounds, &centroids);
        }
        Self { nodes, tri_order }
    }

    /// Find the closest surface points `(point_a_world, point_b_world, dist_mm)` between `mesh_a`
    /// and `mesh_b` under world transforms `tf_a` and `tf_b`, pruning branches farther than `max_dist_mm`.
    pub fn min_distance_between(
        bvh_a: &Self,
        mesh_a: &TriMesh,
        tf_a: &Transform3x4,
        bvh_b: &Self,
        mesh_b: &TriMesh,
        tf_b: &Transform3x4,
        max_dist_mm: f32,
    ) -> Option<([f32; 3], [f32; 3], f32)> {
        if bvh_a.nodes.is_empty() || bvh_b.nodes.is_empty() {
            return None;
        }
        let mut best_d2 = max_dist_mm.max(0.0) * max_dist_mm.max(0.0) + 1e-6;
        let mut best_pts: Option<([f32; 3], [f32; 3])> = None;

        bvh_pair_query(
            bvh_a,
            mesh_a,
            tf_a,
            0,
            bvh_b,
            mesh_b,
            tf_b,
            0,
            &mut best_d2,
            &mut best_pts,
        );

        best_pts.map(|(pa, pb)| (pa, pb, best_d2.max(0.0).sqrt()))
    }
}

fn merge_aabb(a: Aabb, b: Aabb) -> Aabb {
    [
        [a[0][0].min(b[0][0]), a[0][1].min(b[0][1]), a[0][2].min(b[0][2])],
        [a[1][0].max(b[1][0]), a[1][1].max(b[1][1]), a[1][2].max(b[1][2])],
    ]
}

fn build_bvh_rec(
    nodes: &mut Vec<BvhNode>,
    tri_order: &mut [u32],
    start: usize,
    end: usize,
    tri_bounds: &[Aabb],
    centroids: &[[f32; 3]],
) -> u32 {
    let mut bounds = tri_bounds[tri_order[start] as usize];
    for &ti in &tri_order[start + 1..end] {
        bounds = merge_aabb(bounds, tri_bounds[ti as usize]);
    }
    let count = end - start;
    let node_idx = nodes.len() as u32;
    if count <= BVH_LEAF_MAX_TRIS {
        nodes.push(BvhNode {
            bounds,
            left_or_first: start as u32,
            count_or_right: count as u32,
            is_leaf: true,
        });
        return node_idx;
    }

    let dx = bounds[1][0] - bounds[0][0];
    let dy = bounds[1][1] - bounds[0][1];
    let dz = bounds[1][2] - bounds[0][2];
    let axis = if dx >= dy && dx >= dz {
        0
    } else if dy >= dz {
        1
    } else {
        2
    };
    let mid = start + count / 2;
    tri_order[start..end].select_nth_unstable_by(count / 2, |&a, &b| {
        centroids[a as usize][axis].total_cmp(&centroids[b as usize][axis])
    });

    nodes.push(BvhNode {
        bounds,
        left_or_first: 0,
        count_or_right: 0,
        is_leaf: false,
    });
    let left = build_bvh_rec(nodes, tri_order, start, mid, tri_bounds, centroids);
    let right = build_bvh_rec(nodes, tri_order, mid, end, tri_bounds, centroids);
    nodes[node_idx as usize].left_or_first = left;
    nodes[node_idx as usize].count_or_right = right;
    node_idx
}

#[inline]
fn world_aabb_of_box(local: Aabb, m: &Transform3x4) -> Aabb {
    let center = [
        0.5 * (local[0][0] + local[1][0]),
        0.5 * (local[0][1] + local[1][1]),
        0.5 * (local[0][2] + local[1][2]),
    ];
    let half = [
        0.5 * (local[1][0] - local[0][0]),
        0.5 * (local[1][1] - local[0][1]),
        0.5 * (local[1][2] - local[0][2]),
    ];
    let wc = transform_point(m, center);
    let mut wh = [0.0_f32; 3];
    for r in 0..3 {
        wh[r] = m[r][0].abs() * half[0] + m[r][1].abs() * half[1] + m[r][2].abs() * half[2];
    }
    [
        [wc[0] - wh[0], wc[1] - wh[1], wc[2] - wh[2]],
        [wc[0] + wh[0], wc[1] + wh[1], wc[2] + wh[2]],
    ]
}

#[inline]
fn aabb_min_dist_sq(a: Aabb, b: Aabb) -> f32 {
    let mut d2 = 0.0_f32;
    for k in 0..3 {
        if a[1][k] < b[0][k] {
            let d = b[0][k] - a[1][k];
            d2 += d * d;
        } else if b[1][k] < a[0][k] {
            let d = a[0][k] - b[1][k];
            d2 += d * d;
        }
    }
    d2
}

fn bvh_pair_query(
    bvh_a: &MeshBvh,
    mesh_a: &TriMesh,
    tf_a: &Transform3x4,
    idx_a: u32,
    bvh_b: &MeshBvh,
    mesh_b: &TriMesh,
    tf_b: &Transform3x4,
    idx_b: u32,
    best_d2: &mut f32,
    best_pts: &mut Option<([f32; 3], [f32; 3])>,
) {
    if *best_d2 <= 1e-10 {
        return;
    }
    let na = &bvh_a.nodes[idx_a as usize];
    let nb = &bvh_b.nodes[idx_b as usize];
    let wb_a = world_aabb_of_box(na.bounds, tf_a);
    let wb_b = world_aabb_of_box(nb.bounds, tf_b);
    if aabb_min_dist_sq(wb_a, wb_b) >= *best_d2 {
        return;
    }

    if na.is_leaf && nb.is_leaf {
        let a_start = na.left_or_first as usize;
        let a_end = a_start + na.count_or_right as usize;
        let b_start = nb.left_or_first as usize;
        let b_end = b_start + nb.count_or_right as usize;
        for &ta_idx in &bvh_a.tri_order[a_start..a_end] {
            let ia = mesh_a.indices[ta_idx as usize];
            let ta = [
                transform_point(tf_a, mesh_a.positions[ia[0] as usize]),
                transform_point(tf_a, mesh_a.positions[ia[1] as usize]),
                transform_point(tf_a, mesh_a.positions[ia[2] as usize]),
            ];
            for &tb_idx in &bvh_b.tri_order[b_start..b_end] {
                let ib = mesh_b.indices[tb_idx as usize];
                let tb = [
                    transform_point(tf_b, mesh_b.positions[ib[0] as usize]),
                    transform_point(tf_b, mesh_b.positions[ib[1] as usize]),
                    transform_point(tf_b, mesh_b.positions[ib[2] as usize]),
                ];
                let (pa, pb, d2) = tri_tri_closest_points(ta, tb);
                if d2 < *best_d2 {
                    *best_d2 = d2;
                    *best_pts = Some((pa, pb));
                    if d2 <= 1e-10 {
                        return;
                    }
                }
            }
        }
        return;
    }

    if na.is_leaf || (!nb.is_leaf && aabb_volume(nb.bounds) >= aabb_volume(na.bounds)) {
        let bl = nb.left_or_first;
        let br = nb.count_or_right;
        let dl = aabb_min_dist_sq(wb_a, world_aabb_of_box(bvh_b.nodes[bl as usize].bounds, tf_b));
        let dr = aabb_min_dist_sq(wb_a, world_aabb_of_box(bvh_b.nodes[br as usize].bounds, tf_b));
        if dl <= dr {
            bvh_pair_query(bvh_a, mesh_a, tf_a, idx_a, bvh_b, mesh_b, tf_b, bl, best_d2, best_pts);
            bvh_pair_query(bvh_a, mesh_a, tf_a, idx_a, bvh_b, mesh_b, tf_b, br, best_d2, best_pts);
        } else {
            bvh_pair_query(bvh_a, mesh_a, tf_a, idx_a, bvh_b, mesh_b, tf_b, br, best_d2, best_pts);
            bvh_pair_query(bvh_a, mesh_a, tf_a, idx_a, bvh_b, mesh_b, tf_b, bl, best_d2, best_pts);
        }
    } else {
        let al = na.left_or_first;
        let ar = na.count_or_right;
        let dl = aabb_min_dist_sq(world_aabb_of_box(bvh_a.nodes[al as usize].bounds, tf_a), wb_b);
        let dr = aabb_min_dist_sq(world_aabb_of_box(bvh_a.nodes[ar as usize].bounds, tf_a), wb_b);
        if dl <= dr {
            bvh_pair_query(bvh_a, mesh_a, tf_a, al, bvh_b, mesh_b, tf_b, idx_b, best_d2, best_pts);
            bvh_pair_query(bvh_a, mesh_a, tf_a, ar, bvh_b, mesh_b, tf_b, idx_b, best_d2, best_pts);
        } else {
            bvh_pair_query(bvh_a, mesh_a, tf_a, ar, bvh_b, mesh_b, tf_b, idx_b, best_d2, best_pts);
            bvh_pair_query(bvh_a, mesh_a, tf_a, al, bvh_b, mesh_b, tf_b, idx_b, best_d2, best_pts);
        }
    }
}

#[inline]
fn aabb_volume(b: Aabb) -> f32 {
    (b[1][0] - b[0][0]).max(0.0) * (b[1][1] - b[0][1]).max(0.0) * (b[1][2] - b[0][2]).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transform_from_translation_euler_deg;

    fn box_mesh(min: [f32; 3], max: [f32; 3]) -> TriMesh {
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
        TriMesh::from_indexed(positions, indices).expect("valid box")
    }

    fn open_cup_mesh() -> TriMesh {
        // Outer [0..20, 0..20, 0..20] with open top pocket [4..16, 4..16, 4..20]
        let outer = box_mesh([0.0, 0.0, 0.0], [20.0, 20.0, 20.0]);
        let mut pos = outer.positions;
        let mut idx = vec![
            [0, 2, 1], [0, 3, 2], // bottom
            [0, 1, 5], [0, 5, 4], // front
            [1, 2, 6], [1, 6, 5], // right
            [2, 3, 7], [2, 7, 6], // back
            [3, 0, 4], [3, 4, 7], // left
        ];
        let base = pos.len() as u32;
        pos.extend_from_slice(&[
            [4.0, 4.0, 4.0], [16.0, 4.0, 4.0], [16.0, 16.0, 4.0], [4.0, 16.0, 4.0],
            [4.0, 4.0, 20.0], [16.0, 4.0, 20.0], [16.0, 16.0, 20.0], [4.0, 16.0, 20.0],
        ]);
        // Inner cavity walls (inward-facing) + top rim quads
        idx.extend_from_slice(&[
            [base, base + 1, base + 2], [base, base + 2, base + 3],
            [base, base + 5, base + 1], [base, base + 4, base + 5],
            [base + 1, base + 6, base + 2], [base + 1, base + 5, base + 6],
            [base + 2, base + 7, base + 3], [base + 2, base + 6, base + 7],
            [base + 3, base + 4, base], [base + 3, base + 7, base + 4],
            // Top rim connecting outer [4..7] to inner [base+4..base+7]
            [4, 5, base + 5], [4, base + 5, base + 4],
            [5, 6, base + 6], [5, base + 6, base + 5],
            [6, 7, base + 7], [6, base + 7, base + 6],
            [7, 4, base + 4], [7, base + 4, base + 7],
        ]);
        TriMesh::from_indexed(pos, idx).expect("valid cup")
    }

    fn torus_mesh(major_r: f32, minor_r: f32, segs: usize, sides: usize) -> TriMesh {
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
        TriMesh::from_indexed(pos, idx).expect("valid torus")
    }

    #[test]
    fn bitgrid3d_shift_overlap_stamp_and_unstamp_exact() {
        let mut tray = BitGrid3D::new(140, 20, 20, None).expect("tray");
        let mut mask = BitGrid3D::new(70, 5, 4, None).expect("mask");
        mask.set(0, 0, 0);
        mask.set(63, 2, 1);
        mask.set(69, 4, 3);

        // Stamp across a 64-bit word boundary (ox = 61 puts bit 69 at 130)
        assert!(!tray.overlaps_at(&mask, 61, 3, 2));
        tray.stamp_at(&mask, 61, 3, 2);
        assert!(tray.get(61, 3, 2));
        assert!(tray.get(124, 5, 3));
        assert!(tray.get(130, 7, 5));
        assert_eq!(tray.count_ones(), 3);
        assert!(tray.overlaps_at(&mask, 61, 3, 2));
        // Shifting by 1 voxel clears overlap
        assert!(!tray.overlaps_at(&mask, 62, 3, 2));
        // Out of bounds returns overlap = true
        assert!(tray.overlaps_at(&mask, 80, 3, 2));

        let mut hmap = HeightMap2D::new(140, 20, None).expect("hmap");
        hmap.update_from_stamp(&mask, 61, 3, 2);
        assert_eq!(hmap.max_height_in_rect(61, 3, 70, 5), 6);

        tray.unstamp_at(&mask, 61, 3, 2);
        assert_eq!(tray.count_ones(), 0);
        hmap.rebuild_from_grid(&tray);
        assert_eq!(hmap.max_height_in_rect(0, 0, 140, 20), 0);
    }

    #[test]
    fn voxelizes_closed_cube_open_cup_torus_flipped_and_leaky_mesh() {
        let id_rot = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let cube = box_mesh([0.0, 0.0, 0.0], [10.0, 10.0, 10.0]);
        let (cube_grid, _, _) =
            voxelize_mesh_flood_fill(&cube, &id_rot, 1.0, [0, 0, 0], false, None, None).expect("cube");
        assert!(cube_grid.get(5, 5, 5), "interior of closed cube must be solid");

        // Flipped normals must voxelize identically via flood-fill.
        let mut flipped_idx = cube.indices.clone();
        for tri in &mut flipped_idx {
            tri.swap(1, 2);
        }
        let flipped = TriMesh::from_indexed(cube.positions.clone(), flipped_idx).expect("flipped");
        let (flip_grid, _, _) =
            voxelize_mesh_flood_fill(&flipped, &id_rot, 1.0, [0, 0, 0], false, None, None).expect("flipped");
        assert_eq!(cube_grid, flip_grid);

        // Open cup: walls solid, interior pocket empty when fill_open_cavities=false, filled when true.
        let cup = open_cup_mesh();
        let (cup_open, _, _) =
            voxelize_mesh_flood_fill(&cup, &id_rot, 2.0, [0, 0, 0], false, None, None).expect("cup open");
        assert!(!cup_open.get(5, 5, 6), "open cup pocket must stay empty");
        assert!(cup_open.get(5, 5, 1), "cup floor must be solid");

        let (cup_closed, _, _) =
            voxelize_mesh_flood_fill(&cup, &id_rot, 2.0, [0, 0, 0], true, None, None).expect("cup closed");
        assert!(cup_closed.get(5, 5, 6), "fill_open_cavities fills cup pocket");

        // Torus: center hole stays empty, ring stays solid.
        let torus = torus_mesh(12.0, 4.0, 24, 12);
        let (torus_grid, _, _) =
            voxelize_mesh_flood_fill(&torus, &id_rot, 2.0, [0, 0, 0], false, None, None).expect("torus");
        let cx = torus_grid.nx / 2;
        let cy = torus_grid.ny / 2;
        let cz = torus_grid.nz / 2;
        assert!(!torus_grid.get(cx, cy, cz), "torus center hole must remain open");

        // Leaky mesh (missing one face): flood-fill never produces unbounded solid streaks.
        let leaky = TriMesh::from_indexed(cube.positions.clone(), cube.indices[..10].to_vec()).expect("leaky");
        let (leaky_grid, _, _) =
            voxelize_mesh_flood_fill(&leaky, &id_rot, 1.0, [0, 0, 0], false, None, None).expect("leaky");
        assert!(leaky_grid.count_ones() > 0 && leaky_grid.count_ones() <= cube_grid.count_ones());
    }

    #[test]
    fn edt_3d_matches_brute_force_uniform_and_anisotropic() {
        let mut solid = BitGrid3D::new(9, 9, 9, None).expect("grid");
        solid.set(4, 4, 4);
        solid.set(2, 6, 3);

        let mut scratch = EdtScratch::default();
        edt_3d_squared(&solid, [1.0, 1.0, 1.0], &mut scratch, None, None).expect("edt");

        for z in 0..9 {
            for y in 0..9 {
                for x in 0..9 {
                    let d2_a = ((x as f32 - 4.0).powi(2) + (y as f32 - 4.0).powi(2) + (z as f32 - 4.0).powi(2))
                        .min((x as f32 - 2.0).powi(2) + (y as f32 - 6.0).powi(2) + (z as f32 - 3.0).powi(2));
                    let got = scratch.dist[(z * 9 + y) * 9 + x];
                    assert!((got - d2_a).abs() < 1e-4, "mismatch at ({x},{y},{z}): {got} vs {d2_a}");
                }
            }
        }

        // Anisotropic dilation: radius_xy = 2.0, radius_z = 4.0 from center (4,4,4).
        let mut single = BitGrid3D::new(9, 9, 11, None).expect("single");
        single.set(4, 4, 5);
        let dilated = dilate_clearance_mask(&single, 2.0, 4.0, &mut scratch, None, None).expect("dilate");
        assert!(dilated.get(6, 4, 5));
        assert!(!dilated.get(7, 4, 5));
        assert!(dilated.get(4, 4, 9), "z + 4 must be inside r_z=4.0");
        assert!(!dilated.get(4, 4, 10), "z + 5 must be outside r_z=4.0");
    }

    #[test]
    fn symmetry_pruning_collapses_cube_and_keeps_asymmetric_bracket() {
        let cube = box_mesh([0.0, 0.0, 0.0], [16.0, 16.0, 16.0]);
        let id = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let cube_cands = generate_candidate_rotations(
            &cube,
            id,
            RotationConstraint::Free,
            FreeAngleSet::Octahedral24,
            ZOnlyStep::Deg90,
            false,
        );
        assert_eq!(cube_cands.len(), 24);
        let cube_pruned = prune_symmetric_rotations(&cube, &cube_cands, 4.0, None, None).expect("prune");
        assert_eq!(cube_pruned.len(), 1, "cube should collapse 24 -> 1");

        // Asymmetric L-stepped part (10x20x30) should retain multiple distinct orientations.
        let l_pos = vec![
            [0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 6.0, 0.0], [6.0, 6.0, 0.0], [6.0, 14.0, 0.0], [0.0, 14.0, 0.0],
            [0.0, 0.0, 30.0], [20.0, 0.0, 30.0], [20.0, 6.0, 30.0], [6.0, 6.0, 30.0], [6.0, 14.0, 30.0], [0.0, 14.0, 30.0],
        ];
        let l_idx = vec![
            [0, 2, 1], [0, 3, 2], [0, 4, 3], [0, 5, 4],
            [6, 7, 8], [6, 8, 9], [6, 9, 10], [6, 10, 11],
            [0, 1, 7], [0, 7, 6], [1, 2, 8], [1, 8, 7],
            [2, 3, 9], [2, 9, 8], [3, 4, 10], [3, 10, 9],
            [4, 5, 11], [4, 11, 10], [5, 0, 6], [5, 6, 11],
        ];
        let l_mesh = TriMesh::from_indexed(l_pos, l_idx).expect("L mesh");
        let l_cands = generate_candidate_rotations(
            &l_mesh,
            id,
            RotationConstraint::Free,
            FreeAngleSet::Octahedral24,
            ZOnlyStep::Deg90,
            false,
        );
        let l_pruned = prune_symmetric_rotations(&l_mesh, &l_cands, 2.0, None, None).expect("prune L");
        assert!(l_pruned.len() >= 12, "asymmetric L should keep >= 12 orientations, got {}", l_pruned.len());
    }

    #[test]
    fn local_bvh_closest_distance_matches_brute_force_under_rigid_transforms() {
        let mesh_a = torus_mesh(12.0, 3.0, 16, 10);
        let mesh_b = box_mesh([0.0, 0.0, 0.0], [10.0, 8.0, 6.0]);
        let bvh_a = MeshBvh::build(&mesh_a);
        let bvh_b = MeshBvh::build(&mesh_b);

        let tf_a = transform_from_translation_euler_deg([5.0, -3.0, 10.0], [25.0, 10.0, -15.0]);
        let tf_b = transform_from_translation_euler_deg([22.0, 4.0, 11.0], [-10.0, 35.0, 40.0]);

        let (_, _, bvh_dist) =
            MeshBvh::min_distance_between(&bvh_a, &mesh_a, &tf_a, &bvh_b, &mesh_b, &tf_b, 50.0)
                .expect("within 50 mm");

        let mut brute_d2 = f32::INFINITY;
        for ia in &mesh_a.indices {
            let ta = [
                transform_point(&tf_a, mesh_a.positions[ia[0] as usize]),
                transform_point(&tf_a, mesh_a.positions[ia[1] as usize]),
                transform_point(&tf_a, mesh_a.positions[ia[2] as usize]),
            ];
            for ib in &mesh_b.indices {
                let tb = [
                    transform_point(&tf_b, mesh_b.positions[ib[0] as usize]),
                    transform_point(&tf_b, mesh_b.positions[ib[1] as usize]),
                    transform_point(&tf_b, mesh_b.positions[ib[2] as usize]),
                ];
                let (_, _, d2) = tri_tri_closest_points(ta, tb);
                brute_d2 = brute_d2.min(d2);
            }
        }
        let brute_dist = brute_d2.sqrt();
        assert!((bvh_dist - brute_dist).abs() < 1e-4, "bvh={bvh_dist} vs brute={brute_dist}");
    }

    #[test]
    fn memory_estimator_auto_fit_oom_guard_cpu_budget_and_cancel_work() {
        assert_eq!(CpuBudget::Balanced.resolve_for_cores(24), 18);
        assert_eq!(CpuBudget::Balanced.resolve_for_cores(16), 12);
        assert_eq!(CpuBudget::Balanced.resolve_for_cores(8), 6);
        assert_eq!(CpuBudget::Balanced.resolve_for_cores(4), 2);
        assert_eq!(CpuBudget::Quiet.resolve_for_cores(8), 4);
        assert_eq!(CpuBudget::Max.resolve_for_cores(8), 7);

        let parts = [([40.0, 40.0, 40.0], 24), ([20.0, 30.0, 15.0], 8)];
        let est_fine = estimate_voxel_memory_bytes([800, 800, 900], &parts, 0.5, 2.5, 12);
        let est_fast = estimate_voxel_memory_bytes([200, 200, 225], &parts, 2.0, 2.5, 12);
        assert!(est_fine.total_bytes > est_fast.total_bytes * 10);

        let fitted = auto_fit_resolution_mm([400.0, 400.0, 450.0], &parts, 0.5, 2.5, 12, 64 * 1024 * 1024);
        assert!(fitted > 0.5 && fitted <= 4.0);

        // Hard memory limit cleanly returns Err without panicking.
        assert!(BitGrid3D::new(400, 400, 450, Some(1024)).is_err());

        // Cooperative cancellation cleanly aborts voxelization.
        let cube = box_mesh([0.0, 0.0, 0.0], [20.0, 20.0, 20.0]);
        let cancel = AtomicBool::new(true);
        let id = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        assert!(voxelize_mesh_flood_fill(&cube, &id, 1.0, [0, 0, 0], false, None, Some(&cancel)).is_err());
    }
}
