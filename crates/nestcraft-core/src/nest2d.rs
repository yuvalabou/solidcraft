//! Fast 2D bitmap + Euclidean-dilation Bottom-Left-Fill auto-nester.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::Result;
use crate::model::{BuildJob, InstanceId, RotationConstraint, Transform3x4, TriMesh};

/// Where 2D auto-nest places the parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nest2dTarget {
    /// Pack inside the active machine's XY build platform (overflow placed to +X outside).
    ToPlatform,
    /// Pack in-place starting at the parts' current XY origin (unconstrained by platform walls).
    InPlaceOutsidePlatform,
}

/// Configuration for 2D auto-nesting.
#[derive(Debug, Clone, PartialEq)]
pub struct Nest2dConfig {
    /// Minimum XY clearance between parts in millimetres.
    pub min_distance_mm: f32,
    /// Raster grid resolution in millimetres (`0.5`, `1.0`, `2.0`, etc.).
    pub resolution_mm: f32,
    /// Margin from machine X/Y walls in millimetres (used for `ToPlatform`).
    pub wall_margin_mm: f32,
    /// Candidate Z-rotation step in degrees (`0` = no rotation, `90` = 4 angles, `45` = 8, `15` = 24).
    pub rotation_step_deg: u16,
    /// Target placement mode.
    pub target: Nest2dTarget,
    /// If `true` and `selected` is non-empty, only pack selected parts (keeping others fixed).
    pub selected_only: bool,
    /// Drop each nested part so its `min_z` rests on `job.min_z_mm`.
    pub drop_to_min_z: bool,
    /// Avoid enabled `NoBuildZone` XY footprint when `target == ToPlatform`.
    pub avoid_no_build_zone: bool,
    /// Pack using each part's solid minimum XY bounding box instead of concave mesh silhouette.
    pub use_min_bbox: bool,
}

impl Default for Nest2dConfig {
    fn default() -> Self {
        Self {
            min_distance_mm: 2.5,
            resolution_mm: 1.0,
            wall_margin_mm: 5.0,
            rotation_step_deg: 90,
            target: Nest2dTarget::ToPlatform,
            selected_only: false,
            drop_to_min_z: true,
            avoid_no_build_zone: true,
            use_min_bbox: false,
        }
    }
}

/// Summary returned by [`nest_2d`].
#[derive(Debug, Clone, PartialEq)]
pub struct Nest2dResult {
    /// Number of parts placed inside the target region.
    pub placed_count: usize,
    /// Number of parts that did not fit on the platform and were placed in the overflow strip.
    pub overflow_count: usize,
    /// Total nested footprint area as a percentage of the machine bed XY area.
    pub used_area_pct: f64,
    /// Wall-clock runtime in milliseconds.
    pub elapsed_ms: u64,
}

