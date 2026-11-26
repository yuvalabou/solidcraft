//! Fast exact 3D mesh-to-mesh collision detection & overlap voxel extraction.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::model::{
    Aabb, BuildJob, GroupId, InstanceId, PartId, Transform3x4, TriMesh, aabbs_overlap,
    point_inside_mesh_x_ray, point_tri_dist_sq, transform_point, tri_aabb, tri_tri_closest_points,
    tri_tri_intersect_segment,
};

const MAX_VOXELS: usize = 1500;
const MAX_QUADS: usize = 3000;
const MAX_PAIR_QUADS: usize = 1500;
const MAX_SEGMENTS: usize = 2000;
const MAX_PAIR_SEGMENTS: usize = 500;

/// A colliding or clearance-violating pair of placed instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollisionPair {
    pub a: InstanceId,
    pub b: InstanceId,
    pub is_hard_intersection: bool,
}

/// One exposed outer boundary face of an overlap or clearance-violation volume.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollisionQuad {
    pub verts: [[f32; 3]; 4],
    pub normal: [f32; 3],
    pub is_hard: bool,
}

/// Exact closest surface points for a clearance violation or grazing contact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollisionContact {
    pub point_a: [f32; 3],
    pub point_b: [f32; 3],
    pub is_hard: bool,
}

/// Collision detection summary including exact overlap voxel centers, boundary quads,
/// intersection seam segments, and closest surface contact markers.
#[derive(Debug, Clone, PartialEq)]
pub struct CollisionReport {
    pub clearance_mm: f32,
    pub voxel_pitch_mm: f32,
    pub pairs: Vec<CollisionPair>,
    pub colliding_ids: HashSet<InstanceId>,
    pub overlap_voxels: Vec<[f32; 3]>,
    pub overlap_quads: Vec<CollisionQuad>,
    pub intersection_segments: Vec<([f32; 3], [f32; 3])>,
    pub contacts: Vec<CollisionContact>,
}

/// Classification of scene changes relative to a cached [`CollisionCache`] state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollisionDiff {
    NoChange,
    RemovalsOnly {
        removed_ids: HashSet<InstanceId>,
    },
    Incremental {
        dirty_ids: HashSet<InstanceId>,
        removed_ids: HashSet<InstanceId>,
    },
    FullRebuild,
}

#[derive(Debug, Clone)]
struct InstanceSnapshot {
    part_id: PartId,
    mesh: Arc<TriMesh>,
    transform_bits: [[u32; 4]; 3],
}

#[derive(Debug, Clone)]
struct PairCollision {
    is_hard_intersection: bool,
    voxels: Vec<[f32; 3]>,
    quads: Vec<CollisionQuad>,
    segments: Vec<([f32; 3], [f32; 3])>,
    contact: Option<CollisionContact>,
}

fn canonical_pair(a: InstanceId, b: InstanceId) -> (InstanceId, InstanceId) {
    if a.0 <= b.0 { (a, b) } else { (b, a) }
}

fn transform_bits(tf: &Transform3x4) -> [[u32; 4]; 3] {
    let norm = |v: f32| if v == 0.0 { 0.0_f32.to_bits() } else { v.to_bits() };
    [
        [norm(tf[0][0]), norm(tf[0][1]), norm(tf[0][2]), norm(tf[0][3])],
        [norm(tf[1][0]), norm(tf[1][1]), norm(tf[1][2]), norm(tf[1][3])],
        [norm(tf[2][0]), norm(tf[2][1]), norm(tf[2][2]), norm(tf[2][3])],
    ]
}

/// Incremental per-pair collision cache so single-part edits avoid re-testing the whole bed.
#[derive(Debug, Clone, Default)]
pub struct CollisionCache {
    initialized: bool,
    clearance_mm: f32,
    voxel_pitch_mm: f32,
    snapshots: HashMap<InstanceId, InstanceSnapshot>,
    pair_results: HashMap<(InstanceId, InstanceId), PairCollision>,
    last_tested_pairs: usize,
}

impl CollisionCache {
    /// Construct an empty, uninitialized collision cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all cached snapshots and pair results.
    pub fn clear(&mut self) {
        self.initialized = false;
        self.last_tested_pairs = 0;
        self.snapshots.clear();
        self.pair_results.clear();
    }

    /// Return `true` after at least one full or synced pass has populated the cache.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Number of candidate instance pairs tested during the most recent `sync` call.
    pub fn last_tested_pairs(&self) -> usize {
        self.last_tested_pairs
    }

    /// Classify what changed in `job` since the cached collision state.
    pub fn diff(&self, job: &BuildJob, voxel_pitch_mm: f32) -> CollisionDiff {
        let clearance = job.clearance_mm.max(0.0);
        let pitch = voxel_pitch_mm.clamp(1.0, 10.0);
        if !self.initialized
            || self.clearance_mm.to_bits() != clearance.to_bits()
            || self.voxel_pitch_mm.to_bits() != pitch.to_bits()
        {
            return CollisionDiff::FullRebuild;
        }

        let part_map: HashMap<PartId, &Arc<TriMesh>> = job.parts.iter().map(|p| (p.id, &p.mesh)).collect();
        let hidden_groups: HashSet<GroupId> = job.groups.iter().filter(|g| g.hidden).map(|g| g.id).collect();

        let mut seen = HashSet::with_capacity(job.instances.len());
        let mut dirty_ids = HashSet::new();
        for inst in &job.instances {
            if inst.hidden || inst.group_id.is_some_and(|g| hidden_groups.contains(&g)) {
                continue;
            }
            let Some(&mesh) = part_map.get(&inst.part_id) else {
                continue;
            };
            seen.insert(inst.id);
            let tf_bits = transform_bits(&inst.transform);
            let changed = match self.snapshots.get(&inst.id) {
                None => true,
                Some(prev) => {
                    prev.part_id != inst.part_id
                        || !Arc::ptr_eq(&prev.mesh, mesh)
                        || prev.transform_bits != tf_bits
                }
            };
            if changed {
                dirty_ids.insert(inst.id);
            }
        }

        let removed_ids: HashSet<InstanceId> = self
            .snapshots
            .keys()
            .copied()
            .filter(|id| !seen.contains(id))
            .collect();

        if dirty_ids.is_empty() && removed_ids.is_empty() {
            CollisionDiff::NoChange
        } else if dirty_ids.is_empty() {
            CollisionDiff::RemovalsOnly { removed_ids }
        } else {
            CollisionDiff::Incremental {
                dirty_ids,
                removed_ids,
            }
        }
    }

