//! 3D viewport scene builder, hardware instancing, camera controls, and ray picking.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nestcraft_core::model::{
    Aabb, BedOrigin, BuildJob, InstanceId, MachineProfile, PartId, Transform3x4, TriMesh, transform_point,
};
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
        // Vertices are emitted unshared with exact precomputed face normals; keep `flat: false`
        // so the shader uses those normals directly instead of noisy `dpdx`/`dpdy` derivatives.
        flat: false,
        bounds: mesh.bounds,
    })
}

/// Cached GPU triangle meshes for parts and the optional no-build zone.
#[derive(Default)]
pub struct SceneCache {
    pub parts: HashMap<PartId, Arc<SharedTris>>,
    pub no_build_zone: Option<(Arc<TriMesh>, Arc<SharedTris>)>,
    pub thermal_colors: Option<HashMap<InstanceId, [u8; 4]>>,
    pub collision_ids: HashSet<InstanceId>,
    pub max_z_limit_mm: Option<f32>,
}

impl SceneCache {
    fn part_tris(&mut self, part_id: PartId, mesh: &TriMesh) -> Arc<SharedTris> {
        self.parts
            .entry(part_id)
            .or_insert_with(|| mesh_to_shared_tris(mesh))
            .clone()
    }

    fn no_build_zone_tris(&mut self, mesh: &Arc<TriMesh>) -> Arc<SharedTris> {
        if let Some((cached_mesh, tris)) = &self.no_build_zone
            && Arc::ptr_eq(cached_mesh, mesh)
        {
            return tris.clone();
        }
        let tris = mesh_to_shared_tris(mesh);
        self.no_build_zone = Some((mesh.clone(), tris.clone()));
        tris
    }
}

enum IdLookup<'a> {
    Slice(&'a [InstanceId]),
    Set(HashSet<InstanceId>),
}

impl<'a> IdLookup<'a> {
    fn from_slice(ids: &'a [InstanceId]) -> Self {
        if ids.len() > 8 {
            Self::Set(ids.iter().copied().collect())
        } else {
            Self::Slice(ids)
        }
    }

    fn contains(&self, id: InstanceId) -> bool {
        match self {
            Self::Slice(ids) => ids.contains(&id),
            Self::Set(set) => set.contains(&id),
        }
    }
}

/// Item(s) currently being dragged in a separate [`solidcraft_viewport_gpu::view::MovingLayer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragExclude {
    Instances(Vec<InstanceId>),
    NoBuildZone,
}

#[allow(clippy::too_many_arguments)]
fn instance_palette_color(
    scene: &mut GpuScene,
    custom_rgb: Option<[u8; 3]>,
    valid: bool,
    is_selected: bool,
    transparent: bool,
    part_color: PaletteIndex,
    part_faded: PaletteIndex,
    selected_color: PaletteIndex,
    selected_faded: PaletteIndex,
    oob_color: PaletteIndex,
    oob_faded: PaletteIndex,
) -> PaletteIndex {
    if !valid {
        return if transparent { oob_faded } else { oob_color };
    }
    if let Some([r, g, b]) = custom_rgb {
        let alpha = if transparent { 128 } else { 255 };
        return scene.color([r, g, b, alpha]);
    }
    match (is_selected, transparent) {
        (true, true) => selected_faded,
        (true, false) => selected_color,
        (false, true) => part_faded,
        (false, false) => part_color,
    }
}

/// Build the [`GpuScene`] for `job`, including build-chamber wireframe, floor grid, and instanced parts.
#[cfg(test)]
pub fn build_gpu_scene(
    job: &BuildJob,
    selected: &HashSet<InstanceId>,
    tokens: &Tokens,
    cache: &mut SceneCache,
) -> GpuScene {
    build_gpu_scene_with_exclude(job, selected, tokens, cache, None)
}

