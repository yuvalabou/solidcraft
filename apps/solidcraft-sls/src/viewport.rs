//! 3D viewport scene builder, hardware instancing, camera controls, and ray picking.

use std::collections::HashMap;
use std::sync::Arc;

use solidcraft_sls_core::model::{Aabb, BuildJob, InstanceId, MachineProfile, PartId, TriMesh};
use solidcraft_viewport::{Camera, UpAxis};
use solidcraft_viewport_gpu::view::{GpuScene, InstanceLook, InstancedMesh, Palette, PaletteIndex, SharedTris};

/// Convert a pure-Rust [`TriMesh`] into GPU-ready flat-shaded [`SharedTris`].
pub fn mesh_to_shared_tris(mesh: &TriMesh) -> Arc<SharedTris> {
    let mut vertices = Vec::with_capacity(mesh.indices.len() * 3 * 24);
    let mut indices = Vec::with_capacity(mesh.indices.len() * 3);
    let mut next_idx = 0_u32;

    for (tri_i, tri) in mesh.indices.iter().enumerate() {
        let n = mesh.tri_normals.get(tri_i).copied().unwrap_or([0.0, 0.0, 1.0]);
        for &vi in tri {
            let p = mesh.positions.get(vi as usize).copied().unwrap_or([0.0, 0.0, 0.0]);
            for f in p.into_iter().chain(n) {
                vertices.extend_from_slice(&f.to_le_bytes());
            }
            indices.push(next_idx);
            next_idx += 1;
        }
    }

    Arc::new(SharedTris {
        vertices,
        indices,
        flat: true,
        bounds: mesh.bounds,
    })
}

/// Build the [`GpuScene`] for `job`, including build-chamber wireframe, floor grid, and instanced parts.
pub fn build_gpu_scene(
    job: &BuildJob,
    selected: Option<InstanceId>,
    cache: &mut HashMap<PartId, Arc<SharedTris>>,
) -> GpuScene {
    let mut scene = GpuScene::with_palette(Palette::default());

    let part_color = scene.color([210, 214, 220, 255]);
    let selected_color = scene.color([238, 178, 56, 255]);
    let oob_color = scene.color([220, 68, 68, 255]);
    let box_color = scene.color([120, 145, 175, 220]);
    let grid_color = scene.color([82, 90, 102, 160]);
    let axis_x = scene.color([210, 80, 80, 230]);
    let axis_y = scene.color([80, 190, 95, 230]);
    let axis_z = scene.color([80, 145, 225, 230]);

    draw_build_chamber(&mut scene, &job.machine, box_color, grid_color, [axis_x, axis_y, axis_z]);

    let mut per_part: HashMap<PartId, InstancedMesh> = HashMap::new();
    for inst in &job.instances {
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let tris = cache
            .entry(proto.id)
            .or_insert_with(|| mesh_to_shared_tris(&proto.mesh))
            .clone();
        let entry = per_part.entry(proto.id).or_insert_with(|| InstancedMesh {
            key: u64::from(proto.id.0),
            tris,
            instances: Vec::new(),
            look: InstanceLook::Opaque,
            levels: Vec::new(),
            closed: true,
        });

        let color = if !job.is_instance_in_bounds(inst.id) {
            oob_color
        } else if Some(inst.id) == selected {
            selected_color
        } else {
            part_color
        };
        entry.place(inst.transform, color);
    }

    scene.instanced = per_part.into_values().collect();
    scene
}

fn draw_build_chamber(
    scene: &mut GpuScene,
    machine: &MachineProfile,
    box_color: PaletteIndex,
    grid_color: PaletteIndex,
    axes: [PaletteIndex; 3],
) {
    let [w, d, h] = machine.size_mm();

    // 12 edges of [0..w] x [0..d] x [0..h]
    let edges = [
        ([0.0, 0.0, 0.0], [w, 0.0, 0.0]),
        ([w, 0.0, 0.0], [w, d, 0.0]),
        ([w, d, 0.0], [0.0, d, 0.0]),
        ([0.0, d, 0.0], [0.0, 0.0, 0.0]),
        ([0.0, 0.0, h], [w, 0.0, h]),
        ([w, 0.0, h], [w, d, h]),
        ([w, d, h], [0.0, d, h]),
        ([0.0, d, h], [0.0, 0.0, h]),
        ([0.0, 0.0, 0.0], [0.0, 0.0, h]),
        ([w, 0.0, 0.0], [w, 0.0, h]),
        ([w, d, 0.0], [w, d, h]),
        ([0.0, d, 0.0], [0.0, d, h]),
    ];
    for (a, b) in edges {
        scene.line(a, b, box_color, 1.4, false);
    }

    // Origin triad (25 mm)
    scene.line([0.0, 0.0, 0.0], [25.0, 0.0, 0.0], axes[0], 2.2, false);
    scene.line([0.0, 0.0, 0.0], [0.0, 25.0, 0.0], axes[1], 2.2, false);
    scene.line([0.0, 0.0, 0.0], [0.0, 0.0, 25.0], axes[2], 2.2, false);

    // 20 mm floor grid at Z = 0
    let step = 20.0_f32;
    let mut x = step;
    while x < w - 0.5 {
        scene.line([x, 0.0, 0.0], [x, d, 0.0], grid_color, 1.0, false);
        x += step;
    }
    let mut y = step;
    while y < d - 0.5 {
        scene.line([0.0, y, 0.0], [w, y, 0.0], grid_color, 1.0, false);
        y += step;
    }
}

