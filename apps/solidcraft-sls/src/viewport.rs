//! 3D viewport scene builder, hardware instancing, camera controls, and ray picking.

use std::collections::HashMap;
use std::sync::Arc;

use solidcraft_sls_core::model::{Aabb, BuildJob, InstanceId, MachineProfile, PartId, Transform3x4, TriMesh};
use solidcraft_theme::Tokens;
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
    tokens: &Tokens,
    cache: &mut HashMap<PartId, Arc<SharedTris>>,
) -> GpuScene {
    let mut scene = GpuScene::with_palette(Palette::default());

    let part_color = scene.color(tokens.text.to_array());
    let selected_color = scene.color(tokens.accent.to_array());
    let oob_color = scene.color(tokens.form_error.to_array());
    let box_color = scene.color(tokens.grid_major.to_array());
    let grid_color = scene.color(tokens.grid.to_array());
    let axis_x = scene.color(tokens.axis_x.to_array());
    let axis_y = scene.color(tokens.axis_y.to_array());
    let axis_z = scene.color(tokens.axis_z.to_array());

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

/// Compute world-space `(origin, unit_dir)` ray from `cam` through `pointer` inside `rect`.
pub fn screen_ray(
    cam: &Camera,
    rect: egui::Rect,
    pointer: egui::Pos2,
) -> Option<([f32; 3], [f32; 3])> {
    if !rect.contains(pointer) || rect.width() <= 1.0 || rect.height() <= 1.0 {
        return None;
    }
    let px = f64::from(pointer.x - rect.left());
    let py = f64::from(pointer.y - rect.top());
    let (origin, dir) = cam.ray(px, py, f64::from(rect.width()), f64::from(rect.height()));
    Some((origin.to_f32(), dir.to_f32()))
}

/// Intersect a screen ray with a horizontal `Z = plane_z` plane (for XY dragging).
pub fn intersect_horizontal_plane(
    cam: &Camera,
    rect: egui::Rect,
    pointer: egui::Pos2,
    plane_z: f32,
) -> Option<[f32; 3]> {
    let (origin, dir) = screen_ray(cam, rect, pointer)?;
    if dir[2].abs() < 1e-6 {
        return None;
    }
    let t = (plane_z - origin[2]) / dir[2];
    if t < 0.0 || !t.is_finite() {
        return None;
    }
    Some([origin[0] + dir[0] * t, origin[1] + dir[1] * t, plane_z])
}

/// Intersect a screen ray with a vertical camera-facing plane through `anchor` (for Z dragging).
pub fn intersect_vertical_plane(
    cam: &Camera,
    rect: egui::Rect,
    pointer: egui::Pos2,
    anchor: [f32; 3],
) -> Option<f32> {
    let (origin, dir) = screen_ray(cam, rect, pointer)?;
    let back = cam.back().to_f32();
    let nx = back[0];
    let ny = back[1];
    let len = (nx * nx + ny * ny).sqrt();
    if len < 1e-6 {
        return None;
    }
    let (nx, ny) = (nx / len, ny / len);
    let denom = dir[0] * nx + dir[1] * ny;
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = ((anchor[0] - origin[0]) * nx + (anchor[1] - origin[1]) * ny) / denom;
    if t < 0.0 || !t.is_finite() {
        return None;
    }
    Some(origin[2] + dir[2] * t)
}

/// Pick the closest instance whose world AABB and triangles are hit by a screen-space pointer ray.
pub fn pick_instance(
    job: &BuildJob,
    cam: &Camera,
    rect: egui::Rect,
    pointer: egui::Pos2,
) -> Option<InstanceId> {
    let (ray_origin, ray_dir) = screen_ray(cam, rect, pointer)?;

    let mut best: Option<(f32, InstanceId)> = None;
    for inst in &job.instances {
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let wb = inst.world_bounds(proto.mesh.bounds);
        if ray_aabb_intersect(ray_origin, ray_dir, wb).is_none() {
            continue;
        }
        let (local_origin, local_dir) = inverse_transform_ray(&inst.transform, ray_origin, ray_dir);
        for tri in &proto.mesh.indices {
            let a = proto.mesh.positions[tri[0] as usize];
            let b = proto.mesh.positions[tri[1] as usize];
            let c = proto.mesh.positions[tri[2] as usize];
            if let Some(t) = ray_triangle_intersect(local_origin, local_dir, a, b, c)
                && best.is_none_or(|(best_t, _)| t < best_t) {
                    best = Some((t, inst.id));
                }
        }
    }
    best.map(|(_, id)| id)
}

fn inverse_transform_ray(
    m: &Transform3x4,
    origin: [f32; 3],
    dir: [f32; 3],
) -> ([f32; 3], [f32; 3]) {
    let dx = origin[0] - m[0][3];
    let dy = origin[1] - m[1][3];
    let dz = origin[2] - m[2][3];
    // Transpose of orthonormal 3x3 rotation part
    let local_o = [
        m[0][0] * dx + m[1][0] * dy + m[2][0] * dz,
        m[0][1] * dx + m[1][1] * dy + m[2][1] * dz,
        m[0][2] * dx + m[1][2] * dy + m[2][2] * dz,
    ];
    let local_d = [
        m[0][0] * dir[0] + m[1][0] * dir[1] + m[2][0] * dir[2],
        m[0][1] * dir[0] + m[1][1] * dir[1] + m[2][1] * dir[2],
        m[0][2] * dir[0] + m[1][2] * dir[1] + m[2][2] * dir[2],
    ];
    (local_o, local_d)
}

fn ray_triangle_intersect(
    origin: [f32; 3],
    dir: [f32; 3],
    v0: [f32; 3],
    v1: [f32; 3],
    v2: [f32; 3],
) -> Option<f32> {
    let e1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
    let e2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];
    let h = [
        dir[1] * e2[2] - dir[2] * e2[1],
        dir[2] * e2[0] - dir[0] * e2[2],
        dir[0] * e2[1] - dir[1] * e2[0],
    ];
    let a = e1[0] * h[0] + e1[1] * h[1] + e1[2] * h[2];
    if a.abs() < 1e-8 {
        return None;
    }
    let f = 1.0 / a;
    let s = [origin[0] - v0[0], origin[1] - v0[1], origin[2] - v0[2]];
    let u = f * (s[0] * h[0] + s[1] * h[1] + s[2] * h[2]);
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = [
        s[1] * e1[2] - s[2] * e1[1],
        s[2] * e1[0] - s[0] * e1[2],
        s[0] * e1[1] - s[1] * e1[0],
    ];
    let v = f * (dir[0] * q[0] + dir[1] * q[1] + dir[2] * q[2]);
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = f * (e2[0] * q[0] + e2[1] * q[1] + e2[2] * q[2]);
    (t > 1e-5).then_some(t)
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
        let scene = build_gpu_scene(&job, Some(i1), &Tokens::get(), &mut cache);
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
                vec![
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
                ],
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

    #[test]
    fn ray_misses_empty_aabb_corner_of_tetrahedron() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [0.0, 20.0, 0.0], [0.0, 0.0, 20.0]],
                vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
            )
            .expect("valid tet"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("tet", mesh);
        let _iid = job.add_instance_centered_on_floor(pid).expect("placed");

        // Top-down camera looking at the empty (18, 18, 18) corner of the tet's AABB.
        let cam = Camera {
            target: [88.0, 113.0, 10.0].into(),
            yaw: 0.0,
            pitch: 1.54,
            roll: 0.0,
            distance: 100.0,
            fov: 0.5,
            up: UpAxis::Z,
        };
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        assert_eq!(pick_instance(&job, &cam, rect, rect.center()), None);
    }
}
