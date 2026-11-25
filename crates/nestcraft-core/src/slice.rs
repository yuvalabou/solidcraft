//! Exact 2D cross-section (sintered area) distribution across Z layers.

use crate::model::{BuildJob, Transform3x4};
use crate::report::is_bounds_inside_machine;

/// Single Z-slice sample in a layer distribution profile.
#[derive(Debug, Clone, PartialEq)]
pub struct SliceLayerSample {
    /// Slice plane height in millimetres.
    pub z_mm: f32,
    /// Total sintered cross-section area at `z_mm` in mm².
    pub area_mm2: f64,
    /// Percentage of the machine bed XY area (`area_mm2 / bed_area_mm2 * 100.0`).
    pub fill_pct: f64,
    /// Number of placed parts intersecting `z_mm`.
    pub part_count: usize,
}

/// Z-layer cross-section area distribution across a build job.
#[derive(Debug, Clone, PartialEq)]
pub struct SliceDistribution {
    /// Sampling step between consecutive Z slices in mm.
    pub layer_step_mm: f32,
    /// Lowest Z of included parts in mm (`0.0` if empty).
    pub z_min_mm: f32,
    /// Highest Z of included parts in mm (`0.0` if empty).
    pub z_max_mm: f32,
    /// Machine XY bed area in mm².
    pub bed_area_mm2: f64,
    /// Peak cross-section area across sampled layers in mm².
    pub peak_area_mm2: f64,
    /// Z height where `peak_area_mm2` occurs in mm.
    pub peak_z_mm: f32,
    /// Mean cross-section area across sampled layers in mm².
    pub mean_area_mm2: f64,
    /// Number of visible placed parts excluded for being outside the machine.
    pub excluded_oob_count: usize,
    /// Sampled Z layers ordered from bottom (`z_min`) to top (`z_max`).
    pub layers: Vec<SliceLayerSample>,
}

impl SliceDistribution {
    /// Compute the Z-slice cross-section area distribution for visible parts inside `job`'s machine.
    pub fn compute(job: &BuildJob, layer_step_mm: f32) -> Self {
        Self::compute_with_progress(job, layer_step_mm, None)
    }

    /// Compute the Z-slice cross-section area distribution with optional `(done, total)` progress callback.
    pub fn compute_with_progress(
        job: &BuildJob,
        layer_step_mm: f32,
        progress: Option<&dyn Fn(usize, usize)>,
    ) -> Self {
        let step = layer_step_mm.clamp(0.1, 20.0);
        let [bed_x, bed_y, _] = job.machine.size_mm();
        let bed_area_mm2 = (f64::from(bed_x) * f64::from(bed_y)).max(1.0);

        let mut excluded_oob_count = 0usize;
        let mut active_parts: Vec<(&[ [f32; 3] ], &[[u32; 3]], Transform3x4, f32, f32)> = Vec::new();
        let mut global_min_z = f32::INFINITY;
        let mut global_max_z = f32::NEG_INFINITY;

        for inst in &job.instances {
            if !job.is_instance_visible(inst) {
                continue;
            }
            let Some(proto) = job.part(inst.part_id) else {
                continue;
            };
            let wb = inst.world_bounds(proto.mesh.bounds);
            if !is_bounds_inside_machine(job, wb) {
                excluded_oob_count += 1;
                continue;
            }
            global_min_z = global_min_z.min(wb[0][2]);
            global_max_z = global_max_z.max(wb[1][2]);
            active_parts.push((
                &proto.mesh.positions,
                &proto.mesh.indices,
                inst.transform,
                wb[0][2],
                wb[1][2],
            ));
        }

        if active_parts.is_empty() || !global_min_z.is_finite() || !global_max_z.is_finite() {
            return Self {
                layer_step_mm: step,
                z_min_mm: 0.0,
                z_max_mm: 0.0,
                bed_area_mm2,
                peak_area_mm2: 0.0,
                peak_z_mm: 0.0,
                mean_area_mm2: 0.0,
                excluded_oob_count,
                layers: Vec::new(),
            };
        }

        let start_z = (global_min_z / step).floor() * step + step * 0.5;
        let span = (global_max_z - start_z).max(0.0);
        let count = ((span / step).floor() as usize + 1).clamp(1, 5000);
        let mut layer_areas = vec![0.0_f64; count];
        let mut layer_parts = vec![0_usize; count];

        let total_parts = active_parts.len().max(1);
        if let Some(cb) = progress {
            cb(0, total_parts);
        }

        for (part_idx, (positions, indices, tr, p_min_z, p_max_z)) in active_parts.into_iter().enumerate() {
            if let Some(cb) = progress {
                cb(part_idx + 1, total_parts);
            }
            let world_verts: Vec<[f32; 3]> = positions.iter().map(|p| transform_pt(tr, *p)).collect();
            let mut per_part_signed = vec![0.0_f64; count];
            let mut per_part_hit = vec![false; count];

            for tri in indices {
                let a = world_verts[tri[0] as usize];
                let b = world_verts[tri[1] as usize];
                let c = world_verts[tri[2] as usize];
                let t_min_z = a[2].min(b[2]).min(c[2]);
                let t_max_z = a[2].max(b[2]).max(c[2]);
                if t_max_z <= start_z {
                    continue;
                }
                let lo = ((t_min_z - start_z) / step).ceil().max(0.0) as usize;
                let hi = ((t_max_z - start_z) / step).floor().min((count - 1) as f32) as usize;
                if lo > hi || hi >= count {
                    continue;
                }
                let n = tri_normal_raw(a, b, c);
                for idx in lo..=hi {
                    let z = start_z + idx as f32 * step;
                    if let Some((p, q)) = slice_triangle_segment(a, b, c, n, z) {
                        per_part_signed[idx] += 0.5 * (p[0] * q[1] - q[0] * p[1]);
                        per_part_hit[idx] = true;
                    }
                }
            }

            // Fallback mark part count within Z bounds & accumulate non-negative part area.
            let p_lo = ((p_min_z - start_z) / step).ceil().max(0.0) as usize;
            let p_hi = ((p_max_z - start_z) / step).floor().min((count - 1) as f32) as usize;
            for idx in p_lo.min(count)..=p_hi.min(count - 1) {
                layer_areas[idx] += per_part_signed[idx].abs();
                if per_part_hit[idx] || per_part_signed[idx].abs() > 1e-6 {
                    layer_parts[idx] += 1;
                }
            }
        }

        let mut layers = Vec::with_capacity(count);
        let mut peak_area_mm2 = 0.0_f64;
        let mut peak_z_mm = start_z;
        let mut sum_area = 0.0_f64;

        for (idx, area) in layer_areas.into_iter().enumerate() {
            let z_mm = start_z + idx as f32 * step;
            let fill_pct = (area / bed_area_mm2) * 100.0;
            if area > peak_area_mm2 {
                peak_area_mm2 = area;
                peak_z_mm = z_mm;
            }
            sum_area += area;
            layers.push(SliceLayerSample {
                z_mm,
                area_mm2: area,
                fill_pct,
                part_count: layer_parts[idx],
            });
        }

        let mean_area_mm2 = if layers.is_empty() {
            0.0
        } else {
            sum_area / layers.len() as f64
        };

        Self {
            layer_step_mm: step,
            z_min_mm: global_min_z,
            z_max_mm: global_max_z,
            bed_area_mm2,
            peak_area_mm2,
            peak_z_mm,
            mean_area_mm2,
            excluded_oob_count,
            layers,
        }
    }
}

