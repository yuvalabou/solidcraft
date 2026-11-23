//! 3D viewport scene builder, hardware instancing, camera controls, and ray picking.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nestcraft_core::model::{Aabb, BedOrigin, BuildJob, InstanceId, MachineProfile, PartId, Transform3x4, TriMesh};
use solidcraft_theme::Tokens;
use solidcraft_viewport::{Camera, StandardView, UpAxis};
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
    selected: &HashSet<InstanceId>,
    tokens: &Tokens,
    cache: &mut HashMap<PartId, Arc<SharedTris>>,
) -> GpuScene {
    let mut scene = GpuScene::with_palette(Palette::default());

    let part_color = scene.color(tokens.text.to_array());
    let selected_color = scene.color(tokens.accent.to_array());
    let oob_color = scene.color(tokens.form_error.to_array());
    let box_color = scene.color(tokens.grid_major.to_array());
    let grid_color = scene.color(tokens.grid.to_array());
    let margin_color = scene.color(tokens.text_dim.gamma_multiply(0.75).to_array());
    let ceiling_color = scene.color(tokens.accent.gamma_multiply(0.8).to_array());
    let axis_x = scene.color(tokens.axis_x.to_array());
    let axis_y = scene.color(tokens.axis_y.to_array());
    let axis_z = scene.color(tokens.axis_z.to_array());

    draw_free_floor_and_bed(
        &mut scene,
        job,
        box_color,
        grid_color,
        margin_color,
        ceiling_color,
        [axis_x, axis_y, axis_z],
    );

    let mut per_part: HashMap<PartId, InstancedMesh> = HashMap::new();
    for inst in &job.instances {
        if !job.is_instance_visible(inst) {
            continue;
        }
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
            edges: None,
        });

        let above_floor = job.is_instance_above_floor(inst.id);
        let in_nbz = job.is_instance_in_no_build_zone(inst.id);
        let valid = above_floor && !in_nbz;
        let is_selected = selected.contains(&inst.id);
        let color = if !valid {
            oob_color
        } else if is_selected {
            selected_color
        } else {
            part_color
        };
        entry.place(inst.transform, color);

        if is_selected {
            let [lo, hi] = inst.world_bounds(proto.mesh.bounds);
            draw_box_wireframe(&mut scene, lo, hi, if valid { selected_color } else { oob_color }, 1.6);
        }
    }

    scene.instanced = per_part.into_values().collect();

    if let Some(zone) = &job.no_build_zone
        && zone.visible
    {
        let zone_color = scene.color([tokens.form_error.r(), tokens.form_error.g(), tokens.form_error.b(), 110]);
        let tris = mesh_to_shared_tris(&zone.mesh);
        let mut nbz_mesh = InstancedMesh {
            key: u64::MAX,
            tris,
            instances: Vec::new(),
            look: InstanceLook::Faded,
            levels: Vec::new(),
            closed: true,
            edges: None,
        };
        nbz_mesh.place(zone.transform(), zone_color);
        scene.instanced.push(nbz_mesh);
        let [lo, hi] = zone.world_bounds();
        draw_box_wireframe(&mut scene, lo, hi, oob_color, 1.2);
    }

    scene
}

fn draw_box_wireframe(
    scene: &mut GpuScene,
    lo: [f32; 3],
    hi: [f32; 3],
    color: PaletteIndex,
    width: f32,
) {
    let [x0, y0, z0] = lo;
    let [x1, y1, z1] = hi;
    let edges = [
        ([x0, y0, z0], [x1, y0, z0]),
        ([x1, y0, z0], [x1, y1, z0]),
        ([x1, y1, z0], [x0, y1, z0]),
        ([x0, y1, z0], [x0, y0, z0]),
        ([x0, y0, z1], [x1, y0, z1]),
        ([x1, y0, z1], [x1, y1, z1]),
        ([x1, y1, z1], [x0, y1, z1]),
        ([x0, y1, z1], [x0, y0, z1]),
        ([x0, y0, z0], [x0, y0, z1]),
        ([x1, y0, z0], [x1, y0, z1]),
        ([x1, y1, z0], [x1, y1, z1]),
        ([x0, y1, z0], [x0, y1, z1]),
    ];
    for (a, b) in edges {
        scene.line(a, b, color, width, false);
    }
}

fn draw_rect_z(
    scene: &mut GpuScene,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    z: f32,
    color: PaletteIndex,
    width: f32,
) {
    scene.line([x0, y0, z], [x1, y0, z], color, width, false);
    scene.line([x1, y0, z], [x1, y1, z], color, width, false);
    scene.line([x1, y1, z], [x0, y1, z], color, width, false);
    scene.line([x0, y1, z], [x0, y0, z], color, width, false);
}