/// Run 2D auto-nesting on `job` in-place.
pub fn nest_2d(
    job: &mut BuildJob,
    cfg: &Nest2dConfig,
    selected: Option<&HashSet<InstanceId>>,
) -> Result<Nest2dResult> {
    let t0 = Instant::now();
    let res = cfg.resolution_mm.clamp(0.25, 10.0);
    let min_dist = cfg.min_distance_mm.max(0.0);
    let dilate_cells = (min_dist / res).ceil() as isize;

    let ([bed_min_x, bed_min_y], [bed_max_x, bed_max_y]) = job.bed_xy_bounds();
    let bed_area_mm2 = (f64::from(bed_max_x - bed_min_x) * f64::from(bed_max_y - bed_min_y)).max(1.0);

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
        let is_movable = !inst.locked && sel_filter.is_none_or(|s| s.contains(&inst.id));
        if is_movable {
            movable_indices.push(idx);
        } else {
            fixed_indices.push(idx);
        }
    }

    if movable_indices.is_empty() {
        return Ok(Nest2dResult {
            placed_count: 0,
            overflow_count: 0,
            used_area_pct: 0.0,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        });
    }

    // Determine target XY packing rectangle.
    let (tray_min_x, tray_min_y, tray_max_x, tray_max_y) = match cfg.target {
        Nest2dTarget::ToPlatform => {
            let wm = cfg.wall_margin_mm.max(0.0);
            (
                bed_min_x + wm,
                bed_min_y + wm,
                (bed_max_x - wm).max(bed_min_x + wm + res),
                (bed_max_y - wm).max(bed_min_y + wm + res),
            )
        }
        Nest2dTarget::InPlaceOutsidePlatform => {
            let mut ox = f32::INFINITY;
            let mut oy = f32::INFINITY;
            let mut total_area = 0.0_f32;
            for &idx in &movable_indices {
                let inst = &job.instances[idx];
                if let Some(proto) = job.part(inst.part_id) {
                    let wb = inst.world_bounds(proto.mesh.bounds);
                    ox = ox.min(wb[0][0]);
                    oy = oy.min(wb[0][1]);
                    let dx = (wb[1][0] - wb[0][0]).max(1.0) + min_dist;
                    let dy = (wb[1][1] - wb[0][1]).max(1.0) + min_dist;
                    total_area += dx * dy;
                }
            }
            let side = (total_area.sqrt() * 2.2).max(250.0);
            if !ox.is_finite() {
                ox = bed_max_x + 20.0;
                oy = bed_min_y;
            }
            (ox, oy, ox + side, oy + side)
        }
    };

    let grid_w = (((tray_max_x - tray_min_x) / res).ceil() as usize).clamp(4, 2000);
    let grid_h = (((tray_max_y - tray_min_y) / res).ceil() as usize).clamp(4, 2000);
    let mut tray = BitGrid::new(grid_w, grid_h);

    // Pre-mark fixed instances (locked or unselected in SelectedOnly mode) when packing to platform.
    if cfg.target == Nest2dTarget::ToPlatform {
        for &idx in &fixed_indices {
            let inst = &job.instances[idx];
            if let Some(proto) = job.part(inst.part_id) {
                let mask = rasterize_mesh_xy(&proto.mesh, inst.transform, res);
                let ox = ((mask.world_min[0] - tray_min_x) / res).floor() as isize;
                let oy = ((mask.world_min[1] - tray_min_y) / res).floor() as isize;
                tray.stamp_dilated(&mask, ox, oy, dilate_cells);
            }
        }
        if cfg.avoid_no_build_zone
            && let Some(zone) = &job.no_build_zone
            && zone.enabled
        {
            let nbz_tr = zone.transform();
            let mask = rasterize_mesh_xy(&zone.mesh, nbz_tr, res);
            let nbz_dilate = ((min_dist + zone.clearance_mm.max(0.0)) / res).ceil() as isize;
            let ox = ((mask.world_min[0] - tray_min_x) / res).floor() as isize;
            let oy = ((mask.world_min[1] - tray_min_y) / res).floor() as isize;
            tray.stamp_dilated(&mask, ox, oy, nbz_dilate);
        }
    }

    // Sort movable instances largest XY area first.
    movable_indices.sort_by(|&a, &b| {
        let area_of = |i: usize| -> f32 {
            let inst = &job.instances[i];
            job.part(inst.part_id)
                .map(|p| {
                    let wb = inst.world_bounds(p.mesh.bounds);
                    (wb[1][0] - wb[0][0]) * (wb[1][1] - wb[0][1])
                })
                .unwrap_or(0.0)
        };
        area_of(b).total_cmp(&area_of(a))
    });

    let angles_deg: Vec<f32> = if cfg.rotation_step_deg == 0 || cfg.rotation_step_deg >= 360 {
        vec![0.0]
    } else {
        let step = f32::from(cfg.rotation_step_deg.max(5));
        let n = (360.0 / step).floor() as usize;
        (0..n).map(|k| k as f32 * step).collect()
    };

    let mut placed_count = 0usize;
    let mut overflow_count = 0usize;
    let mut total_solid_cells = 0usize;
    let mut overflow_x = bed_max_x + 15.0;
    let mut overflow_y = bed_min_y;
    let mut overflow_row_h = 0.0_f32;

    // Build per-instance candidate angle lists (including minimum-area OBB alignment for Free/ZOnly).
    let mut instance_angles: Vec<(usize, Vec<f32>)> = Vec::with_capacity(movable_indices.len());
    let mut obb_cache: HashMap<(u32, [u32; 6]), f32> = HashMap::new();

    for &idx in &movable_indices {
        let inst = &job.instances[idx];
        if inst.constraint == RotationConstraint::Fixed {
            instance_angles.push((idx, vec![0.0]));
            continue;
        }
        let mut cands = angles_deg.clone();
        if let Some(proto) = job.part(inst.part_id) {
            let rot_key = [
                inst.transform[0][0].to_bits(),
                inst.transform[0][1].to_bits(),
                inst.transform[0][2].to_bits(),
                inst.transform[1][0].to_bits(),
                inst.transform[1][1].to_bits(),
                inst.transform[1][2].to_bits(),
            ];
            let obb_deg = *obb_cache.entry((proto.id.0, rot_key)).or_insert_with(|| {
                min_area_rect_angle_rad(&proto.mesh, inst.transform)
                    .map(f32::to_degrees)
                    .unwrap_or(0.0)
            });
            for extra in [obb_deg, obb_deg + 90.0] {
                let norm = extra.rem_euclid(360.0);
                if cands.iter().all(|&a| (a - norm).abs() > 1.0 && (a - norm).abs() < 359.0) {
                    cands.push(norm);
                }
            }
        }
        instance_angles.push((idx, cands));
    }

    // Deduplicate (PartId, rot_key, quantized_angle) and rasterize all unique masks in parallel.
    type MaskKey = (u32, [u32; 6], i32);
    let mut unique_jobs: Vec<(MaskKey, std::sync::Arc<TriMesh>, Transform3x4, f32)> = Vec::new();
    let mut seen_keys: HashSet<MaskKey> = HashSet::new();

    for &(idx, ref cands) in &instance_angles {
        let inst = &job.instances[idx];
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let rot_key = [
            inst.transform[0][0].to_bits(),
            inst.transform[0][1].to_bits(),
            inst.transform[0][2].to_bits(),
            inst.transform[1][0].to_bits(),
            inst.transform[1][1].to_bits(),
            inst.transform[1][2].to_bits(),
        ];
        for &deg in cands {
            let q = (deg * 100.0).round() as i32;
            let key = (proto.id.0, rot_key, q);
            if seen_keys.insert(key) {
                unique_jobs.push((key, proto.mesh.clone(), inst.transform, deg));
            }
        }
    }

    let use_min_bbox = cfg.use_min_bbox;
    let mask_cache: HashMap<MaskKey, (Transform3x4, std::sync::Arc<BitMask>)> = if unique_jobs.len() <= 2 {
        unique_jobs
            .into_iter()
            .map(|(key, mesh, tr, deg)| {
                let rot_tr = rotate_z_around_origin(tr, deg.to_radians());
                let mask = std::sync::Arc::new(if use_min_bbox {
                    rasterize_bbox_xy(&mesh, rot_tr, res)
                } else {
                    rasterize_mesh_xy(&mesh, rot_tr, res)
                });
                (key, (rot_tr, mask))
            })
            .collect()
    } else {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(unique_jobs.len());
        let chunk_size = unique_jobs.len().div_ceil(threads);
        let mut merged = HashMap::with_capacity(unique_jobs.len());
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for chunk in unique_jobs.chunks(chunk_size) {
                handles.push(s.spawn(move || {
                    chunk
                        .iter()
                        .map(|(key, mesh, tr, deg)| {
                            let rot_tr = rotate_z_around_origin(*tr, deg.to_radians());
                            let mask = std::sync::Arc::new(if use_min_bbox {
                                rasterize_bbox_xy(mesh, rot_tr, res)
                            } else {
                                rasterize_mesh_xy(mesh, rot_tr, res)
                            });
                            (*key, (rot_tr, mask))
                        })
                        .collect::<Vec<_>>()
                }));
            }
            for h in handles {
                if let Ok(items) = h.join() {
                    for (k, v) in items {
                        merged.insert(k, v);
                    }
                }
            }
        });
        merged
    };

    for (idx, cands) in instance_angles {
        let (part_id, orig_tr) = {
            let inst = &job.instances[idx];
            (inst.part_id, inst.transform)
        };
        let Some(proto) = job.part(part_id).cloned() else {
            continue;
        };
        let rot_key = [
            orig_tr[0][0].to_bits(),
            orig_tr[0][1].to_bits(),
            orig_tr[0][2].to_bits(),
            orig_tr[1][0].to_bits(),
            orig_tr[1][1].to_bits(),
            orig_tr[1][2].to_bits(),
        ];

        let mut cand_masks: Vec<(Transform3x4, std::sync::Arc<BitMask>)> = Vec::with_capacity(cands.len());
        for &deg in &cands {
            let key = (proto.id.0, rot_key, (deg * 100.0).round() as i32);
            if let Some((rot_tr, mask)) = mask_cache.get(&key)
                && mask.w <= grid_w
                && mask.h <= grid_h
            {
                cand_masks.push((*rot_tr, mask.clone()));
            }
        }

        let best: Option<(usize, usize, Transform3x4, std::sync::Arc<BitMask>)> = if cand_masks.len() <= 1 {
            cand_masks.into_iter().next().and_then(|(rot_tr, mask)| {
                tray.find_first_fit(&mask).map(|(gx, gy)| (gx, gy, rot_tr, mask))
            })
        } else {
            let tray_ref = &tray;
            std::thread::scope(|s| {
                let handles: Vec<_> = cand_masks
                    .iter()
                    .map(|(rot_tr, mask)| {
                        let mask_ref = mask.as_ref();
                        let rt = *rot_tr;
                        s.spawn(move || {
                            tray_ref.find_first_fit(mask_ref).map(|(gx, gy)| {
                                let score = (gy + mask_ref.h) * 4096 + gx;
                                (score, gx, gy, rt, mask.clone())
                            })
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .filter_map(|h| h.join().ok().flatten())
                    .min_by_key(|(score, ..)| *score)
                    .map(|(_, gx, gy, rt, m)| (gx, gy, rt, m))
            })
        };

        if let Some((gx, gy, mut new_tr, mask)) = best {
            let target_x = tray_min_x + gx as f32 * res;
            let target_y = tray_min_y + gy as f32 * res;
            new_tr[0][3] += target_x - mask.world_min[0];
            new_tr[1][3] += target_y - mask.world_min[1];
            if cfg.drop_to_min_z {
                let wb_z = world_z_min(&proto.mesh, new_tr);
                new_tr[2][3] += job.min_z_mm - wb_z;
            }
            job.instances[idx].transform = new_tr;
            tray.stamp_dilated(&mask, gx as isize, gy as isize, dilate_cells);
            total_solid_cells += mask.solid_count;
            placed_count += 1;
        } else {
            // Overflow strip to +X outside the machine bed.
            let mut new_tr = orig_tr;
            let wb = job.instances[idx].world_bounds(proto.mesh.bounds);
            let pw = (wb[1][0] - wb[0][0]).max(1.0);
            let pd = (wb[1][1] - wb[0][1]).max(1.0);
            if overflow_y + pd > bed_max_y && overflow_row_h > 0.0 {
                overflow_y = bed_min_y;
                overflow_x += overflow_row_h + min_dist.max(5.0);
                overflow_row_h = 0.0;
            }
            new_tr[0][3] += overflow_x - wb[0][0];
            new_tr[1][3] += overflow_y - wb[0][1];
            if cfg.drop_to_min_z {
                new_tr[2][3] += job.min_z_mm - wb[0][2];
            }
            job.instances[idx].transform = new_tr;
            overflow_y += pd + min_dist.max(5.0);
            overflow_row_h = overflow_row_h.max(pw);
            overflow_count += 1;
        }
    }

    let used_area_pct = ((total_solid_cells as f64) * f64::from(res) * f64::from(res) / bed_area_mm2) * 100.0;
    Ok(Nest2dResult {
        placed_count,
        overflow_count,
        used_area_pct,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    })
}

fn rotate_z_around_origin(tr: Transform3x4, angle_rad: f32) -> Transform3x4 {
    if angle_rad.abs() < 1e-6 {
        return tr;
    }
    let (s, c) = angle_rad.sin_cos();
    let mut out = tr;
    for col in 0..3 {
        let x = tr[0][col];
        let y = tr[1][col];
        out[0][col] = c * x - s * y;
        out[1][col] = s * x + c * y;
    }
    out[0][3] = 0.0;
    out[1][3] = 0.0;
    out
}

fn world_z_min(mesh: &TriMesh, tr: Transform3x4) -> f32 {
    let mut min_z = f32::INFINITY;
    for p in &mesh.positions {
        let z = tr[2][0] * p[0] + tr[2][1] * p[1] + tr[2][2] * p[2] + tr[2][3];
        min_z = min_z.min(z);
    }
    if min_z.is_finite() { min_z } else { 0.0 }
}

struct BitMask {
    w: usize,
    h: usize,
    words_per_row: usize,
    rows: Vec<u64>,
    solid_count: usize,
    world_min: [f32; 2],
}

impl BitMask {
    fn new(w: usize, h: usize, world_min: [f32; 2]) -> Self {
        let words_per_row = w.div_ceil(64).max(1);
        Self {
            w,
            h,
            words_per_row,
            rows: vec![0u64; h * words_per_row],
            solid_count: 0,
            world_min,
        }
    }

    fn set(&mut self, x: usize, y: usize) {
        if x < self.w && y < self.h {
            let idx = y * self.words_per_row + (x >> 6);
            let bit = 1u64 << (x & 63);
            if (self.rows[idx] & bit) == 0 {
                self.rows[idx] |= bit;
                self.solid_count += 1;
            }
        }
    }

    fn get(&self, x: usize, y: usize) -> bool {
        if x >= self.w || y >= self.h {
            return false;
        }
        (self.rows[y * self.words_per_row + (x >> 6)] & (1u64 << (x & 63))) != 0
    }
}

struct BitGrid {
    w: usize,
    h: usize,
    words_per_row: usize,
    rows: Vec<u64>,
}

impl BitGrid {
    fn new(w: usize, h: usize) -> Self {
        let words_per_row = w.div_ceil(64).max(1);
        Self {
            w,
            h,
            words_per_row,
            rows: vec![0u64; h * words_per_row],
        }
    }

    fn set(&mut self, x: usize, y: usize) {
        if x < self.w && y < self.h {
            self.rows[y * self.words_per_row + (x >> 6)] |= 1u64 << (x & 63);
        }
    }

    fn find_first_fit(&self, mask: &BitMask) -> Option<(usize, usize)> {
        if mask.w > self.w || mask.h > self.h {
            return None;
        }
        let max_x = self.w - mask.w;
        let max_y = self.h - mask.h;
        for gy in 0..=max_y {
            for gx in 0..=max_x {
                if self.fits_at(mask, gx, gy) {
                    return Some((gx, gy));
                }
            }
        }
        None
    }

    fn fits_at(&self, mask: &BitMask, gx: usize, gy: usize) -> bool {
        let word_off = gx >> 6;
        let bit_shift = (gx & 63) as u32;
        for my in 0..mask.h {
            let tray_row = (gy + my) * self.words_per_row;
            let mask_row = my * mask.words_per_row;
            for mw in 0..mask.words_per_row {
                let m = mask.rows[mask_row + mw];
                if m == 0 {
                    continue;
                }
                let tw = self.rows[tray_row + word_off + mw];
                if (tw & (m << bit_shift)) != 0 {
                    return false;
                }
                if bit_shift > 0 && word_off + mw + 1 < self.words_per_row {
                    let tw_next = self.rows[tray_row + word_off + mw + 1];
                    if (tw_next & (m >> (64 - bit_shift))) != 0 {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn stamp_dilated(&mut self, mask: &BitMask, ox: isize, oy: isize, r: isize) {
        let r2 = r * r;
        let mut offsets = Vec::new();
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r2 {
                    offsets.push((dx, dy));
                }
            }
        }
        for my in 0..mask.h {
            for mx in 0..mask.w {
                if !mask.get(mx, my) {
                    continue;
                }
                let cx = ox + mx as isize;
                let cy = oy + my as isize;
                for &(dx, dy) in &offsets {
                    let nx = cx + dx;
                    let ny = cy + dy;
                    if nx >= 0 && ny >= 0 && (nx as usize) < self.w && (ny as usize) < self.h {
                        self.set(nx as usize, ny as usize);
                    }
                }
            }
        }
    }
}

fn rasterize_bbox_xy(mesh: &TriMesh, tr: Transform3x4, res: f32) -> BitMask {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for p in &mesh.positions {
        let x = tr[0][0] * p[0] + tr[0][1] * p[1] + tr[0][2] * p[2] + tr[0][3];
        let y = tr[1][0] * p[0] + tr[1][1] * p[1] + tr[1][2] * p[2] + tr[1][3];
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    if !min_x.is_finite() || !min_y.is_finite() {
        return BitMask::new(1, 1, [0.0, 0.0]);
    }
    let w = (((max_x - min_x) / res).ceil() as usize).clamp(1, 2000);
    let h = (((max_y - min_y) / res).ceil() as usize).clamp(1, 2000);
    let mut mask = BitMask::new(w, h, [min_x, min_y]);
    for iy in 0..h {
        for ix in 0..w {
            mask.set(ix, iy);
        }
    }
    mask
}

fn rasterize_mesh_xy(mesh: &TriMesh, tr: Transform3x4, res: f32) -> BitMask {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    let verts_xy: Vec<[f32; 2]> = mesh
        .positions
        .iter()
        .map(|p| {
            let x = tr[0][0] * p[0] + tr[0][1] * p[1] + tr[0][2] * p[2] + tr[0][3];
            let y = tr[1][0] * p[0] + tr[1][1] * p[1] + tr[1][2] * p[2] + tr[1][3];
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            [x, y]
        })
        .collect();

    if !min_x.is_finite() || !min_y.is_finite() {
        return BitMask::new(1, 1, [0.0, 0.0]);
    }

    let w = (((max_x - min_x) / res).ceil() as usize).clamp(1, 2000);
    let h = (((max_y - min_y) / res).ceil() as usize).clamp(1, 2000);
    let mut mask = BitMask::new(w, h, [min_x, min_y]);

    for tri in &mesh.indices {
        let a = verts_xy[tri[0] as usize];
        let b = verts_xy[tri[1] as usize];
        let c = verts_xy[tri[2] as usize];

        let t_min_x = a[0].min(b[0]).min(c[0]);
        let t_max_x = a[0].max(b[0]).max(c[0]);
        let t_min_y = a[1].min(b[1]).min(c[1]);
        let t_max_y = a[1].max(b[1]).max(c[1]);

        let ix0 = (((t_min_x - min_x) / res).floor().max(0.0) as usize).min(w - 1);
        let ix1 = (((t_max_x - min_x) / res).ceil().max(0.0) as usize).min(w - 1);
        let iy0 = (((t_min_y - min_y) / res).floor().max(0.0) as usize).min(h - 1);
        let iy1 = (((t_max_y - min_y) / res).ceil().max(0.0) as usize).min(h - 1);

        for iy in iy0..=iy1 {
            let py = min_y + (iy as f32 + 0.5) * res;
            for ix in ix0..=ix1 {
                let px = min_x + (ix as f32 + 0.5) * res;
                if point_in_tri_xy([px, py], a, b, c) || ix0 == ix1 || iy0 == iy1 {
                    mask.set(ix, iy);
                }
            }
        }
    }

    if mask.solid_count == 0 {
        mask.set(0, 0);
    }
    mask
}

fn point_in_tri_xy(p: [f32; 2], a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> bool {
    let e0 = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
    let e1 = (c[0] - b[0]) * (p[1] - b[1]) - (c[1] - b[1]) * (p[0] - b[0]);
    let e2 = (a[0] - c[0]) * (p[1] - c[1]) - (a[1] - c[1]) * (p[0] - c[0]);
    (e0 >= -1e-4 && e1 >= -1e-4 && e2 >= -1e-4) || (e0 <= 1e-4 && e1 <= 1e-4 && e2 <= 1e-4)
}

/// Compute the Z-rotation angle (radians) that aligns `mesh`'s XY projection to its minimum-area OBB.
fn min_area_rect_angle_rad(mesh: &TriMesh, tr: Transform3x4) -> Option<f32> {
    let mut pts: Vec<[f64; 2]> = mesh
        .positions
        .iter()
        .map(|p| {
            [
                f64::from(tr[0][0] * p[0] + tr[0][1] * p[1] + tr[0][2] * p[2]),
                f64::from(tr[1][0] * p[0] + tr[1][1] * p[1] + tr[1][2] * p[2]),
            ]
        })
        .collect();
    if pts.len() < 3 {
        return None;
    }
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then_with(|| a[1].total_cmp(&b[1])));
    pts.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6);
    if pts.len() < 3 {
        return None;
    }

    let cross = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| -> f64 {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };
    let mut hull: Vec<[f64; 2]> = Vec::with_capacity(pts.len() * 2);
    for &p in &pts {
        while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    let lower_len = hull.len();
    for &p in pts.iter().rev() {
        while hull.len() > lower_len && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    hull.pop();
    if hull.len() < 3 {
        return None;
    }

    let mut best_area = f64::INFINITY;
    let mut best_angle = 0.0_f64;
    for i in 0..hull.len() {
        let p0 = hull[i];
        let p1 = hull[(i + 1) % hull.len()];
        let dx = p1[0] - p0[0];
        let dy = p1[1] - p0[1];
        if dx * dx + dy * dy < 1e-12 {
            continue;
        }
        let theta = -dy.atan2(dx);
        let (s, c) = theta.sin_cos();
        let (mut min_u, mut max_u) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut min_v, mut max_v) = (f64::INFINITY, f64::NEG_INFINITY);
        for &h in &hull {
            let u = c * h[0] - s * h[1];
            let v = s * h[0] + c * h[1];
            min_u = min_u.min(u);
            max_u = max_u.max(u);
            min_v = min_v.min(v);
            max_v = max_v.max(v);
        }
        let area = (max_u - min_u) * (max_v - min_v);
        if area < best_area {
            best_area = area;
            best_angle = theta;
        }
    }
    Some(best_angle as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MachineProfile, TriMesh};
    use std::sync::Arc;

    fn box_mesh(wx: f32, wy: f32, wz: f32) -> Arc<TriMesh> {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [wx, 0.0, 0.0],
            [wx, wy, 0.0],
            [0.0, wy, 0.0],
            [0.0, 0.0, wz],
            [wx, 0.0, wz],
            [wx, wy, wz],
            [0.0, wy, wz],
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
        Arc::new(TriMesh::from_indexed(positions, indices).expect("box"))
    }

    #[test]
    fn nest_2d_packs_multiple_parts_with_clearance_and_overflow() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("Box50x40", box_mesh(50.0, 40.0, 15.0));
        for k in 0..4 {
            job.add_instance(
                pid,
                [[1.0, 0.0, 0.0, 10.0 + k as f32], [0.0, 1.0, 0.0, 10.0], [0.0, 0.0, 1.0, 5.0]],
            )
            .expect("inst");
        }
        let cfg = Nest2dConfig {
            min_distance_mm: 5.0,
            resolution_mm: 2.0,
            ..Nest2dConfig::default()
        };
        let res = nest_2d(&mut job, &cfg, None).expect("nest2d");
        assert_eq!(res.placed_count, 4);
        assert_eq!(res.overflow_count, 0);

        // Verify all pairs respect min_distance_mm in XY and sit on min_z_mm.
        let proto = job.part(pid).expect("proto").clone();
        for (i, a) in job.instances.iter().enumerate() {
            let wa = a.world_bounds(proto.mesh.bounds);
            assert!((wa[0][2] - job.min_z_mm).abs() < 1e-3);
            for b in &job.instances[i + 1..] {
                let wb = b.world_bounds(proto.mesh.bounds);
                let dx = (wa[0][0] - wb[1][0]).max(wb[0][0] - wa[1][0]);
                let dy = (wa[0][1] - wb[1][1]).max(wb[0][1] - wa[1][1]);
                assert!(dx >= 4.0 || dy >= 4.0, "overlap: {wa:?} vs {wb:?}");
            }
        }
    }

    #[test]
    fn nest_2d_in_place_outside_platform_keeps_outside_origin() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("Box30x30", box_mesh(30.0, 30.0, 10.0));
        job.add_instance(pid, [[1.0, 0.0, 0.0, 300.0], [0.0, 1.0, 0.0, 20.0], [0.0, 0.0, 1.0, 0.0]])
            .expect("1");
        job.add_instance(pid, [[1.0, 0.0, 0.0, 305.0], [0.0, 1.0, 0.0, 25.0], [0.0, 0.0, 1.0, 0.0]])
            .expect("2");

        let cfg = Nest2dConfig {
            target: Nest2dTarget::InPlaceOutsidePlatform,
            min_distance_mm: 5.0,
            resolution_mm: 2.0,
            ..Nest2dConfig::default()
        };
        let res = nest_2d(&mut job, &cfg, None).expect("nest2d");
        assert_eq!(res.placed_count, 2);
        let proto = job.part(pid).expect("proto").clone();
        for inst in &job.instances {
            let wb = inst.world_bounds(proto.mesh.bounds);
            assert!(wb[0][0] >= 299.0, "should stay outside platform at X>=300: {wb:?}");
        }
    }
}
