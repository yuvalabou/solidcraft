//! 3D sliding-box sintered thermal density analysis for SLS powder beds.

use crate::model::{BuildJob, InstanceId, Transform3x4};
use crate::report::is_bounds_inside_machine;

/// Per-instance thermal density summary.
#[derive(Debug, Clone, PartialEq)]
pub struct PartThermalReport {
    /// Instance identifier.
    pub instance_id: InstanceId,
    /// Part prototype name.
    pub name: String,
    /// Maximum local sintered volume fraction (`0.0..=100.0` %) in a 20×20×20 mm neighborhood.
    pub peak_density_pct: f32,
    /// Mean local sintered volume fraction (`0.0..=100.0` %) across the part's voxels.
    pub mean_density_pct: f32,
    /// World-space `[x, y, z]` mm coordinate of the part's hottest voxel.
    pub hotspot_xyz: [f32; 3],
}

/// Chamber-wide 3D thermal density map result.
#[derive(Debug, Clone, PartialEq)]
pub struct ThermalMapReport {
    /// Voxel pitch used in mm.
    pub pitch_mm: f32,
    /// Sliding neighborhood box size in mm (default `20.0`).
    pub kernel_mm: f32,
    /// Peak local sintered density (`0.0..=100.0` %) across all included parts.
    pub peak_density_pct: f32,
    /// World-space `[x, y, z]` mm coordinate of the global hotspot.
    pub hotspot_xyz: [f32; 3],
    /// Number of parts exceeding the 30% thermal hotspot threshold.
    pub hotspot_part_count: usize,
    /// Number of visible placed parts excluded for being outside the machine.
    pub excluded_oob_count: usize,
    /// Per-part thermal summaries sorted hottest-first.
    pub parts: Vec<PartThermalReport>,
}

/// Map a local sintered density percentage (`0..=100`) to an RGBA color ramp.
pub fn thermal_rgba(density_pct: f32) -> [u8; 4] {
    let t = (density_pct / 35.0).clamp(0.0, 1.0);
    // 0.0 -> blue, 0.43 (~15%) -> green, 0.71 (~25%) -> amber, 1.0 (35%+) -> red
    let (c0, c1, u) = if t < 0.43 {
        ([59.0_f32, 130.0, 246.0], [34.0_f32, 197.0, 94.0], t / 0.43)
    } else if t < 0.71 {
        ([34.0_f32, 197.0, 94.0], [245.0_f32, 158.0, 11.0], (t - 0.43) / 0.28)
    } else {
        ([245.0_f32, 158.0, 11.0], [239.0_f32, 68.0, 68.0], (t - 0.71) / 0.29)
    };
    [
        (c0[0] + (c1[0] - c0[0]) * u).round() as u8,
        (c0[1] + (c1[1] - c0[1]) * u).round() as u8,
        (c0[2] + (c1[2] - c0[2]) * u).round() as u8,
        255,
    ]
}

impl ThermalMapReport {
    /// Compute a 20×20×20 mm sliding-box thermal density map for visible parts inside the machine.
    pub fn compute(job: &BuildJob) -> Self {
        Self::compute_with_progress(job, None)
    }