#[allow(clippy::too_many_arguments)]
fn draw_free_floor_and_bed(
    scene: &mut GpuScene,
    job: &BuildJob,
    major_grid_color: PaletteIndex,
    minor_grid_color: PaletteIndex,
    margin_color: PaletteIndex,
    bed_outline_color: PaletteIndex,
    axes: [PaletteIndex; 3],
) {
    let [w, d, h] = job.machine.size_mm();
    let ([x0, y0], [x1, y1]) = job.bed_xy_bounds();

    // Compute extended Z = 0 floor grid bounds covering the bed + all placed parts.
    let mut min_x = x0 - 300.0_f32;
    let mut min_y = y0 - 300.0_f32;
    let mut max_x = x1 + 300.0;
    let mut max_y = y1 + 300.0;
    for inst in &job.instances {
        if let Some(proto) = job.part(inst.part_id) {
            let wb = inst.world_bounds(proto.mesh.bounds);
            min_x = min_x.min(wb[0][0] - 200.0);
            min_y = min_y.min(wb[0][1] - 200.0);
            max_x = max_x.max(wb[1][0] + 200.0);
            max_y = max_y.max(wb[1][1] + 200.0);
        }
    }

    let snap_down = |v: f32| (v / 100.0).floor() * 100.0;
    let snap_up = |v: f32| (v / 100.0).ceil() * 100.0;
    let gx0 = snap_down(min_x).max(-5000.0);
    let gy0 = snap_down(min_y).max(-5000.0);
    let gx1 = snap_up(max_x).min(5000.0);
    let gy1 = snap_up(max_y).min(5000.0);

    // 20 mm minor floor grid & 100 mm major floor grid at Z = 0
    let span = (gx1 - gx0).max(gy1 - gy0);
    let minor_step = if span > 3000.0 { 100.0_f32 } else { 20.0_f32 };
    let mut x = gx0;
    while x <= gx1 + 0.5 {
        let is_major = (x / 100.0).round() * 100.0 - x;
        let (col, width) = if is_major.abs() < 0.1 {
            (major_grid_color, 1.05)
        } else {
            (minor_grid_color, 0.75)
        };
        scene.line([x, gy0, 0.0], [x, gy1, 0.0], col, width, false);
        x += minor_step;
    }
    let mut y = gy0;
    while y <= gy1 + 0.5 {
        let is_major = (y / 100.0).round() * 100.0 - y;
        let (col, width) = if is_major.abs() < 0.1 {
            (major_grid_color, 1.05)
        } else {
            (minor_grid_color, 0.75)
        };
        scene.line([gx0, y, 0.0], [gx1, y, 0.0], col, width, false);
        y += minor_step;
    }

    // Subtle reference bed footprint on Z = 0 and platform height envelope at Z = h
    draw_rect_z(scene, x0, y0, x1, y1, 0.05, bed_outline_color, 1.6);
    let min_z = job.min_z_mm.max(0.0);
    if min_z > 0.01 {
        draw_rect_z(scene, x0, y0, x1, y1, min_z, bed_outline_color, 1.2);
    }
    if h > 0.1 {
        draw_rect_z(scene, x0, y0, x1, y1, h, margin_color, 1.1);
        scene.line([x0, y0, 0.0], [x0, y0, h], margin_color, 1.0, false);
        scene.line([x1, y0, 0.0], [x1, y0, h], margin_color, 1.0, false);
        scene.line([x1, y1, 0.0], [x1, y1, h], margin_color, 1.0, false);
        scene.line([x0, y1, 0.0], [x0, y1, h], margin_color, 1.0, false);
    }

    // Wall-margin reference zone inside bed footprint at Z = min_z
    let m = job.wall_margin_mm.max(0.0);
    if m > 0.0 && w > 2.0 * m && d > 2.0 * m {
        draw_rect_z(scene, x0 + m, y0 + m, x1 - m, y1 - m, min_z + 0.05, margin_color, 1.1);
    }

    // Origin triad (25 mm)
    scene.line([0.0, 0.0, 0.0], [25.0, 0.0, 0.0], axes[0], 2.2, false);
    scene.line([0.0, 0.0, 0.0], [0.0, 25.0, 0.0], axes[1], 2.2, false);
    scene.line([0.0, 0.0, 0.0], [0.0, 0.0, 25.0], axes[2], 2.2, false);
}

/// Create a default isometric camera framed around `machine` and `origin`.
pub fn default_camera_with_origin(machine: &MachineProfile, origin: BedOrigin) -> Camera {
    let [w, d, h] = machine.size_mm();
    let diag = (w * w + d * d + h * h).sqrt();
    let (yaw, pitch) = StandardView::Iso.angles();
    let (tx, ty) = match origin {
        BedOrigin::FrontLeft => (f64::from(w) * 0.5, f64::from(d) * 0.5),
        BedOrigin::Center => (0.0, 0.0),
    };
    Camera {
        target: [tx, ty, f64::from(h) * 0.35].into(),
        yaw,
        pitch,
        roll: 0.0,
        distance: f64::from(diag) * 1.15,
        fov: 0.65,
        up: UpAxis::Z,
    }
}