    /// Count how many AABB-overlapping pairs involve at least one instance in `dirty_ids`.
    pub fn count_candidate_pairs(&self, job: &BuildJob, dirty_ids: &HashSet<InstanceId>) -> usize {
        if dirty_ids.is_empty() {
            return 0;
        }
        let entries = build_entries(job, job.clearance_mm.max(0.0));
        let is_active: Vec<bool> = entries.iter().map(|e| dirty_ids.contains(&e.id)).collect();
        let active_indices: Vec<usize> = is_active
            .iter()
            .enumerate()
            .filter_map(|(idx, &active)| active.then_some(idx))
            .collect();

        let mut count = 0usize;
        for &i in &active_indices {
            for j in 0..entries.len() {
                if j == i || (is_active[j] && j < i) {
                    continue;
                }
                if aabbs_overlap(entries[i].wb_pad, entries[j].wb_pad) {
                    count += 1;
                }
            }
        }
        count
    }

    /// Assemble a deterministic [`CollisionReport`] from the cached pair results.
    pub fn report(&self) -> CollisionReport {
        assemble_report(self.clearance_mm, self.voxel_pitch_mm, &self.pair_results)
    }

    /// Incrementally synchronize the cache with `job` and return the updated report.
    pub fn sync(&mut self, job: &BuildJob, voxel_pitch_mm: f32) -> CollisionReport {
        self.sync_with_progress(job, voxel_pitch_mm, None)
    }

    /// Incrementally synchronize the cache with `job`, reporting `(done, total)` progress.
    pub fn sync_with_progress(
        &mut self,
        job: &BuildJob,
        voxel_pitch_mm: f32,
        progress: Option<&dyn Fn(usize, usize)>,
    ) -> CollisionReport {
        match self.diff(job, voxel_pitch_mm) {
            CollisionDiff::NoChange => {
                self.last_tested_pairs = 0;
                if let Some(cb) = progress {
                    cb(1, 1);
                }
                self.report()
            }
            CollisionDiff::RemovalsOnly { removed_ids } => {
                for id in &removed_ids {
                    self.snapshots.remove(id);
                }
                self.pair_results
                    .retain(|(a, b), _| !removed_ids.contains(a) && !removed_ids.contains(b));
                self.last_tested_pairs = 0;
                if let Some(cb) = progress {
                    cb(1, 1);
                }
                self.report()
            }
            CollisionDiff::Incremental {
                dirty_ids,
                removed_ids,
            } => {
                for id in &removed_ids {
                    self.snapshots.remove(id);
                }
                self.pair_results.retain(|(a, b), _| {
                    !removed_ids.contains(a)
                        && !removed_ids.contains(b)
                        && !dirty_ids.contains(a)
                        && !dirty_ids.contains(b)
                });
                let entries = build_entries(job, self.clearance_mm);
                for entry in &entries {
                    if dirty_ids.contains(&entry.id) {
                        self.snapshots.insert(
                            entry.id,
                            InstanceSnapshot {
                                part_id: entry.part_id,
                                mesh: Arc::clone(entry.mesh_arc),
                                transform_bits: transform_bits(&entry.tf),
                            },
                        );
                    }
                }
                self.last_tested_pairs = test_pairs_into(
                    &entries,
                    Some(&dirty_ids),
                    self.clearance_mm,
                    self.voxel_pitch_mm,
                    progress,
                    &mut self.pair_results,
                );
                self.report()
            }
            CollisionDiff::FullRebuild => {
                let clearance = job.clearance_mm.max(0.0);
                let pitch = voxel_pitch_mm.clamp(1.0, 10.0);
                self.snapshots.clear();
                self.pair_results.clear();
                self.initialized = true;
                self.clearance_mm = clearance;
                self.voxel_pitch_mm = pitch;

                let entries = build_entries(job, clearance);
                for entry in &entries {
                    self.snapshots.insert(
                        entry.id,
                        InstanceSnapshot {
                            part_id: entry.part_id,
                            mesh: Arc::clone(entry.mesh_arc),
                            transform_bits: transform_bits(&entry.tf),
                        },
                    );
                }
                self.last_tested_pairs =
                    test_pairs_into(&entries, None, clearance, pitch, progress, &mut self.pair_results);
                self.report()
            }
        }
    }
}

/// Detect part-to-part collisions / clearance violations and extract 3D overlap voxels.
///
/// When `active_ids` is `Some(moving)`, only pairs involving at least one instance in `moving`
/// are tested (for fast live drag highlighting).
pub fn detect_collisions(
    job: &BuildJob,
    active_ids: Option<&HashSet<InstanceId>>,
    voxel_pitch_mm: f32,
) -> CollisionReport {
    detect_collisions_with_progress(job, active_ids, voxel_pitch_mm, None)
}

/// Detect part-to-part collisions with optional `(done, total)` progress callback.
pub fn detect_collisions_with_progress(
    job: &BuildJob,
    active_ids: Option<&HashSet<InstanceId>>,
    voxel_pitch_mm: f32,
    progress: Option<&dyn Fn(usize, usize)>,
) -> CollisionReport {
    let clearance = job.clearance_mm.max(0.0);
    let pitch = voxel_pitch_mm.clamp(1.0, 10.0);
    let entries = build_entries(job, clearance);
    let mut pair_results = HashMap::new();
    test_pairs_into(&entries, active_ids, clearance, pitch, progress, &mut pair_results);
    assemble_report(clearance, pitch, &pair_results)
}

struct Entry<'a> {
    id: InstanceId,
    part_id: PartId,
    mesh: &'a TriMesh,
    mesh_arc: &'a Arc<TriMesh>,
    tf: Transform3x4,
    wb: Aabb,
    wb_pad: Aabb,
}

fn build_entries<'a>(job: &'a BuildJob, clearance: f32) -> Vec<Entry<'a>> {
    let part_map: HashMap<PartId, &'a Arc<TriMesh>> = job.parts.iter().map(|p| (p.id, &p.mesh)).collect();
    let hidden_groups: HashSet<GroupId> = job.groups.iter().filter(|g| g.hidden).map(|g| g.id).collect();
    let pad = clearance * 0.5;

    let mut entries = Vec::with_capacity(job.instances.len());
    for inst in &job.instances {
        if inst.hidden || inst.group_id.is_some_and(|g| hidden_groups.contains(&g)) {
            continue;
        }
        let Some(&mesh_arc) = part_map.get(&inst.part_id) else {
            continue;
        };
        let wb = inst.world_bounds(mesh_arc.bounds);
        let wb_pad = [
            [wb[0][0] - pad, wb[0][1] - pad, wb[0][2] - pad],
            [wb[1][0] + pad, wb[1][1] + pad, wb[1][2] + pad],
        ];
        entries.push(Entry {
            id: inst.id,
            part_id: inst.part_id,
            mesh: mesh_arc.as_ref(),
            mesh_arc,
            tf: inst.transform,
            wb,
            wb_pad,
        });
    }
    entries
}