/// Build the static [`GpuScene`] for `job`, optionally excluding an item rendered in `MovingLayer`.
pub fn build_gpu_scene_with_exclude(
    job: &BuildJob,
    selected: &HashSet<InstanceId>,
    tokens: &Tokens,
    cache: &mut SceneCache,
    exclude: Option<&DragExclude>,
) -> GpuScene {
    let mut scene = GpuScene::with_palette(Palette::default());

    let part_color = scene.color(tokens.text.to_array());
    let selected_color = scene.color(tokens.accent.to_array());
    let oob_color = scene.color(tokens.form_error.to_array());
    let part_faded = scene.color([tokens.text.r(), tokens.text.g(), tokens.text.b(), 128]);
    let selected_faded = scene.color([tokens.accent.r(), tokens.accent.g(), tokens.accent.b(), 128]);
    let oob_faded = scene.color([tokens.form_error.r(), tokens.form_error.g(), tokens.form_error.b(), 128]);
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
        cache.max_z_limit_mm,
    );

    let exclude_lookup = match exclude {
        Some(DragExclude::Instances(ids)) => Some(IdLookup::from_slice(ids)),
        _ => None,
    };
    let mut per_part: HashMap<(PartId, bool), InstancedMesh> = HashMap::new();
    for inst in &job.instances {
        if exclude_lookup.as_ref().is_some_and(|l| l.contains(inst.id)) || !job.is_instance_visible(inst) {
            continue;
        }
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let tris = cache.part_tris(proto.id, &proto.mesh);
        let entry = per_part.entry((proto.id, inst.transparent)).or_insert_with(|| InstancedMesh {
            key: u64::from(proto.id.0),
            tris,
            instances: Vec::new(),
            look: if inst.transparent {
                InstanceLook::Faded
            } else {
                InstanceLook::Opaque
            },
            levels: Vec::new(),
            closed: true,
            edges: None,
        });

        let wb = inst.world_bounds(proto.mesh.bounds);
        let above_floor = job.world_bounds_above_floor(wb);
        let in_nbz = job.instance_in_no_build_zone(inst);
        let in_collision = cache.collision_ids.contains(&inst.id);
        let valid = above_floor && !in_nbz && !in_collision;
        let is_selected = selected.contains(&inst.id);
        let color = if let Some(t_rgba) = cache.thermal_colors.as_ref().and_then(|m| m.get(&inst.id)) {
            let mut rgba = *t_rgba;
            if inst.transparent {
                rgba[3] = 128;
            }
            scene.color(rgba)
        } else {
            instance_palette_color(
                &mut scene,
                inst.color,
                valid,
                is_selected,
                inst.transparent,
                part_color,
                part_faded,
                selected_color,
                selected_faded,
                oob_color,
                oob_faded,
            )
        };
        entry.place(inst.transform, color);

        if is_selected {
            draw_box_wireframe(&mut scene, wb[0], wb[1], if valid { selected_color } else { oob_color }, 1.6);
        }
    }

    scene.instanced = per_part.into_values().collect();

    if exclude != Some(&DragExclude::NoBuildZone)
        && let Some(zone) = &job.no_build_zone
        && zone.visible
    {
        let zone_color = scene.color([tokens.form_error.r(), tokens.form_error.g(), tokens.form_error.b(), 110]);
        let tris = cache.no_build_zone_tris(&zone.mesh);
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

/// Build a [`GpuScene`] containing only `target` at its current transform for [`solidcraft_viewport_gpu::view::MovingLayer`].
pub fn build_moving_scene(
    job: &BuildJob,
    selected: &HashSet<InstanceId>,
    tokens: &Tokens,
    cache: &mut SceneCache,
    target: &DragExclude,
) -> GpuScene {
    let mut scene = GpuScene::with_palette(Palette::default());

    let part_color = scene.color(tokens.text.to_array());
    let selected_color = scene.color(tokens.accent.to_array());
    let oob_color = scene.color(tokens.form_error.to_array());
    let part_faded = scene.color([tokens.text.r(), tokens.text.g(), tokens.text.b(), 128]);
    let selected_faded = scene.color([tokens.accent.r(), tokens.accent.g(), tokens.accent.b(), 128]);
    let oob_faded = scene.color([tokens.form_error.r(), tokens.form_error.g(), tokens.form_error.b(), 128]);

    match target {
        DragExclude::Instances(ids) => {
            let include_lookup = IdLookup::from_slice(ids);
            let mut per_part: HashMap<(PartId, bool), InstancedMesh> = HashMap::new();
            for inst in &job.instances {
                if !include_lookup.contains(inst.id) || !job.is_instance_visible(inst) {
                    continue;
                }
                let Some(proto) = job.part(inst.part_id) else {
                    continue;
                };
                let tris = cache.part_tris(proto.id, &proto.mesh);
                let entry = per_part.entry((proto.id, inst.transparent)).or_insert_with(|| InstancedMesh {
                    key: u64::from(proto.id.0),
                    tris,
                    instances: Vec::new(),
                    look: if inst.transparent {
                        InstanceLook::Faded
                    } else {
                        InstanceLook::Opaque
                    },
                    levels: Vec::new(),
                    closed: true,
                    edges: None,
                });
                let wb = inst.world_bounds(proto.mesh.bounds);
                let valid = job.world_bounds_above_floor(wb) && !job.instance_in_no_build_zone(inst);
                let is_selected = selected.contains(&inst.id);
                let color = instance_palette_color(
                    &mut scene,
                    inst.color,
                    valid,
                    is_selected,
                    inst.transparent,
                    part_color,
                    part_faded,
                    selected_color,
                    selected_faded,
                    oob_color,
                    oob_faded,
                );
                entry.place(inst.transform, color);
                if is_selected {
                    draw_box_wireframe(&mut scene, wb[0], wb[1], if valid { selected_color } else { oob_color }, 1.6);
                }
            }
            scene.instanced = per_part.into_values().collect();
        }
        DragExclude::NoBuildZone => {
            if let Some(zone) = &job.no_build_zone
                && zone.visible
            {
                let zone_color = scene.color([tokens.form_error.r(), tokens.form_error.g(), tokens.form_error.b(), 110]);
                let tris = cache.no_build_zone_tris(&zone.mesh);
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
        }
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

fn draw_dashed_rect_z(
    scene: &mut GpuScene,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    z: f32,
    color: PaletteIndex,
    width: f32,
) {
    let draw_dashed_edge = |scene: &mut GpuScene, a: [f32; 3], b: [f32; 3]| {
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let dz = b[2] - a[2];
        let len = (dx * dx + dy * dy + dz * dz).sqrt();
        if len < 1.0 {
            return;
        }
        let dash = 8.0_f32;
        let step = 14.0_f32;
        let mut t = 0.0_f32;
        while t < len {
            let t0 = t / len;
            let t1 = ((t + dash).min(len)) / len;
            scene.line(
                [a[0] + dx * t0, a[1] + dy * t0, a[2] + dz * t0],
                [a[0] + dx * t1, a[1] + dy * t1, a[2] + dz * t1],
                color,
                width,
                false,
            );
            t += step;
        }
    };
    draw_dashed_edge(scene, [x0, y0, z], [x1, y0, z]);
    draw_dashed_edge(scene, [x1, y0, z], [x1, y1, z]);
    draw_dashed_edge(scene, [x1, y1, z], [x0, y1, z]);
    draw_dashed_edge(scene, [x0, y1, z], [x0, y0, z]);
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
    max_z_limit_mm: Option<f32>,
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

    if let Some(z_lim) = max_z_limit_mm {
        let z_clamped = z_lim.clamp(min_z + 1.0, h.max(min_z + 1.0));
        let cx0 = if w > 2.0 * m { x0 + m } else { x0 };
        let cx1 = if w > 2.0 * m { x1 - m } else { x1 };
        let cy0 = if d > 2.0 * m { y0 + m } else { y0 };
        let cy1 = if d > 2.0 * m { y1 - m } else { y1 };
        draw_dashed_rect_z(scene, cx0, cy0, cx1, cy1, z_clamped, bed_outline_color, 1.6);
    }

    // Origin triad (25 mm)
    scene.line([0.0, 0.0, 0.0], [25.0, 0.0, 0.0], axes[0], 2.2, false);
    scene.line([0.0, 0.0, 0.0], [0.0, 25.0, 0.0], axes[1], 2.2, false);
    scene.line([0.0, 0.0, 0.0], [0.0, 0.0, 25.0], axes[2], 2.2, false);
}

/// Create a default isometric camera framed around `machine` and `origin` (+X right, +Y far).
pub fn default_camera_with_origin(machine: &MachineProfile, origin: BedOrigin) -> Camera {
    let [w, d, h] = machine.size_mm();
    let diag = (w * w + d * d + h * h).sqrt();
    let (_, pitch) = StandardView::Iso.angles();
    let yaw = std::f64::consts::FRAC_PI_4;
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

/// Result of picking a face (triangle + coplanar face) on a part instance.
#[derive(Debug, Clone)]
pub struct FacePick {
    pub instance_id: InstanceId,
    pub tri_index: usize,
    pub local_normal: [f32; 3],
    pub world_tris: Vec<[[f32; 3]; 3]>,
    pub world_normal: [f32; 3],
}

/// Pick the closest triangle under `pointer` (optionally restricted to `filter_instance`)
/// and collect coplanar triangles on that face for highlighting.
pub fn pick_face(
    job: &BuildJob,
    cam: &Camera,
    rect: egui::Rect,
    pointer: egui::Pos2,
    filter_instance: Option<InstanceId>,
) -> Option<FacePick> {
    let (ray_origin, ray_dir) = screen_ray(cam, rect, pointer)?;
    let mut best: Option<(f32, InstanceId, usize)> = None;

    for inst in &job.instances {
        if filter_instance.is_some_and(|id| id != inst.id) || !job.is_instance_visible(inst) {
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
        for (tri_i, tri) in proto.mesh.indices.iter().enumerate() {
            let a = proto.mesh.positions[tri[0] as usize];
            let b = proto.mesh.positions[tri[1] as usize];
            let c = proto.mesh.positions[tri[2] as usize];
            if let Some(t) = ray_triangle_intersect(local_origin, local_dir, a, b, c)
                && best.is_none_or(|(best_t, _, _)| t < best_t)
            {
                best = Some((t, inst.id, tri_i));
            }
        }
    }

    let (_, inst_id, hit_tri) = best?;
    let inst = job.instances.iter().find(|i| i.id == inst_id)?;
    let proto = job.part(inst.part_id)?;
    let local_n = proto.mesh.tri_normals.get(hit_tri).copied().unwrap_or([0.0, 0.0, 1.0]);
    let hit_v0 = proto.mesh.positions[proto.mesh.indices[hit_tri][0] as usize];
    let hit_d = local_n[0] * hit_v0[0] + local_n[1] * hit_v0[1] + local_n[2] * hit_v0[2];

    let mut world_tris = Vec::new();
    for (tri_i, tri) in proto.mesh.indices.iter().enumerate() {
        let n = proto.mesh.tri_normals.get(tri_i).copied().unwrap_or([0.0, 0.0, 1.0]);
        let dot = n[0] * local_n[0] + n[1] * local_n[1] + n[2] * local_n[2];
        if dot < 0.998 {
            continue;
        }
        let v0 = proto.mesh.positions[tri[0] as usize];
        let d = local_n[0] * v0[0] + local_n[1] * v0[1] + local_n[2] * v0[2];
        if (d - hit_d).abs() > 0.05 {
            continue;
        }
        world_tris.push([
            transform_point(&inst.transform, v0),
            transform_point(&inst.transform, proto.mesh.positions[tri[1] as usize]),
            transform_point(&inst.transform, proto.mesh.positions[tri[2] as usize]),
        ]);
    }
    if world_tris.is_empty() {
        let tri = proto.mesh.indices[hit_tri];
        world_tris.push([
            transform_point(&inst.transform, proto.mesh.positions[tri[0] as usize]),
            transform_point(&inst.transform, proto.mesh.positions[tri[1] as usize]),
            transform_point(&inst.transform, proto.mesh.positions[tri[2] as usize]),
        ]);
    }
    let m = &inst.transform;
    let wn = [
        m[0][0] * local_n[0] + m[0][1] * local_n[1] + m[0][2] * local_n[2],
        m[1][0] * local_n[0] + m[1][1] * local_n[1] + m[1][2] * local_n[2],
        m[2][0] * local_n[0] + m[2][1] * local_n[1] + m[2][2] * local_n[2],
    ];
    Some(FacePick {
        instance_id: inst_id,
        tri_index: hit_tri,
        local_normal: local_n,
        world_tris,
        world_normal: wn,
    })
}

/// Build a highlight [`GpuScene`] for `pick`'s coplanar triangles.
pub fn build_face_highlight_scene(pick: &FacePick, tokens: &Tokens) -> GpuScene {
    let mut scene = GpuScene::with_palette(Palette::default());
    let col = scene.color([tokens.accent.r(), tokens.accent.g(), tokens.accent.b(), 180]);
    for &[a, b, c] in &pick.world_tris {
        scene.hover_face_tri(a, pick.world_normal, col);
        scene.hover_face_tri(b, pick.world_normal, col);
        scene.hover_face_tri(c, pick.world_normal, col);
    }
    scene
}

/// Build a depth-tested yellow [`GpuScene`] highlight showing exact collision volumes, intersection seams, and contacts.
pub fn build_collision_highlight_scene(rep: &nestcraft_core::collision::CollisionReport) -> GpuScene {
    let mut scene = GpuScene::with_palette(Palette::default());
    if rep.colliding_ids.is_empty()
        && rep.overlap_voxels.is_empty()
        && rep.overlap_quads.is_empty()
        && rep.intersection_segments.is_empty()
        && rep.contacts.is_empty()
    {
        return scene;
    }

    let hard_fill = scene.color([255, 220, 0, 210]);
    let clear_fill = scene.color([255, 160, 0, 175]);
    for q in &rep.overlap_quads {
        let col = if q.is_hard { hard_fill } else { clear_fill };
        let [v0, v1, v2, v3] = q.verts;
        for v in [v0, v1, v2, v0, v2, v3] {
            scene.trans_tri(v, q.normal, col);
        }
    }

    let seam_col = scene.color([255, 250, 110, 255]);
    for &(p0, p1) in &rep.intersection_segments {
        scene.lifted_line(p0, p1, seam_col, 3.0, [[0.0; 3]; 2]);
    }

    let contact_hard = scene.color([255, 245, 60, 255]);
    let contact_clear = scene.color([255, 180, 0, 255]);
    let draw_octa = rep.overlap_quads.is_empty() && rep.intersection_segments.is_empty();
    for c in &rep.contacts {
        let col = if c.is_hard { contact_hard } else { contact_clear };
        let dx = c.point_b[0] - c.point_a[0];
        let dy = c.point_b[1] - c.point_a[1];
        let dz = c.point_b[2] - c.point_a[2];
        if dx * dx + dy * dy + dz * dz > 1e-6 {
            scene.lifted_line(c.point_a, c.point_b, col, 3.2, [[0.0; 3]; 2]);
        }
        let mid = [
            (c.point_a[0] + c.point_b[0]) * 0.5,
            (c.point_a[1] + c.point_b[1]) * 0.5,
            (c.point_a[2] + c.point_b[2]) * 0.5,
        ];
        const ARM: f32 = 2.0;
        scene.lifted_line([mid[0] - ARM, mid[1], mid[2]], [mid[0] + ARM, mid[1], mid[2]], col, 2.4, [[0.0; 3]; 2]);
        scene.lifted_line([mid[0], mid[1] - ARM, mid[2]], [mid[0], mid[1] + ARM, mid[2]], col, 2.4, [[0.0; 3]; 2]);
        scene.lifted_line([mid[0], mid[1], mid[2] - ARM], [mid[0], mid[1], mid[2] + ARM], col, 2.4, [[0.0; 3]; 2]);

        if draw_octa {
            let fill = if c.is_hard { hard_fill } else { clear_fill };
            const R: f32 = 1.5;
            let px = [mid[0] + R, mid[1], mid[2]];
            let nx = [mid[0] - R, mid[1], mid[2]];
            let py = [mid[0], mid[1] + R, mid[2]];
            let ny = [mid[0], mid[1] - R, mid[2]];
            let pz = [mid[0], mid[1], mid[2] + R];
            let nz = [mid[0], mid[1], mid[2] - R];
            let tris = [
                (px, py, pz),
                (py, nx, pz),
                (nx, ny, pz),
                (ny, px, pz),
                (py, px, nz),
                (nx, py, nz),
                (ny, nx, nz),
                (px, ny, nz),
            ];
            for (a, b, d) in tris {
                let n = [0.0, 0.0, 1.0];
                scene.trans_tri(a, n, fill);
                scene.trans_tri(b, n, fill);
                scene.trans_tri(d, n, fill);
            }
        }
    }

    scene
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

/// Projected screen-space centerpoint handle for a visible selected instance.
#[derive(Debug, Clone, Copy)]
pub struct CenterpointHandle {
    pub id: InstanceId,
    pub screen_pos: egui::Pos2,
    pub world_pos: [f32; 3],
    pub valid: bool,
}

/// Project the world-space bounding-box centers of visible selected instances to viewport coordinates.
pub fn selected_centerpoints(
    job: &BuildJob,
    selected: &HashSet<InstanceId>,
    cam: &Camera,
    rect: egui::Rect,
) -> Vec<CenterpointHandle> {
    if selected.is_empty() {
        return Vec::new();
    }
    let w = f64::from(rect.width().max(1.0));
    let h = f64::from(rect.height().max(1.0));
    let [mw, md, mh] = job.machine.size_mm();
    let radius = f64::from((mw * mw + md * md + mh * mh).sqrt()) * 2.0;

    let mut centers = Vec::new();
    for inst in &job.instances {
        if !selected.contains(&inst.id) || !job.is_instance_visible(inst) {
            continue;
        }
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let wb = inst.world_bounds(proto.mesh.bounds);
        let world_pos = [
            (wb[0][0] + wb[1][0]) * 0.5,
            (wb[0][1] + wb[1][1]) * 0.5,
            (wb[0][2] + wb[1][2]) * 0.5,
        ];
        if let Some((px, py, _)) = cam.to_screen(
            [f64::from(world_pos[0]), f64::from(world_pos[1]), f64::from(world_pos[2])].into(),
            w,
            h,
            radius,
        ) {
            let screen_pos = egui::pos2(rect.left() + px as f32, rect.top() + py as f32);
            if rect.contains(screen_pos) {
                let valid = job.world_bounds_above_floor(wb) && !job.instance_in_no_build_zone(inst);
                centers.push(CenterpointHandle {
                    id: inst.id,
                    screen_pos,
                    world_pos,
                    valid,
                });
            }
        }
    }
    centers
}

/// Project the top-left-front corner of the combined bounding box of visible selected instances
/// for the free-rotation handle, returning `(screen_pos, combined_center_pivot)`.
pub fn selected_rotation_handle(
    job: &BuildJob,
    selected: &HashSet<InstanceId>,
    cam: &Camera,
    rect: egui::Rect,
) -> Option<(egui::Pos2, [f32; 3])> {
    if selected.is_empty() {
        return None;
    }
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    let mut any = false;
    for inst in &job.instances {
        if !selected.contains(&inst.id) || !job.is_instance_visible(inst) {
            continue;
        }
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let wb = inst.world_bounds(proto.mesh.bounds);
        for k in 0..3 {
            lo[k] = lo[k].min(wb[0][k]);
            hi[k] = hi[k].max(wb[1][k]);
        }
        any = true;
    }
    if !any {
        return None;
    }
    let corner = [lo[0], lo[1], hi[2]];
    let pivot = [
        (lo[0] + hi[0]) * 0.5,
        (lo[1] + hi[1]) * 0.5,
        (lo[2] + hi[2]) * 0.5,
    ];
    let w = f64::from(rect.width().max(1.0));
    let h = f64::from(rect.height().max(1.0));
    let [mw, md, mh] = job.machine.size_mm();
    let radius = f64::from((mw * mw + md * md + mh * mh).sqrt()) * 2.0;
    let (px, py, _) = cam.to_screen(
        [f64::from(corner[0]), f64::from(corner[1]), f64::from(corner[2])].into(),
        w,
        h,
        radius,
    )?;
    let screen_pos = egui::pos2(rect.left() + px as f32, rect.top() + py as f32);
    rect.contains(screen_pos).then_some((screen_pos, pivot))
}

/// Projected 3D world-axis ring for the interactive rotation gizmo.
#[derive(Debug, Clone)]
pub struct RotationGizmoRing {
    pub axis: usize,
    pub screen_points: Vec<egui::Pos2>,
    pub axis_tip: Option<egui::Pos2>,
}

/// Projected world-axis rotation gizmo centered on the combined bounds of visible selected instances.
#[derive(Debug, Clone)]
pub struct RotationGizmo {
    pub pivot: [f32; 3],
    pub pivot_screen: egui::Pos2,
    pub rings: Vec<RotationGizmoRing>,
}

/// Project 3D world-axis rotation rings (`X`, `Y`, `Z`, or `Z` only when `z_only` is true)
/// centered on the combined bounding box of visible selected instances.
pub fn project_rotation_gizmo(
    job: &BuildJob,
    selected: &HashSet<InstanceId>,
    cam: &Camera,
    rect: egui::Rect,
    z_only: bool,
) -> Option<RotationGizmo> {
    if selected.is_empty() {
        return None;
    }
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    let mut any = false;
    for inst in &job.instances {
        if !selected.contains(&inst.id) || !job.is_instance_visible(inst) {
            continue;
        }
        let Some(proto) = job.part(inst.part_id) else {
            continue;
        };
        let wb = inst.world_bounds(proto.mesh.bounds);
        for k in 0..3 {
            lo[k] = lo[k].min(wb[0][k]);
            hi[k] = hi[k].max(wb[1][k]);
        }
        any = true;
    }
    if !any {
        return None;
    }
    let pivot = [
        (lo[0] + hi[0]) * 0.5,
        (lo[1] + hi[1]) * 0.5,
        (lo[2] + hi[2]) * 0.5,
    ];
    let diag = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2)).sqrt();
    let gizmo_r = (diag * 0.65).clamp(18.0, 220.0);

    let w = f64::from(rect.width().max(1.0));
    let h = f64::from(rect.height().max(1.0));
    let [mw, md, mh] = job.machine.size_mm();
    let radius = f64::from((mw * mw + md * md + mh * mh).sqrt()) * 2.0;

    let project_pt = |pt: [f32; 3]| -> Option<egui::Pos2> {
        let (px, py, _) = cam.to_screen(
            [f64::from(pt[0]), f64::from(pt[1]), f64::from(pt[2])].into(),
            w,
            h,
            radius,
        )?;
        Some(egui::pos2(rect.left() + px as f32, rect.top() + py as f32))
    };

    let pivot_screen = project_pt(pivot)?;
    let axes: &[(usize, [f32; 3], [f32; 3], [f32; 3])] = if z_only {
        &[(2, [0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0])]
    } else {
        &[
            (0, [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            (1, [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            (2, [0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ]
    };

    const SEGMENTS: usize = 48;
    let mut rings = Vec::with_capacity(axes.len());
    for &(axis, dir, u, v) in axes {
        let mut screen_points = Vec::with_capacity(SEGMENTS + 1);
        for i in 0..=SEGMENTS {
            let a = (i as f32 / SEGMENTS as f32) * std::f32::consts::TAU;
            let (s, c) = a.sin_cos();
            let pt = [
                pivot[0] + gizmo_r * (c * u[0] + s * v[0]),
                pivot[1] + gizmo_r * (c * u[1] + s * v[1]),
                pivot[2] + gizmo_r * (c * u[2] + s * v[2]),
            ];
            if let Some(sp) = project_pt(pt) {
                screen_points.push(sp);
            }
        }
        let tip = [
            pivot[0] + dir[0] * gizmo_r * 1.18,
            pivot[1] + dir[1] * gizmo_r * 1.18,
            pivot[2] + dir[2] * gizmo_r * 1.18,
        ];
        rings.push(RotationGizmoRing {
            axis,
            screen_points,
            axis_tip: project_pt(tip),
        });
    }

    Some(RotationGizmo {
        pivot,
        pivot_screen,
        rings,
    })
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

        let mut cache = SceneCache::default();
        let selected = HashSet::from([i1]);
        let scene = build_gpu_scene(&job, &selected, &Tokens::get(), &mut cache);
        assert_eq!(scene.instanced.len(), 1);
        assert_eq!(scene.instanced[0].instances.len(), 4 * INSTANCE_SIZE);
        assert!(!scene.lines.is_empty());
    }

    #[test]
    fn drag_splits_moving_instance_and_caches_no_build_zone_tris() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
                vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
            )
            .expect("valid mesh"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("bracket", mesh.clone());
        let i1 = job.add_instance(pid, IDENTITY_TRANSFORM).expect("i1");
        let i2 = job.add_instance(pid, IDENTITY_TRANSFORM).expect("i2");
        job.no_build_zone = Some(nestcraft_core::model::NoBuildZone::new("nbz", mesh));

        let mut cache = SceneCache::default();
        let selected = HashSet::from([i1]);
        let tokens = Tokens::get();
        let exclude = DragExclude::Instances(vec![i1]);
        let static_scene = build_gpu_scene_with_exclude(
            &job,
            &selected,
            &tokens,
            &mut cache,
            Some(&exclude),
        );
        let moving_scene = build_moving_scene(&job, &selected, &tokens, &mut cache, &exclude);
        assert_eq!(static_scene.instanced.len(), 2); // i2 + no_build_zone
        assert_eq!(static_scene.instanced[0].instances.len(), INSTANCE_SIZE);
        assert_eq!(moving_scene.instanced.len(), 1);
        assert_eq!(moving_scene.instanced[0].instances.len(), INSTANCE_SIZE);
        assert!(Arc::ptr_eq(&static_scene.instanced[0].tris, &moving_scene.instanced[0].tris));

        let second_static = build_gpu_scene(&job, &selected, &tokens, &mut cache);
        let nbz_a = static_scene.instanced.iter().find(|m| m.key == u64::MAX).expect("nbz a");
        let nbz_b = second_static.instanced.iter().find(|m| m.key == u64::MAX).expect("nbz b");
        assert!(Arc::ptr_eq(&nbz_a.tris, &nbz_b.tris));
        let _ = i2;
    }

    #[test]
    fn transparent_instance_uses_faded_look_separate_from_opaque_siblings() {
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
        let i2 = job.add_instance(pid, IDENTITY_TRANSFORM).expect("i2");
        assert!(job.set_instance_transparent(i2, true));

        let mut cache = SceneCache::default();
        let scene = build_gpu_scene(&job, &HashSet::from([i1]), &Tokens::get(), &mut cache);
        assert_eq!(scene.instanced.len(), 2);
        assert!(scene.instanced.iter().any(|m| m.look == InstanceLook::Opaque));
        assert!(scene.instanced.iter().any(|m| m.look == InstanceLook::Faded));
        assert!(Arc::ptr_eq(&scene.instanced[0].tris, &scene.instanced[1].tris));
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

        let selected = HashSet::from([iid]);
        let centers = selected_centerpoints(&job, &selected, &cam, rect);
        assert_eq!(centers.len(), 1);
        assert!(centers[0].valid);
        assert!(rect.contains(centers[0].screen_pos));
        assert!(selected_rotation_handle(&job, &selected, &cam, rect).is_some());

        assert!(selected_centerpoints(&job, &HashSet::new(), &cam, rect).is_empty());
        assert!(selected_rotation_handle(&job, &HashSet::new(), &cam, rect).is_none());
    }

    #[test]
    fn collision_highlight_uses_depth_tested_yellow_geometry_without_xray() {
        let cube = Arc::new(
            TriMesh::from_indexed(
                vec![
                    [0.0, 0.0, 0.0],
                    [20.0, 0.0, 0.0],
                    [20.0, 20.0, 0.0],
                    [0.0, 20.0, 0.0],
                    [0.0, 0.0, 20.0],
                    [20.0, 0.0, 20.0],
                    [20.0, 20.0, 20.0],
                    [0.0, 20.0, 20.0],
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
            .expect("valid cube"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("cube", cube);
        let _a = job.add_instance(pid, nestcraft_core::model::IDENTITY_TRANSFORM).expect("a");
        let _b = job
            .add_instance(pid, [[1.0, 0.0, 0.0, 10.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]])
            .expect("b");

        let rep = nestcraft_core::collision::detect_collisions(&job, None, 3.0);
        let hl = build_collision_highlight_scene(&rep);
        assert!(hl.xray.is_empty(), "collision highlight should not use xray");
        assert!(hl.overlay.is_empty(), "collision highlight should not draw un-occluded overlay lines");
        assert!(!hl.trans.is_empty(), "expected depth-tested translucent yellow overlap quads");
        assert!(!hl.lines.is_empty(), "expected depth-tested seam/contact lines");

        let mut cache = SceneCache {
            collision_ids: rep.colliding_ids.clone(),
            ..Default::default()
        };
        let scene = build_gpu_scene_with_exclude(&job, &HashSet::new(), &Tokens::get(), &mut cache, None);
        assert!(
            scene.instanced.iter().all(|m| m.look == InstanceLook::Opaque),
            "colliding non-transparent parts should stay opaque"
        );
    }

    #[test]
    fn mesh_to_shared_tris_uses_exact_vertex_normals_without_flat_derivatives() {
        let mesh = TriMesh::from_indexed(
            vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
            vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
        )
        .expect("valid tet");

        let tris = mesh_to_shared_tris(&mesh);
        assert!(!tris.flat, "must keep vertex normals instead of flat derivative reconstruction");
        for (tri_i, corners) in tris.corners().enumerate() {
            let expected = mesh.tri_normals[tri_i];
            for (_, n) in corners {
                for k in 0..3 {
                    assert!((n[k] - expected[k]).abs() < 1e-6);
                }
            }
        }
    }
}