    /// Compute a 20×20×20 mm sliding-box thermal density map with optional `(done, total)` progress callback.
    pub fn compute_with_progress(job: &BuildJob, progress: Option<&dyn Fn(usize, usize)>) -> Self {
        const KERNEL_MM: f32 = 20.0;
        let ([min_x, min_y], [max_x, max_y]) = job.bed_xy_bounds();
        let min_z = job.min_z_mm;
        let max_z = job.machine.size_mm()[2];
        let span_x = (max_x - min_x).max(10.0);
        let span_y = (max_y - min_y).max(10.0);
        let span_z = (max_z - min_z).max(10.0);

        // Keep grid <= 100 cells per axis for instant interactive response.
        let max_span = span_x.max(span_y).max(span_z);
        let pitch = (max_span / 96.0).max(5.0);
        let nx = ((span_x / pitch).ceil() as usize).clamp(1, 100);
        let ny = ((span_y / pitch).ceil() as usize).clamp(1, 100);
        let nz = ((span_z / pitch).ceil() as usize).clamp(1, 100);

        let total_cells = nx * ny * nz;
        // Stores 0 for empty, or 1-based index into `included` for occupied voxels.
        let mut occ = vec![0u32; total_cells];
        let mut excluded_oob_count = 0usize;

        struct IncludedPart {
            instance_id: InstanceId,
            name: String,
            sum_density: f64,
            voxel_count: usize,
            peak_density_pct: f32,
            hotspot_xyz: [f32; 3],
        }

        let mut included: Vec<IncludedPart> = Vec::new();
        let total_insts = job.instances.len().max(1);
        if let Some(cb) = progress {
            cb(0, total_insts);
        }

        for (step_idx, inst) in job.instances.iter().enumerate() {
            if let Some(cb) = progress {
                cb(step_idx + 1, total_insts);
            }
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
            let part_tag = (included.len() + 1) as u32;
            included.push(IncludedPart {
                instance_id: inst.id,
                name: proto.name.clone(),
                sum_density: 0.0,
                voxel_count: 0,
                peak_density_pct: 0.0,
                hotspot_xyz: [
                    (wb[0][0] + wb[1][0]) * 0.5,
                    (wb[0][1] + wb[1][1]) * 0.5,
                    (wb[0][2] + wb[1][2]) * 0.5,
                ],
            });

            voxelize_part_into(
                &proto.mesh.positions,
                &proto.mesh.indices,
                inst.transform,
                wb,
                [min_x, min_y, min_z],
                pitch,
                [nx, ny, nz],
                part_tag,
                &mut occ,
            );
        }

        if included.is_empty() {
            return Self {
                pitch_mm: pitch,
                kernel_mm: KERNEL_MM,
                peak_density_pct: 0.0,
                hotspot_xyz: [0.0, 0.0, 0.0],
                hotspot_part_count: 0,
                excluded_oob_count,
                parts: Vec::new(),
            };
        }

        // Build 3D prefix sum over binary occupancy (1 if occupied, 0 if empty).
        let sx = nx + 1;
        let sy = ny + 1;
        let sz = nz + 1;
        let mut pref = vec![0u32; sx * sy * sz];
        let p_idx = |x: usize, y: usize, z: usize| -> usize { (z * sy + y) * sx + x };

        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let val = u32::from(occ[(z * ny + y) * nx + x] != 0);
                    let sum = val
                        + pref[p_idx(x, y + 1, z + 1)]
                        + pref[p_idx(x + 1, y, z + 1)]
                        + pref[p_idx(x + 1, y + 1, z)]
                        - pref[p_idx(x, y, z + 1)]
                        - pref[p_idx(x, y + 1, z)]
                        - pref[p_idx(x + 1, y, z)]
                        + pref[p_idx(x, y, z)];
                    pref[p_idx(x + 1, y + 1, z + 1)] = sum;
                }
            }
        }

        let r = ((KERNEL_MM * 0.5) / pitch).round().max(1.0) as usize;
        let mut global_peak = 0.0_f32;
        let mut global_hotspot = [0.0_f32; 3];

        for z in 0..nz {
            let z0 = z.saturating_sub(r);
            let z1 = (z + r + 1).min(nz);
            for y in 0..ny {
                let y0 = y.saturating_sub(r);
                let y1 = (y + r + 1).min(ny);
                for x in 0..nx {
                    let tag = occ[(z * ny + y) * nx + x];
                    if tag == 0 {
                        continue;
                    }
                    let x0 = x.saturating_sub(r);
                    let x1 = (x + r + 1).min(nx);
                    let solid = pref[p_idx(x1, y1, z1)]
                        + pref[p_idx(x0, y0, z1)]
                        + pref[p_idx(x0, y1, z0)]
                        + pref[p_idx(x1, y0, z0)]
                        - pref[p_idx(x0, y1, z1)]
                        - pref[p_idx(x1, y0, z1)]
                        - pref[p_idx(x1, y1, z0)]
                        - pref[p_idx(x0, y0, z0)];
                    let box_cells = ((x1 - x0) * (y1 - y0) * (z1 - z0)).max(1) as f32;
                    let pct = (solid as f32 / box_cells) * 100.0;
                    let world = [
                        min_x + (x as f32 + 0.5) * pitch,
                        min_y + (y as f32 + 0.5) * pitch,
                        min_z + (z as f32 + 0.5) * pitch,
                    ];

                    if let Some(part) = included.get_mut((tag - 1) as usize) {
                        part.sum_density += f64::from(pct);
                        part.voxel_count += 1;
                        if pct > part.peak_density_pct {
                            part.peak_density_pct = pct;
                            part.hotspot_xyz = world;
                        }
                    }
                    if pct > global_peak {
                        global_peak = pct;
                        global_hotspot = world;
                    }
                }
            }
        }

        let mut parts: Vec<PartThermalReport> = included
            .into_iter()
            .map(|p| PartThermalReport {
                instance_id: p.instance_id,
                name: p.name,
                peak_density_pct: p.peak_density_pct,
                mean_density_pct: if p.voxel_count > 0 {
                    (p.sum_density / p.voxel_count as f64) as f32
                } else {
                    0.0
                },
                hotspot_xyz: p.hotspot_xyz,
            })
            .collect();
        parts.sort_by(|a, b| b.peak_density_pct.total_cmp(&a.peak_density_pct));
        let hotspot_part_count = parts.iter().filter(|p| p.peak_density_pct >= 30.0).count();

        Self {
            pitch_mm: pitch,
            kernel_mm: KERNEL_MM,
            peak_density_pct: global_peak,
            hotspot_xyz: global_hotspot,
            hotspot_part_count,
            excluded_oob_count,
            parts,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn voxelize_part_into(
    positions: &[[f32; 3]],
    indices: &[[u32; 3]],
    tr: Transform3x4,
    wb: [[f32; 3]; 2],
    origin: [f32; 3],
    pitch: f32,
    dims: [usize; 3],
    tag: u32,
    occ: &mut [u32],
) {
    let [nx, ny, nz] = dims;
    let world_verts: Vec<[f32; 3]> = positions
        .iter()
        .map(|p| {
            [
                tr[0][0] * p[0] + tr[0][1] * p[1] + tr[0][2] * p[2] + tr[0][3],
                tr[1][0] * p[0] + tr[1][1] * p[1] + tr[1][2] * p[2] + tr[1][3],
                tr[2][0] * p[0] + tr[2][1] * p[1] + tr[2][2] * p[2] + tr[2][3],
            ]
        })
        .collect();

    let z_lo = (((wb[0][2] - origin[2]) / pitch).floor().max(0.0) as usize).min(nz);
    let z_hi = (((wb[1][2] - origin[2]) / pitch).ceil().max(0.0) as usize).min(nz);
    let y_lo = (((wb[0][1] - origin[1]) / pitch).floor().max(0.0) as usize).min(ny);
    let y_hi = (((wb[1][1] - origin[1]) / pitch).ceil().max(0.0) as usize).min(ny);

    let mut segs: Vec<([f32; 2], [f32; 2])> = Vec::new();
    let mut xs: Vec<f32> = Vec::new();

    for iz in z_lo..z_hi {
        let z = origin[2] + (iz as f32 + 0.5) * pitch;
        segs.clear();
        for tri in indices {
            let a = world_verts[tri[0] as usize];
            let b = world_verts[tri[1] as usize];
            let c = world_verts[tri[2] as usize];
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
        for iy in y_lo..y_hi {
            let y = origin[1] + (iy as f32 + 0.5) * pitch;
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
                let x_start = (((x0 - origin[0]) / pitch).floor().max(0.0) as usize).min(nx);
                let x_end = (((x1 - origin[0]) / pitch).ceil().max(0.0) as usize).min(nx);
                for ix in x_start..x_end {
                    occ[(iz * ny + iy) * nx + ix] = tag;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MachineProfile, TriMesh};
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
    fn thermal_map_flags_solid_cube_hotspot() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("SolidBlock", cube_mesh(30.0));
        let tr = [[1.0, 0.0, 0.0, 20.0], [0.0, 1.0, 0.0, 20.0], [0.0, 0.0, 1.0, 0.0]];
        job.add_instance(pid, tr).expect("in");

        let rep = ThermalMapReport::compute(&job);
        assert_eq!(rep.parts.len(), 1);
        assert!(rep.peak_density_pct > 50.0, "expected hotspot > 50%, got {}", rep.peak_density_pct);
        assert_eq!(rep.hotspot_part_count, 1);
    }
}