fn test_pairs_into(
    entries: &[Entry<'_>],
    active_ids: Option<&HashSet<InstanceId>>,
    clearance: f32,
    pitch: f32,
    progress: Option<&dyn Fn(usize, usize)>,
    out_pairs: &mut HashMap<(InstanceId, InstanceId), PairCollision>,
) -> usize {
    let clearance_sq = clearance * clearance;
    let mut tested_pairs = 0usize;

    if let Some(moving) = active_ids {
        let is_active: Vec<bool> = entries.iter().map(|e| moving.contains(&e.id)).collect();
        let active_indices: Vec<usize> = is_active
            .iter()
            .enumerate()
            .filter_map(|(idx, &active)| active.then_some(idx))
            .collect();
        let total_steps = active_indices.len().max(1);
        if let Some(cb) = progress {
            cb(0, total_steps);
        }
        for (step, &i) in active_indices.iter().enumerate() {
            if let Some(cb) = progress {
                cb(step + 1, total_steps);
            }
            for j in 0..entries.len() {
                if j == i || (is_active[j] && j < i) {
                    continue;
                }
                tested_pairs += 1;
                if let Some(col) = test_single_pair(&entries[i], &entries[j], clearance, clearance_sq, pitch, MAX_VOXELS)
                {
                    out_pairs.insert(canonical_pair(entries[i].id, entries[j].id), col);
                }
            }
        }
    } else {
        let total_steps = entries.len().max(1);
        if let Some(cb) = progress {
            cb(0, total_steps);
        }
        for i in 0..entries.len() {
            if let Some(cb) = progress {
                cb(i + 1, total_steps);
            }
            for j in (i + 1)..entries.len() {
                tested_pairs += 1;
                if let Some(col) = test_single_pair(&entries[i], &entries[j], clearance, clearance_sq, pitch, MAX_VOXELS)
                {
                    out_pairs.insert(canonical_pair(entries[i].id, entries[j].id), col);
                }
            }
        }
    }

    tested_pairs
}

fn test_single_pair(
    a: &Entry<'_>,
    b: &Entry<'_>,
    clearance: f32,
    clearance_sq: f32,
    pitch: f32,
    voxel_budget: usize,
) -> Option<PairCollision> {
    if !aabbs_overlap(a.wb_pad, b.wb_pad) {
        return None;
    }

    let ov = [
        [
            a.wb_pad[0][0].max(b.wb_pad[0][0]),
            a.wb_pad[0][1].max(b.wb_pad[0][1]),
            a.wb_pad[0][2].max(b.wb_pad[0][2]),
        ],
        [
            a.wb_pad[1][0].min(b.wb_pad[1][0]),
            a.wb_pad[1][1].min(b.wb_pad[1][1]),
            a.wb_pad[1][2].min(b.wb_pad[1][2]),
        ],
    ];

    // Fast scanline voxelization inside `ov` (O(tris_in_yz_slab + grid_cells)).
    let vox_res = fast_overlap_voxels(
        ov,
        pitch,
        clearance * 0.5,
        a.mesh,
        &a.tf,
        b.mesh,
        &b.tf,
        voxel_budget,
    );

    if !vox_res.voxels.is_empty() {
        let segments = if vox_res.has_hard {
            let seam_box = if let Some(ob) = vox_res.occupied_bounds {
                let pad = vox_res.step;
                [
                    [
                        (ob[0][0] - pad).max(ov[0][0]),
                        (ob[0][1] - pad).max(ov[0][1]),
                        (ob[0][2] - pad).max(ov[0][2]),
                    ],
                    [
                        (ob[1][0] + pad).min(ov[1][0]),
                        (ob[1][1] + pad).min(ov[1][1]),
                        (ob[1][2] + pad).min(ov[1][2]),
                    ],
                ]
            } else {
                ov
            };
            let tris_a = collect_tris_in_box(a.mesh, &a.tf, seam_box);
            let tris_b = collect_tris_in_box(b.mesh, &b.tf, seam_box);
            collect_intersection_segments(&tris_a, &tris_b, seam_box, MAX_PAIR_SEGMENTS)
        } else {
            Vec::new()
        };
        return Some(PairCollision {
            is_hard_intersection: vox_res.has_hard,
            voxels: vox_res.voxels,
            quads: vox_res.quads,
            segments,
            contact: vox_res.contact,
        });
    }

    // Fallback: exact spatial-grid triangle-triangle distance for thin/grazing clearance violations.
    let ov_pad = [
        [ov[0][0] - clearance, ov[0][1] - clearance, ov[0][2] - clearance],
        [ov[1][0] + clearance, ov[1][1] + clearance, ov[1][2] + clearance],
    ];
    let tris_a = collect_tris_in_box(a.mesh, &a.tf, ov_pad);
    let tris_b = collect_tris_in_box(b.mesh, &b.tf, ov_pad);

    let (min_dist_sq, closest_pair) = spatial_min_tri_closest_points(&tris_a, &tris_b, ov_pad, clearance);
    let mut contained = false;
    if min_dist_sq > clearance_sq {
        if let Some(p0) = a.mesh.positions.first() {
            let wp = transform_point(&a.tf, *p0);
            if aabbs_overlap([wp, wp], b.wb) && point_inside_mesh_x_ray(wp, b.mesh, &b.tf) {
                contained = true;
            }
        }
        if !contained && let Some(p0) = b.mesh.positions.first() {
            let wp = transform_point(&b.tf, *p0);
            if aabbs_overlap([wp, wp], a.wb) && point_inside_mesh_x_ray(wp, a.mesh, &a.tf) {
                contained = true;
            }
        }
    }

    if min_dist_sq <= clearance_sq + 1e-5 || contained {
        let is_hard = min_dist_sq <= 1e-4 || contained;
        let segments = if min_dist_sq <= 1e-4 {
            collect_intersection_segments(&tris_a, &tris_b, ov_pad, MAX_PAIR_SEGMENTS)
        } else {
            Vec::new()
        };
        let contact = closest_pair.map(|(point_a, point_b)| CollisionContact {
            point_a,
            point_b,
            is_hard,
        });
        let mut voxels = Vec::new();
        if voxel_budget > 0 {
            let pt = if let Some((pa, pb)) = closest_pair {
                [(pa[0] + pb[0]) * 0.5, (pa[1] + pb[1]) * 0.5, (pa[2] + pb[2]) * 0.5]
            } else {
                [
                    (ov[0][0] + ov[1][0]) * 0.5,
                    (ov[0][1] + ov[1][1]) * 0.5,
                    (ov[0][2] + ov[1][2]) * 0.5,
                ]
            };
            voxels.push(pt);
        }
        return Some(PairCollision {
            is_hard_intersection: is_hard,
            voxels,
            quads: Vec::new(),
            segments,
            contact,
        });
    }

    None
}

fn append_capped<T: Copy>(slices: &[&[T]], limit: usize, out: &mut Vec<T>) {
    let total: usize = slices.iter().map(|s| s.len()).sum();
    if total == 0 || limit == 0 {
        return;
    }
    if total <= limit {
        out.reserve(total);
        for s in slices {
            out.extend_from_slice(s);
        }
        return;
    }
    out.reserve(limit);
    let non_empty = slices.iter().filter(|s| !s.is_empty()).count().max(1);
    let per_pair = (limit / non_empty).max(1);
    for s in slices {
        if out.len() >= limit {
            break;
        }
        let remaining = limit - out.len();
        let take = per_pair.min(remaining).min(s.len());
        if take == s.len() {
            out.extend_from_slice(s);
        } else if let Some(q) = s.len().checked_div(take) {
            let stride = q.max(1);
            for item in s.iter().step_by(stride).take(take) {
                out.push(*item);
            }
        }
    }
}

fn assemble_report(
    clearance_mm: f32,
    voxel_pitch_mm: f32,
    pair_results: &HashMap<(InstanceId, InstanceId), PairCollision>,
) -> CollisionReport {
    let mut sorted: Vec<((InstanceId, InstanceId), &PairCollision)> =
        pair_results.iter().map(|(&k, v)| (k, v)).collect();
    sorted.sort_unstable_by_key(|((a, b), _)| (a.0, b.0));

    let mut pairs = Vec::with_capacity(sorted.len());
    let mut colliding_ids = HashSet::with_capacity(sorted.len() * 2);
    let mut contacts = Vec::new();
    let mut voxel_slices = Vec::with_capacity(sorted.len());
    let mut quad_slices = Vec::with_capacity(sorted.len());
    let mut segment_slices = Vec::with_capacity(sorted.len());

    for &((a, b), col) in &sorted {
        pairs.push(CollisionPair {
            a,
            b,
            is_hard_intersection: col.is_hard_intersection,
        });
        colliding_ids.insert(a);
        colliding_ids.insert(b);
        if let Some(c) = col.contact {
            contacts.push(c);
        }
        voxel_slices.push(col.voxels.as_slice());
        quad_slices.push(col.quads.as_slice());
        segment_slices.push(col.segments.as_slice());
    }

    let mut overlap_voxels = Vec::new();
    append_capped(&voxel_slices, MAX_VOXELS, &mut overlap_voxels);
    let mut overlap_quads = Vec::new();
    append_capped(&quad_slices, MAX_QUADS, &mut overlap_quads);
    let mut intersection_segments = Vec::new();
    append_capped(&segment_slices, MAX_SEGMENTS, &mut intersection_segments);

    CollisionReport {
        clearance_mm,
        voxel_pitch_mm,
        pairs,
        colliding_ids,
        overlap_voxels,
        overlap_quads,
        intersection_segments,
        contacts,
    }
}

fn collect_tris_in_box(mesh: &TriMesh, tf: &Transform3x4, box_aabb: Aabb) -> Vec<([[f32; 3]; 3], Aabb)> {
    let mut out = Vec::new();
    for tri in &mesh.indices {
        let a = transform_point(tf, mesh.positions[tri[0] as usize]);
        let b = transform_point(tf, mesh.positions[tri[1] as usize]);
        let c = transform_point(tf, mesh.positions[tri[2] as usize]);
        let tb = tri_aabb(a, b, c);
        if aabbs_overlap(tb, box_aabb) {
            out.push(([a, b, c], tb));
        }
    }
    out
}

struct TriGrid {
    bounds: Aabb,
    inv_size: [f32; 3],
    cells: Vec<Vec<usize>>,
}

impl TriGrid {
    const G: usize = 8;

    fn build(tris_b: &[([[f32; 3]; 3], Aabb)], bounds: Aabb) -> Self {
        let inv_size = [
            Self::G as f32 / (bounds[1][0] - bounds[0][0]).max(1e-3),
            Self::G as f32 / (bounds[1][1] - bounds[0][1]).max(1e-3),
            Self::G as f32 / (bounds[1][2] - bounds[0][2]).max(1e-3),
        ];
        let mut grid = Self {
            bounds,
            inv_size,
            cells: vec![Vec::new(); Self::G * Self::G * Self::G],
        };
        for (idx, &(_, bb)) in tris_b.iter().enumerate() {
            let (lo, hi) = grid.cell_range(bb, 0.0);
            for z in lo[2]..=hi[2] {
                for y in lo[1]..=hi[1] {
                    for x in lo[0]..=hi[0] {
                        grid.cells[(z * Self::G + y) * Self::G + x].push(idx);
                    }
                }
            }
        }
        grid
    }

    fn cell_range(&self, b: Aabb, pad: f32) -> ([usize; 3], [usize; 3]) {
        let max_c = Self::G as isize - 1;
        let lo = [
            (((b[0][0] - pad - self.bounds[0][0]) * self.inv_size[0]).floor() as isize).clamp(0, max_c) as usize,
            (((b[0][1] - pad - self.bounds[0][1]) * self.inv_size[1]).floor() as isize).clamp(0, max_c) as usize,
            (((b[0][2] - pad - self.bounds[0][2]) * self.inv_size[2]).floor() as isize).clamp(0, max_c) as usize,
        ];
        let hi = [
            (((b[1][0] + pad - self.bounds[0][0]) * self.inv_size[0]).floor() as isize).clamp(0, max_c) as usize,
            (((b[1][1] + pad - self.bounds[0][1]) * self.inv_size[1]).floor() as isize).clamp(0, max_c) as usize,
            (((b[1][2] + pad - self.bounds[0][2]) * self.inv_size[2]).floor() as isize).clamp(0, max_c) as usize,
        ];
        (lo, hi)
    }
}

/// 8×8×8 uniform spatial grid accelerated closest surface points between two triangle sets.
fn spatial_min_tri_closest_points(
    tris_a: &[([[f32; 3]; 3], Aabb)],
    tris_b: &[([[f32; 3]; 3], Aabb)],
    bounds: Aabb,
    clearance: f32,
) -> (f32, Option<([f32; 3], [f32; 3])>) {
    if tris_a.is_empty() || tris_b.is_empty() {
        return (f32::INFINITY, None);
    }
    let grid = TriGrid::build(tris_b, bounds);
    let mut seen_b = vec![0_u32; tris_b.len()];
    let mut stamp = 0_u32;
    let mut min_d2 = f32::INFINITY;
    let mut best_pair = None;

    for &(ta, ba) in tris_a {
        stamp = stamp.wrapping_add(1).max(1);
        let ba_pad = [
            [ba[0][0] - clearance, ba[0][1] - clearance, ba[0][2] - clearance],
            [ba[1][0] + clearance, ba[1][1] + clearance, ba[1][2] + clearance],
        ];
        let (lo, hi) = grid.cell_range(ba, clearance);
        for z in lo[2]..=hi[2] {
            for y in lo[1]..=hi[1] {
                for x in lo[0]..=hi[0] {
                    for &b_idx in &grid.cells[(z * TriGrid::G + y) * TriGrid::G + x] {
                        if seen_b[b_idx] == stamp {
                            continue;
                        }
                        seen_b[b_idx] = stamp;
                        let (tb, bb) = tris_b[b_idx];
                        if !aabbs_overlap(ba_pad, bb) {
                            continue;
                        }
                        let (pa, pb, d2) = tri_tri_closest_points(ta, tb);
                        if d2 < min_d2 {
                            min_d2 = d2;
                            best_pair = Some((pa, pb));
                            if min_d2 <= 1e-6 {
                                return (0.0, best_pair);
                            }
                        }
                    }
                }
            }
        }
    }
    (min_d2, best_pair)
}

fn collect_intersection_segments(
    tris_a: &[([[f32; 3]; 3], Aabb)],
    tris_b: &[([[f32; 3]; 3], Aabb)],
    bounds: Aabb,
    max_segments: usize,
) -> Vec<([f32; 3], [f32; 3])> {
    if tris_a.is_empty() || tris_b.is_empty() || max_segments == 0 {
        return Vec::new();
    }
    let grid = TriGrid::build(tris_b, bounds);
    let mut seen_b = vec![0_u32; tris_b.len()];
    let mut stamp = 0_u32;
    let mut segments = Vec::new();

    for &(ta, ba) in tris_a {
        if segments.len() >= max_segments {
            break;
        }
        stamp = stamp.wrapping_add(1).max(1);
        let (lo, hi) = grid.cell_range(ba, 1e-4);
        for z in lo[2]..=hi[2] {
            for y in lo[1]..=hi[1] {
                for x in lo[0]..=hi[0] {
                    for &b_idx in &grid.cells[(z * TriGrid::G + y) * TriGrid::G + x] {
                        if seen_b[b_idx] == stamp {
                            continue;
                        }
                        seen_b[b_idx] = stamp;
                        let (tb, bb) = tris_b[b_idx];
                        if !aabbs_overlap(ba, bb) {
                            continue;
                        }
                        if let Some(seg) = tri_tri_intersect_segment(ta, tb) {
                            segments.push(seg);
                            if segments.len() >= max_segments {
                                return segments;
                            }
                        }
                    }
                }
            }
        }
    }
    segments
}

struct VoxelOverlapResult {
    voxels: Vec<[f32; 3]>,
    quads: Vec<CollisionQuad>,
    has_hard: bool,
    step: f32,
    occupied_bounds: Option<Aabb>,
    contact: Option<CollisionContact>,
}

fn expand_bounds_point(bounds: &mut Option<Aabb>, lo: [f32; 3], hi: [f32; 3]) {
    match bounds {
        None => *bounds = Some([lo, hi]),
        Some(b) => {
            for k in 0..3 {
                b[0][k] = b[0][k].min(lo[k]);
                b[1][k] = b[1][k].max(hi[k]);
            }
        }
    }
}

fn extract_exposed_quads(
    ov: Aabb,
    step: f32,
    dims: [usize; 3],
    occupied: &[bool],
    is_hard: bool,
    max_quads: usize,
) -> Vec<CollisionQuad> {
    let [nx, ny, nz] = dims;
    let mut quads = Vec::new();
    let is_occ = |ix: isize, iy: isize, iz: isize| -> bool {
        if ix < 0 || iy < 0 || iz < 0 {
            return false;
        }
        let (ux, uy, uz) = (ix as usize, iy as usize, iz as usize);
        ux < nx && uy < ny && uz < nz && occupied[(uz * ny + uy) * nx + ux]
    };

    for iz in 0..nz {
        let z0 = ov[0][2] + iz as f32 * step;
        let z1 = z0 + step;
        for iy in 0..ny {
            let y0 = ov[0][1] + iy as f32 * step;
            let y1 = y0 + step;
            for ix in 0..nx {
                let idx = (iz * ny + iy) * nx + ix;
                if !occupied[idx] {
                    continue;
                }
                let x0 = ov[0][0] + ix as f32 * step;
                let x1 = x0 + step;
                let (six, siy, siz) = (ix as isize, iy as isize, iz as isize);

                let faces: [([isize; 3], [f32; 3], [[f32; 3]; 4]); 6] = [
                    ([-1, 0, 0], [-1.0, 0.0, 0.0], [[x0, y0, z0], [x0, y0, z1], [x0, y1, z1], [x0, y1, z0]]),
                    ([1, 0, 0], [1.0, 0.0, 0.0], [[x1, y0, z0], [x1, y1, z0], [x1, y1, z1], [x1, y0, z1]]),
                    ([0, -1, 0], [0.0, -1.0, 0.0], [[x0, y0, z0], [x1, y0, z0], [x1, y0, z1], [x0, y0, z1]]),
                    ([0, 1, 0], [0.0, 1.0, 0.0], [[x0, y1, z0], [x0, y1, z1], [x1, y1, z1], [x1, y1, z0]]),
                    ([0, 0, -1], [0.0, 0.0, -1.0], [[x0, y0, z0], [x0, y1, z0], [x1, y1, z0], [x1, y0, z0]]),
                    ([0, 0, 1], [0.0, 0.0, 1.0], [[x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1]]),
                ];
                for (d, normal, verts) in faces {
                    if !is_occ(six + d[0], siy + d[1], siz + d[2]) {
                        quads.push(CollisionQuad { verts, normal, is_hard });
                        if quads.len() >= max_quads {
                            return quads;
                        }
                    }
                }
            }
        }
    }
    quads
}

/// Fast scanline voxelization of `mesh_a` and `mesh_b` inside `ov`.
#[allow(clippy::too_many_arguments)]
fn fast_overlap_voxels(
    ov: Aabb,
    pitch: f32,
    half_clear: f32,
    mesh_a: &TriMesh,
    tf_a: &Transform3x4,
    mesh_b: &TriMesh,
    tf_b: &Transform3x4,
    budget: usize,
) -> VoxelOverlapResult {
    let max_extent = (ov[1][0] - ov[0][0])
        .max(ov[1][1] - ov[0][1])
        .max(ov[1][2] - ov[0][2])
        .max(0.5);
    let base_step = (max_extent / 16.0).clamp(0.5, pitch);

    if budget == 0 {
        return VoxelOverlapResult {
            voxels: Vec::new(),
            quads: Vec::new(),
            has_hard: false,
            step: base_step,
            occupied_bounds: None,
            contact: None,
        };
    }

    let sx = ((ov[1][0] - ov[0][0]) / base_step).ceil().max(1.0) as usize;
    let sy = ((ov[1][1] - ov[0][1]) / base_step).ceil().max(1.0) as usize;
    let sz = ((ov[1][2] - ov[0][2]) / base_step).ceil().max(1.0) as usize;
    let total = sx.saturating_mul(sy).saturating_mul(sz);
    let step_mul = if total > 4096 {
        ((total as f32 / 3000.0).cbrt()).ceil().max(1.0)
    } else {
        1.0
    };
    let step = base_step * step_mul;

    let nx = (((ov[1][0] - ov[0][0]) / step).ceil() as usize).clamp(1, 48);
    let ny = (((ov[1][1] - ov[0][1]) / step).ceil() as usize).clamp(1, 48);
    let nz = (((ov[1][2] - ov[0][2]) / step).ceil() as usize).clamp(1, 48);

    let mut mask_a = vec![false; nx * ny * nz];
    let mut mask_b = vec![false; nx * ny * nz];
    fill_scanline_mask(mesh_a, tf_a, ov, step, [nx, ny, nz], &mut mask_a);
    fill_scanline_mask(mesh_b, tf_b, ov, step, [nx, ny, nz], &mut mask_b);

    let mut voxels = Vec::new();
    let mut occupied = vec![false; nx * ny * nz];
    let mut occupied_bounds = None;
    let mut has_hard = false;
    for iz in 0..nz {
        let z0 = ov[0][2] + iz as f32 * step;
        for iy in 0..ny {
            let y0 = ov[0][1] + iy as f32 * step;
            for ix in 0..nx {
                let idx = (iz * ny + iy) * nx + ix;
                if mask_a[idx] && mask_b[idx] {
                    has_hard = true;
                    occupied[idx] = true;
                    let x0 = ov[0][0] + ix as f32 * step;
                    expand_bounds_point(&mut occupied_bounds, [x0, y0, z0], [x0 + step, y0 + step, z0 + step]);
                    if voxels.len() < budget {
                        voxels.push([x0 + 0.5 * step, y0 + 0.5 * step, z0 + 0.5 * step]);
                    }
                }
            }
        }
    }

    let mut contact = None;

    // If no direct solid overlap, verify exact clearance violation before dilating overlap voxels.
    if voxels.is_empty() && half_clear > 0.0 {
        let clearance = half_clear * 2.0;
        let clearance_sq = clearance * clearance;
        let ov_pad = [
            [ov[0][0] - clearance, ov[0][1] - clearance, ov[0][2] - clearance],
            [ov[1][0] + clearance, ov[1][1] + clearance, ov[1][2] + clearance],
        ];
        let tris_a = collect_tris_in_box(mesh_a, tf_a, ov_pad);
        let tris_b = collect_tris_in_box(mesh_b, tf_b, ov_pad);
        if !tris_a.is_empty() && !tris_b.is_empty() {
            let (min_d2, closest) = spatial_min_tri_closest_points(&tris_a, &tris_b, ov_pad, clearance);
            if min_d2 <= clearance_sq + 1e-5 {
                let tol = half_clear + step * 0.5;
                let tol_sq = tol * tol;
                let tri_voxel_range = |tb: Aabb| -> Option<([usize; 3], [usize; 3])> {
                    let mut lo = [0usize; 3];
                    let mut hi = [0usize; 3];
                    let dims = [nx, ny, nz];
                    for k in 0..3 {
                        let a = (((tb[0][k] - tol - ov[0][k]) / step) - 0.5).floor() as isize;
                        let b = (((tb[1][k] + tol - ov[0][k]) / step) - 0.5).ceil() as isize;
                        let max_idx = dims[k] as isize - 1;
                        if b < 0 || a > max_idx {
                            return None;
                        }
                        lo[k] = a.clamp(0, max_idx) as usize;
                        hi[k] = b.clamp(0, max_idx) as usize;
                    }
                    Some((lo, hi))
                };

                let mut near_a = mask_a;
                for &(t, tb) in &tris_a {
                    let Some((lo, hi)) = tri_voxel_range(tb) else {
                        continue;
                    };
                    for iz in lo[2]..=hi[2] {
                        let z = ov[0][2] + (iz as f32 + 0.5) * step;
                        for iy in lo[1]..=hi[1] {
                            let y = ov[0][1] + (iy as f32 + 0.5) * step;
                            for ix in lo[0]..=hi[0] {
                                let idx = (iz * ny + iy) * nx + ix;
                                if !near_a[idx] {
                                    let p = [ov[0][0] + (ix as f32 + 0.5) * step, y, z];
                                    if point_tri_dist_sq(p, t) <= tol_sq {
                                        near_a[idx] = true;
                                    }
                                }
                            }
                        }
                    }
                }

                for iz in 0..nz {
                    let z0 = ov[0][2] + iz as f32 * step;
                    for iy in 0..ny {
                        let y0 = ov[0][1] + iy as f32 * step;
                        for ix in 0..nx {
                            let idx = (iz * ny + iy) * nx + ix;
                            if near_a[idx] && mask_b[idx] {
                                occupied[idx] = true;
                                let x0 = ov[0][0] + ix as f32 * step;
                                expand_bounds_point(
                                    &mut occupied_bounds,
                                    [x0, y0, z0],
                                    [x0 + step, y0 + step, z0 + step],
                                );
                                if voxels.len() < budget {
                                    voxels.push([x0 + 0.5 * step, y0 + 0.5 * step, z0 + 0.5 * step]);
                                }
                            }
                        }
                    }
                }

                for &(t, tb) in &tris_b {
                    let Some((lo, hi)) = tri_voxel_range(tb) else {
                        continue;
                    };
                    for iz in lo[2]..=hi[2] {
                        let z0 = ov[0][2] + iz as f32 * step;
                        let z = z0 + 0.5 * step;
                        for iy in lo[1]..=hi[1] {
                            let y0 = ov[0][1] + iy as f32 * step;
                            let y = y0 + 0.5 * step;
                            for ix in lo[0]..=hi[0] {
                                let idx = (iz * ny + iy) * nx + ix;
                                if near_a[idx] && !occupied[idx] {
                                    let x0 = ov[0][0] + ix as f32 * step;
                                    let p = [x0 + 0.5 * step, y, z];
                                    if point_tri_dist_sq(p, t) <= tol_sq {
                                        occupied[idx] = true;
                                        expand_bounds_point(
                                            &mut occupied_bounds,
                                            [x0, y0, z0],
                                            [x0 + step, y0 + step, z0 + step],
                                        );
                                        if voxels.len() < budget {
                                            voxels.push(p);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if !voxels.is_empty() {
                    contact = closest.map(|(point_a, point_b)| CollisionContact {
                        point_a,
                        point_b,
                        is_hard: false,
                    });
                }
            }
        }
    }

    let quads = if !voxels.is_empty() {
        extract_exposed_quads(ov, step, [nx, ny, nz], &occupied, has_hard, MAX_PAIR_QUADS)
    } else {
        Vec::new()
    };

    VoxelOverlapResult {
        voxels,
        quads,
        has_hard,
        step,
        occupied_bounds,
        contact,
    }
}

fn fill_scanline_mask(
    mesh: &TriMesh,
    tf: &Transform3x4,
    ov: Aabb,
    step: f32,
    dims: [usize; 3],
    mask: &mut [bool],
) {
    let [nx, ny, nz] = dims;
    let y_lo = ov[0][1];
    let y_hi = ov[0][1] + ny as f32 * step;
    let z_lo = ov[0][2];
    let z_hi = ov[0][2] + nz as f32 * step;

    // Collect only triangles intersecting the YZ slab of `ov`.
    let mut slab_tris: Vec<[[f32; 3]; 3]> = Vec::new();
    for tri in &mesh.indices {
        let a = transform_point(tf, mesh.positions[tri[0] as usize]);
        let b = transform_point(tf, mesh.positions[tri[1] as usize]);
        let c = transform_point(tf, mesh.positions[tri[2] as usize]);
        let t_min_y = a[1].min(b[1]).min(c[1]);
        let t_max_y = a[1].max(b[1]).max(c[1]);
        let t_min_z = a[2].min(b[2]).min(c[2]);
        let t_max_z = a[2].max(b[2]).max(c[2]);
        if t_max_y >= y_lo && t_min_y <= y_hi && t_max_z >= z_lo && t_min_z <= z_hi {
            slab_tris.push([a, b, c]);
        }
    }
    if slab_tris.is_empty() {
        return;
    }

    let mut segs: Vec<([f32; 2], [f32; 2])> = Vec::new();
    let mut xs: Vec<f32> = Vec::new();
    for iz in 0..nz {
        let z = ov[0][2] + (iz as f32 + 0.5) * step;
        segs.clear();
        for &[a, b, c] in &slab_tris {
            let edges = [(a, b), (b, c), (c, a)];
            let mut pts = [[0.0_f32; 2]; 2];
            let mut found = 0usize;
            for (p0, p1) in edges {
                if (p0[2] <= z) != (p1[2] <= z) {
                    let dz = p1[2] - p0[2];
                    if dz.abs() > 1e-6 && found < 2 {
                        let t = ((z - p0[2]) / dz).clamp(0.0, 1.0);
                        pts[found] = [p0[0] + t * (p1[0] - p0[0]), p0[1] + t * (p1[1] - p0[1])];
                        found += 1;
                    }
                }
            }
            if found == 2 {
                segs.push((pts[0], pts[1]));
            }
        }
        if segs.is_empty() {
            continue;
        }
        for iy in 0..ny {
            let y = ov[0][1] + (iy as f32 + 0.5) * step;
            xs.clear();
            for &(p0, p1) in &segs {
                if (p0[1] <= y) != (p1[1] <= y) {
                    let dy = p1[1] - p0[1];
                    if dy.abs() > 1e-6 {
                        let t = ((y - p0[1]) / dy).clamp(0.0, 1.0);
                        xs.push(p0[0] + t * (p1[0] - p0[0]));
                    }
                }
            }
            if xs.len() < 2 {
                continue;
            }
            xs.sort_by(f32::total_cmp);
            for &[x0, x1] in xs.as_chunks::<2>().0 {
                let ix0 = (((x0 - ov[0][0]) / step).floor().max(0.0) as usize).min(nx);
                let ix1 = (((x1 - ov[0][0]) / step).ceil().max(0.0) as usize).min(nx);
                for ix in ix0..ix1 {
                    mask[(iz * ny + iy) * nx + ix] = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IDENTITY_TRANSFORM, MachineProfile};
    use std::sync::Arc;

    fn cube_mesh(side: f32) -> Arc<TriMesh> {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [side, 0.0, 0.0],
            [side, side, 0.0],
            [0.0, side, 0.0],
            [0.0, 0.0, side],
            [side, 0.0, side],
            [side, side, side],
            [0.0, side, side],
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
        Arc::new(TriMesh::from_indexed(positions, indices).expect("cube"))
    }

    #[test]
    fn collision_detects_overlap_and_emits_voxels() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("Cube20", cube_mesh(20.0));
        let a = job.add_instance(pid, IDENTITY_TRANSFORM).expect("a");
        let b = job
            .add_instance(pid, [[1.0, 0.0, 0.0, 10.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]])
            .expect("b");

        let rep = detect_collisions(&job, None, 2.5);
        assert_eq!(rep.pairs.len(), 1);
        assert!(rep.pairs[0].is_hard_intersection);
        assert!(rep.colliding_ids.contains(&a));
        assert!(rep.colliding_ids.contains(&b));
        assert!(!rep.overlap_voxels.is_empty(), "expected 3D overlap voxels");
    }

    fn translated(x: f32, y: f32, z: f32) -> Transform3x4 {
        [[1.0, 0.0, 0.0, x], [0.0, 1.0, 0.0, y], [0.0, 0.0, 1.0, z]]
    }

    #[test]
    fn incremental_cache_only_retests_moved_part_and_preserves_static_pairs() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("Cube20", cube_mesh(20.0));
        let a = job.add_instance(pid, translated(0.0, 0.0, 0.0)).expect("a");
        let b = job.add_instance(pid, translated(10.0, 0.0, 0.0)).expect("b");
        let c = job.add_instance(pid, translated(100.0, 0.0, 0.0)).expect("c");
        let d = job.add_instance(pid, translated(150.0, 0.0, 0.0)).expect("d");

        let mut cache = CollisionCache::new();
        assert_eq!(cache.diff(&job, 3.0), CollisionDiff::FullRebuild);
        let rep0 = cache.sync(&job, 3.0);
        assert_eq!(cache.last_tested_pairs(), 6);
        assert_eq!(rep0.pairs.len(), 1);
        assert_eq!(rep0.pairs[0].a, a);
        assert_eq!(rep0.pairs[0].b, b);
        let static_voxels = rep0.overlap_voxels.clone();
        assert!(!static_voxels.is_empty());

        // No-op sync does zero work.
        assert_eq!(cache.diff(&job, 3.0), CollisionDiff::NoChange);
        let rep_noop = cache.sync(&job, 3.0);
        assert_eq!(cache.last_tested_pairs(), 0);
        assert_eq!(rep_noop, rep0);

        // Move only `c` so it overlaps `d`: only 3 pairs (c vs a, b, d) should be tested.
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == c) {
            inst.set_translation([140.0, 0.0, 0.0]);
        }
        assert_eq!(
            cache.diff(&job, 3.0),
            CollisionDiff::Incremental {
                dirty_ids: HashSet::from([c]),
                removed_ids: HashSet::new(),
            }
        );
        let rep1 = cache.sync(&job, 3.0);
        assert_eq!(cache.last_tested_pairs(), 3);
        assert_eq!(rep1.pairs.len(), 2);
        assert!(rep1.colliding_ids.contains(&a));
        assert!(rep1.colliding_ids.contains(&b));
        assert!(rep1.colliding_ids.contains(&c));
        assert!(rep1.colliding_ids.contains(&d));
        // Untouched static pair's overlap geometry is preserved (downsampled only if total > cap).
        assert!(static_voxels.iter().any(|v| rep1.overlap_voxels.contains(v)));
        assert!(rep0.overlap_quads.iter().any(|q| rep1.overlap_quads.contains(q)));

        // Move `c` back into free space: zero AABB candidate pairs, resolves (c, d) while keeping (a, b).
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == c) {
            inst.set_translation([100.0, 0.0, 0.0]);
        }
        assert_eq!(cache.count_candidate_pairs(&job, &HashSet::from([c])), 0);
        let rep2 = cache.sync(&job, 3.0);
        assert_eq!(rep2.pairs.len(), 1);
        assert_eq!(rep2.pairs[0].a, a);
        assert_eq!(rep2.pairs[0].b, b);
        assert_eq!(rep2.overlap_voxels, static_voxels);
        assert_eq!(rep2.overlap_quads, rep0.overlap_quads);
        assert_eq!(rep2.intersection_segments, rep0.intersection_segments);

        // Hiding `b` is a removal-only update testing 0 pairs and clearing (a, b).
        assert!(job.set_instance_hidden(b, true));
        assert_eq!(
            cache.diff(&job, 3.0),
            CollisionDiff::RemovalsOnly {
                removed_ids: HashSet::from([b]),
            }
        );
        let rep3 = cache.sync(&job, 3.0);
        assert_eq!(cache.last_tested_pairs(), 0);
        assert!(rep3.pairs.is_empty());
        assert!(rep3.overlap_voxels.is_empty());

        // Changing clearance triggers FullRebuild.
        job.clearance_mm = 4.0;
        assert_eq!(cache.diff(&job, 3.0), CollisionDiff::FullRebuild);
    }

    #[test]
    fn incremental_cache_detects_and_clears_clearance_violation() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.clearance_mm = 2.5;
        let pid = job.add_part("Cube20", cube_mesh(20.0));
        let a = job.add_instance(pid, translated(0.0, 0.0, 0.0)).expect("a");
        let b = job.add_instance(pid, translated(22.0, 0.0, 0.0)).expect("b");

        let direct = detect_collisions(&job, None, 1.0);
        assert_eq!(direct.pairs.len(), 1);
        assert!(!direct.pairs[0].is_hard_intersection);
        assert!(!direct.overlap_voxels.is_empty());

        let mut cache = CollisionCache::new();
        let cached = cache.sync(&job, 1.0);
        assert_eq!(cached.pairs, direct.pairs);
        assert_eq!(cached.colliding_ids, HashSet::from([a, b]));
        assert!(!cached.overlap_voxels.is_empty());

        // Move `b` past clearance (gap = 5.0 mm > 2.5 mm) -> clears.
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == b) {
            inst.set_translation([25.0, 0.0, 0.0]);
        }
        let cleared = cache.sync(&job, 1.0);
        assert!(cleared.pairs.is_empty());
        assert!(cleared.colliding_ids.is_empty());
        assert!(cleared.overlap_voxels.is_empty());
    }

    #[test]
    fn collision_extracts_exact_boundary_quads_intersection_seams_and_contacts() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.clearance_mm = 2.5;
        let pid = job.add_part("Cube20", cube_mesh(20.0));
        let _a = job.add_instance(pid, translated(0.0, 0.0, 0.0)).expect("a");
        let b = job.add_instance(pid, translated(10.0, 0.0, 0.0)).expect("b");

        let hard_rep = detect_collisions(&job, None, 3.0);
        assert!(!hard_rep.overlap_quads.is_empty());
        assert!(hard_rep.overlap_quads.iter().all(|q| q.is_hard));
        assert!(!hard_rep.intersection_segments.is_empty());
        for &(p0, p1) in &hard_rep.intersection_segments {
            for p in [p0, p1] {
                let on_x_face = (p[0] - 10.0).abs() <= 1e-3 || (p[0] - 20.0).abs() <= 1e-3;
                let on_yz_face = p[1].abs() <= 1e-3
                    || (p[1] - 20.0).abs() <= 1e-3
                    || p[2].abs() <= 1e-3
                    || (p[2] - 20.0).abs() <= 1e-3;
                assert!(
                    on_x_face || on_yz_face,
                    "seam point {p:?} should lie on cube intersection boundary"
                );
                assert!(
                    p[0] >= 10.0 - 1e-3 && p[0] <= 20.0 + 1e-3,
                    "seam point x out of overlap: {p:?}"
                );
                assert!(p[1] >= -1e-3 && p[1] <= 20.0 + 1e-3, "seam point y out of range: {p:?}");
                assert!(p[2] >= -1e-3 && p[2] <= 20.0 + 1e-3, "seam point z out of range: {p:?}");
            }
        }

        // Move `b` to x = 22.0 (2.0 mm air gap < 2.5 mm clearance) -> clearance violation with contact.
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == b) {
            inst.set_translation([22.0, 0.0, 0.0]);
        }
        let clear_rep = detect_collisions(&job, None, 1.0);
        assert!(!clear_rep.overlap_quads.is_empty());
        assert!(clear_rep.overlap_quads.iter().all(|q| !q.is_hard));
        assert_eq!(clear_rep.contacts.len(), 1);
        let c = clear_rep.contacts[0];
        assert!(!c.is_hard);
        let dx = c.point_b[0] - c.point_a[0];
        let dy = c.point_b[1] - c.point_a[1];
        let dz = c.point_b[2] - c.point_a[2];
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        assert!((dist - 2.0).abs() < 1e-3, "expected 2.0 mm contact gap, got {dist}");
    }

    #[test]
    fn coarse_pitch_does_not_flag_valid_clearance_on_45_deg_rotated_parts() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.clearance_mm = 2.5;
        let pid = job.add_part("Cube20", cube_mesh(20.0));
        let c = std::f32::consts::FRAC_1_SQRT_2;
        let tf_a = [[c, -c, 0.0, 0.0], [c, c, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];
        let tf_b_ok = [
            [c, -c, 0.0, 23.0 * c],
            [c, c, 0.0, 23.0 * c],
            [0.0, 0.0, 1.0, 0.0],
        ];
        let _a = job.add_instance(pid, tf_a).expect("a");
        let b = job.add_instance(pid, tf_b_ok).expect("b");

        let ok_rep = detect_collisions(&job, None, 3.0);
        assert!(
            ok_rep.pairs.is_empty(),
            "3.0 mm gap with 2.5 mm clearance must not report false collision at 3.0 mm pitch: {:?}",
            ok_rep.pairs
        );

        // Move `b` to a 2.0 mm gap (< 2.5 mm clearance): must report 1 non-hard clearance violation.
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == b) {
            inst.set_translation([22.0 * c, 22.0 * c, 0.0]);
        }
        let bad_rep = detect_collisions(&job, None, 3.0);
        assert_eq!(bad_rep.pairs.len(), 1);
        assert!(!bad_rep.pairs[0].is_hard_intersection);
    }
}