fn transform_pt(tr: Transform3x4, p: [f32; 3]) -> [f32; 3] {
    [
        tr[0][0] * p[0] + tr[0][1] * p[1] + tr[0][2] * p[2] + tr[0][3],
        tr[1][0] * p[0] + tr[1][1] * p[1] + tr[1][2] * p[2] + tr[1][3],
        tr[2][0] * p[0] + tr[2][1] * p[1] + tr[2][2] * p[2] + tr[2][3],
    ]
}

fn tri_normal_raw(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f64; 3] {
    let ux = f64::from(b[0] - a[0]);
    let uy = f64::from(b[1] - a[1]);
    let uz = f64::from(b[2] - a[2]);
    let vx = f64::from(c[0] - a[0]);
    let vy = f64::from(c[1] - a[1]);
    let vz = f64::from(c[2] - a[2]);
    [uy * vz - uz * vy, uz * vx - ux * vz, ux * vy - uy * vx]
}

/// Intersect triangle `(a, b, c)` with horizontal plane `Z = z` and orient segment CCW around `+Z`.
fn slice_triangle_segment(
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    n: [f64; 3],
    z: f32,
) -> Option<([f64; 2], [f64; 2])> {
    let edges = [(a, b), (b, c), (c, a)];
    let mut pts = [[0.0_f64; 2]; 2];
    let mut found = 0usize;

    for (p0, p1) in edges {
        let below0 = p0[2] <= z;
        let below1 = p1[2] <= z;
        if below0 != below1 {
            let dz = f64::from(p1[2] - p0[2]);
            if dz.abs() > 1e-9 {
                let t = (f64::from(z - p0[2]) / dz).clamp(0.0, 1.0);
                if found < 2 {
                    pts[found] = [
                        f64::from(p0[0]) + t * f64::from(p1[0] - p0[0]),
                        f64::from(p0[1]) + t * f64::from(p1[1] - p0[1]),
                    ];
                    found += 1;
                }
            }
        }
    }

    if found < 2 {
        return None;
    }

    let (p, q) = (pts[0], pts[1]);
    // Orient p -> q so that (q - p) × (+Z) aligns with outward triangle normal n.
    let cross_dot = (q[1] - p[1]) * n[0] - (q[0] - p[0]) * n[1];
    if cross_dot >= 0.0 {
        Some((p, q))
    } else {
        Some((q, p))
    }
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
    fn slice_distribution_computes_exact_box_cross_section_and_excludes_oob() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("Box20x10x10", box_mesh(20.0, 10.0, 10.0));
        let tr_in = [[1.0, 0.0, 0.0, 10.0], [0.0, 1.0, 0.0, 10.0], [0.0, 0.0, 1.0, 0.0]];
        let tr_out = [[1.0, 0.0, 0.0, 500.0], [0.0, 1.0, 0.0, 10.0], [0.0, 0.0, 1.0, 0.0]];
        job.add_instance(pid, tr_in).expect("in");
        job.add_instance(pid, tr_out).expect("out");

        let dist = SliceDistribution::compute(&job, 1.0);
        assert_eq!(dist.excluded_oob_count, 1);
        assert_eq!(dist.layers.len(), 10);
        assert!((dist.peak_area_mm2 - 200.0).abs() < 1e-3, "got {}", dist.peak_area_mm2);
        assert!((dist.mean_area_mm2 - 200.0).abs() < 1e-3, "got {}", dist.mean_area_mm2);
        assert!(dist.layers.iter().all(|l| l.part_count == 1));
    }
}