/// Create a default isometric camera framed around `machine`.
pub fn default_camera(machine: &MachineProfile) -> Camera {
    let [w, d, h] = machine.size_mm();
    let diag = (w * w + d * d + h * h).sqrt();
    Camera {
        target: [f64::from(w) * 0.5, f64::from(d) * 0.5, f64::from(h) * 0.35].into(),
        yaw: 0.72,
        pitch: 0.48,
        roll: 0.0,
        distance: f64::from(diag) * 1.15,
        fov: 0.65,
        up: UpAxis::Z,
    }
}

/// Pick the closest instance whose world AABB is hit by a screen-space pointer ray.
pub fn pick_instance(
    job: &BuildJob,
    cam: &Camera,
    rect: egui::Rect,
    pointer: egui::Pos2,
) -> Option<InstanceId> {
    if !rect.contains(pointer) || rect.width() <= 1.0 || rect.height() <= 1.0 {
        return None;
    }
    let ndc_x = ((pointer.x - rect.left()) / rect.width()) * 2.0 - 1.0;
    let ndc_y = 1.0 - ((pointer.y - rect.top()) / rect.height()) * 2.0;

    let (right, up, back) = cam.basis();
    let half_h = cam.half_height() as f32;
    let aspect = rect.width() / rect.height();
    let half_w = half_h * aspect;
    let eye = cam.eye().to_f32();
    let forward = [-back.x as f32, -back.y as f32, -back.z as f32];
    let r = [right.x as f32, right.y as f32, right.z as f32];
    let u = [up.x as f32, up.y as f32, up.z as f32];

    let (ray_origin, ray_dir) = if cam.fov > 0.0 {
        let dir = [
            forward[0] + r[0] * (ndc_x * half_w) + u[0] * (ndc_y * half_h),
            forward[1] + r[1] * (ndc_x * half_w) + u[1] * (ndc_y * half_h),
            forward[2] + r[2] * (ndc_x * half_w) + u[2] * (ndc_y * half_h),
        ];
        (eye, normalize3(dir))
    } else {
        let origin = [
            eye[0] + r[0] * (ndc_x * half_w) + u[0] * (ndc_y * half_h),
            eye[1] + r[1] * (ndc_x * half_w) + u[1] * (ndc_y * half_h),
            eye[2] + r[2] * (ndc_x * half_w) + u[2] * (ndc_y * half_h),
        ];
        (origin, forward)
    };

    let mut best: Option<(f32, InstanceId)> = None;
    for inst in &job.instances {
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let wb = inst.world_bounds(proto.mesh.bounds);
        if let Some(t) = ray_aabb_intersect(ray_origin, ray_dir, wb) {
            if best.is_none_or(|(best_t, _)| t < best_t) {
                best = Some((t, inst.id));
            }
        }
    }
    best.map(|(_, id)| id)
}

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-9 {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 0.0, -1.0]
    }
}

fn ray_aabb_intersect(origin: [f32; 3], dir: [f32; 3], aabb: Aabb) -> Option<f32> {
    let mut tmin = 0.0_f32;
    let mut tmax = f32::INFINITY;
    for k in 0..3 {
        let inv = if dir[k].abs() > 1e-8 { 1.0 / dir[k] } else { f32::INFINITY * dir[k].signum() };
        let mut t0 = (aabb[0][k] - origin[k]) * inv;
        let mut t1 = (aabb[1][k] - origin[k]) * inv;
        if t0 > t1 {
            std::mem::swap(&mut t0, &mut t1);
        }
        tmin = tmin.max(t0);
        tmax = tmax.min(t1);
        if tmax < tmin {
            return None;
        }
    }
    Some(tmin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solidcraft_sls_core::model::IDENTITY_TRANSFORM;
    use solidcraft_viewport_gpu::view::INSTANCE_SIZE;

    #[test]
    fn builds_instanced_scene_for_duplicate_parts() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
                vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
            )
            .expect("valid mesh"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("bracket", mesh);
        let i1 = job.add_instance(pid, IDENTITY_TRANSFORM).expect("i1");
        let _ = job.duplicate_instance(i1, 3).expect("duplicated");

        let mut cache = HashMap::new();
        let scene = build_gpu_scene(&job, Some(i1), &mut cache);
        assert_eq!(scene.instanced.len(), 1);
        assert_eq!(scene.instanced[0].instances.len(), 4 * INSTANCE_SIZE);
        assert!(!scene.lines.is_empty());
    }

    #[test]
    fn ray_picks_centered_instance() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![
                    [-10.0, -10.0, 0.0],
                    [10.0, -10.0, 0.0],
                    [10.0, 10.0, 0.0],
                    [-10.0, 10.0, 0.0],
                    [-10.0, -10.0, 20.0],
                    [10.0, -10.0, 20.0],
                    [10.0, 10.0, 20.0],
                    [-10.0, 10.0, 20.0],
                ],
                vec![[0, 2, 1], [0, 3, 2], [4, 5, 6], [4, 6, 7]],
            )
            .expect("valid box"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("box", mesh);
        let iid = job.add_instance_centered_on_floor(pid).expect("placed");

        let mut cam = default_camera(&job.machine);
        cam.target = [80.0, 105.0, 10.0].into();
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let picked = pick_instance(&job, &cam, rect, rect.center());
        assert_eq!(picked, Some(iid));
    }
}