/// Create a default isometric camera framed around `machine` (front-left origin).
#[cfg(test)]
pub fn default_camera(machine: &MachineProfile) -> Camera {
    default_camera_with_origin(machine, BedOrigin::FrontLeft)
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

/// Intersect a screen ray with the camera-facing plane passing through `anchor`.
pub fn intersect_camera_plane(
    cam: &Camera,
    rect: egui::Rect,
    pointer: egui::Pos2,
    anchor: [f32; 3],
) -> Option<[f32; 3]> {
    let (origin, dir) = screen_ray(cam, rect, pointer)?;
    let n = cam.back().to_f32();
    let denom = dir[0] * n[0] + dir[1] * n[1] + dir[2] * n[2];
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = ((anchor[0] - origin[0]) * n[0]
        + (anchor[1] - origin[1]) * n[1]
        + (anchor[2] - origin[2]) * n[2])
        / denom;
    if t < 0.0 || !t.is_finite() {
        return None;
    }
    Some([origin[0] + dir[0] * t, origin[1] + dir[1] * t, origin[2] + dir[2] * t])
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
        if !job.is_instance_visible(inst) {
            continue;
        }
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

/// Return `true` when the screen-space pointer ray hits the visible [`NoBuildZone`] mesh.
pub fn pick_no_build_zone(
    job: &BuildJob,
    cam: &Camera,
    rect: egui::Rect,
    pointer: egui::Pos2,
) -> bool {
    let Some(zone) = &job.no_build_zone else {
        return false;
    };
    if !zone.visible {
        return false;
    }
    let Some((ray_origin, ray_dir)) = screen_ray(cam, rect, pointer) else {
        return false;
    };
    if ray_aabb_intersect(ray_origin, ray_dir, zone.world_bounds()).is_none() {
        return false;
    }
    let (local_origin, local_dir) = inverse_transform_ray(&zone.transform(), ray_origin, ray_dir);
    for tri in &zone.mesh.indices {
        let a = zone.mesh.positions[tri[0] as usize];
        let b = zone.mesh.positions[tri[1] as usize];
        let c = zone.mesh.positions[tri[2] as usize];
        if ray_triangle_intersect(local_origin, local_dir, a, b, c).is_some() {
            return true;
        }
    }
    false
}

/// Return all visible instances whose projected world AABB intersects `sel_rect`.
pub fn instances_in_screen_rect(
    job: &BuildJob,
    cam: &Camera,
    rect: egui::Rect,
    sel_rect: egui::Rect,
) -> Vec<InstanceId> {
    let w = f64::from(rect.width().max(1.0));
    let h = f64::from(rect.height().max(1.0));
    let [mw, md, mh] = job.machine.size_mm();
    let radius = f64::from((mw * mw + md * md + mh * mh).sqrt()) * 2.0;

    let mut hits = Vec::new();
    for inst in &job.instances {
        if !job.is_instance_visible(inst) {
            continue;
        }
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let [lo, hi] = inst.world_bounds(proto.mesh.bounds);
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        let mut any_projected = false;
        for sx in [lo[0], hi[0]] {
            for sy in [lo[1], hi[1]] {
                for sz in [lo[2], hi[2]] {
                    let p = [f64::from(sx), f64::from(sy), f64::from(sz)].into();
                    if let Some((px, py, _)) = cam.to_screen(p, w, h, radius) {
                        let x = rect.left() + px as f32;
                        let y = rect.top() + py as f32;
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x);
                        max_y = max_y.max(y);
                        any_projected = true;
                    }
                }
            }
        }
        if any_projected {
            let proj_rect = egui::Rect::from_min_max(egui::pos2(min_x, min_y), egui::pos2(max_x, max_y));
            if proj_rect.intersects(sel_rect) {
                hits.push(inst.id);
            }
        }
    }
    hits
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
    use nestcraft_core::model::IDENTITY_TRANSFORM;
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
        let selected = HashSet::from([i1]);
        let scene = build_gpu_scene(&job, &selected, &Tokens::get(), &mut cache);
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
        cam.target = [100.0, 125.0, 10.0].into();
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
            target: [108.0, 133.0, 10.0].into(),
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

    #[test]
    fn box_select_finds_visible_instance_in_screen_rect() {
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

        let cam = default_camera(&job.machine);
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        assert_eq!(instances_in_screen_rect(&job, &cam, rect, rect), vec![iid]);
        let empty = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(5.0, 5.0));
        assert!(instances_in_screen_rect(&job, &cam, rect, empty).is_empty());
    }
}
