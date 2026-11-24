//! NestCraft 3D nesting desktop application.

#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod icons;
mod viewport;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use nestcraft_core::io::threemf::ThreeMfLoadedItem;
use nestcraft_core::io::{nestc, stl, threemf};
use nestcraft_core::manifold::{self, ManifoldReport};
use nestcraft_core::model::{
    BedOrigin, BuildJob, GroupId, IDENTITY_TRANSFORM, InstanceId, MachineProfile, NoBuildZone, PartId,
    RotationConstraint, TriMesh,
};
use nestcraft_core::sinterbox::{self, CageShape, LabelPanel, SinterboxConfig};
use serde::{Deserialize, Serialize};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Edge, Surface};
use solidcraft_ui_kit::view_compass::{self, Compass, Drag as CompassDrag, ViewAction, ViewAxes};
use solidcraft_ui_kit::{
    Button, Segments, StageBar, StatusTone, button, catalog_footer_with_tip, checkbox_row,
    icon_label_button, row, scope, section, segmented, status_line, value_row,
};
use solidcraft_viewport::vec::{Bounds, Vec3Ops as _};
use solidcraft_viewport::{Camera, StandardView};
use solidcraft_viewport_egui::{ViewportCallback, paint_background};
use solidcraft_viewport_gpu::view::{Frame, GpuScene, GpuTarget, MovingLayer, SceneSlot, SharedTris};

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NestCraft")
            .with_inner_size([1280.0, 820.0])
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        depth_buffer: 0,
        stencil_buffer: 0,
        multisampling: 1,
        ..Default::default()
    };

    eframe::run_native(
        "NestCraft",
        native_options,
        Box::new(|cc| {
            icons::install_look_and_icons(&cc.egui_ctx);
            solidcraft_theme::apply(&cc.egui_ctx);
            if let Some(rs) = &cc.wgpu_render_state {
                let window = GpuTarget {
                    format: rs.target_format,
                    depth: None,
                    samples: 1,
                    tess: false,
                    oit: false,
                    edges: false,
                };
                let scene = GpuTarget {
                    depth: Some(solidcraft_viewport_gpu::depth_format(&rs.device, true)),
                    ..window
                };
                let _ = solidcraft_viewport_egui::install(rs, scene, window, false, false, |err| {
                    eprintln!("nestcraft wgpu error: {err}");
                });
            }
            Ok(Box::new(NestCraftApp::new()))
        }),
    )
}

#[derive(Debug, Clone)]
struct DragState {
    anchor_id: InstanceId,
    instances: Vec<(InstanceId, [f32; 3])>,
    start_hit: [f32; 3],
    plane_z: f32,
}

#[derive(Debug, Clone)]
struct RotateDragState {
    pivot: [f32; 3],
    instances: Vec<(InstanceId, [[f32; 4]; 3], RotationConstraint)>,
    accum_dx: f32,
    accum_dy: f32,
}

#[derive(Debug, Clone, Copy)]
struct DragZoneState {
    start_position: [f32; 3],
    start_hit: [f32; 3],
    plane_z: f32,
}

#[derive(Debug, Clone, Copy)]
struct BoxSelectState {
    start: egui::Pos2,
    current: egui::Pos2,
    additive: bool,
}

enum ImportedKind {
    Stl(Arc<TriMesh>, Arc<SharedTris>),
    ThreeMf(Vec<(ThreeMfLoadedItem, Arc<SharedTris>)>),
}

#[derive(Debug, Clone)]
enum KeepOrDeleteAction {
    SplitShells(Vec<InstanceId>),
    MergeSelected(Vec<InstanceId>),
}

enum BackgroundLoadOutcome {
    Imported {
        name: String,
        kind: ImportedKind,
    },
    OpenedBed {
        job: BuildJob,
        path_display: String,
    },
    Failed(String),
}

struct LoadProgressMsg {
    completed: usize,
    total: usize,
    current_label: String,
    outcome: Option<BackgroundLoadOutcome>,
}

struct ActiveLoadTask {
    rx: Receiver<LoadProgressMsg>,
    completed: usize,
    total: usize,
    current_label: String,
    imported_files: usize,
    imported_items: usize,
    below_zero_instances: Vec<(InstanceId, f32)>,
    pushed_undo: bool,
    errors: Vec<String>,
}

#[derive(Clone)]
struct UndoSnapshot {
    job: BuildJob,
    selected: Option<InstanceId>,
    selected_set: HashSet<InstanceId>,
    selection_anchor: Option<InstanceId>,
}

fn load_single_file_outcome(path: &Path) -> BackgroundLoadOutcome {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return BackgroundLoadOutcome::Failed(format!("Failed to read {}: {e}", path.display())),
    };
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ext == "nestc" {
        match nestc::read_nestc(&bytes) {
            Ok(job) => BackgroundLoadOutcome::OpenedBed {
                job,
                path_display: path.display().to_string(),
            },
            Err(e) => BackgroundLoadOutcome::Failed(format!("Open bed error: {e}")),
        }
    } else if ext == "3mf" {
        match threemf::read_threemf(&bytes) {
            Ok(items) => {
                let prepared = items
                    .into_iter()
                    .map(|item| {
                        let tris = viewport::mesh_to_shared_tris(&item.mesh);
                        (item, tris)
                    })
                    .collect();
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file")
                    .to_owned();
                BackgroundLoadOutcome::Imported {
                    name,
                    kind: ImportedKind::ThreeMf(prepared),
                }
            }
            Err(e) => BackgroundLoadOutcome::Failed(format!("3MF import error: {e}")),
        }
    } else if ext == "stl" {
        match stl::parse_stl(&bytes) {
            Ok(mesh) => {
                let mesh = Arc::new(mesh);
                let tris = viewport::mesh_to_shared_tris(&mesh);
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("part")
                    .to_owned();
                BackgroundLoadOutcome::Imported {
                    name: stem,
                    kind: ImportedKind::Stl(mesh, tris),
                }
            }
            Err(e) => BackgroundLoadOutcome::Failed(format!("STL import error: {e}")),
        }
    } else {
        BackgroundLoadOutcome::Failed(format!(
            "Unsupported file '{}' (expected .stl, .3mf, or .nestc)",
            path.display()
        ))
    }
}

struct NestCraftApp {
    job: BuildJob,
    selected: Option<InstanceId>,
    selected_set: HashSet<InstanceId>,
    selection_anchor: Option<InstanceId>,
    camera: Camera,
    scene_cache: viewport::SceneCache,
    scene_slot: SceneSlot,
    moving_slot: SceneSlot,
    hl_slot: SceneSlot,
    empty_slot: SceneSlot,
    scene_key: u64,
    hl_key: u64,
    moving_key: u64,
    moving_target: Option<viewport::DragExclude>,
    align_face_target: Option<InstanceId>,
    hovered_face: Option<(InstanceId, usize)>,
    scene_dirty: bool,
    status: String,
    euler_edit: HashMap<InstanceId, [f32; 3]>,
    dup_dialog_open: bool,
    dup_axes: [bool; 3],
    dup_counts: [usize; 3],
    dup_total: usize,
    dup_exact_total: bool,
    dup_gap_mm: [f32; 3],
    move_dialog_open: bool,
    move_mode_idx: usize,
    move_xyz: [f32; 3],
    scale_dialog_open: bool,
    scale_uniform: bool,
    scale_keep_open: bool,
    scale_mode_idx: usize,
    scale_pct: [f32; 3],
    scale_abs_mm: [f32; 3],
    scale_factor: [f32; 3],
    scale_base_dims: [f32; 3],
    scale_initial_dims: [f32; 3],
    scale_initial_snapshot: Option<UndoSnapshot>,
    scale_applied_in_session: usize,
    sinterbox_dialog_open: bool,
    sinterbox_shape_idx: usize,
    sinterbox_inner_clearance: f32,
    sinterbox_window_mm: f32,
    sinterbox_window_auto: bool,
    sinterbox_bar_mm: f32,
    sinterbox_bar_auto: bool,
    sinterbox_label_enabled: bool,
    sinterbox_label_text: String,
    sinterbox_label_panel_idx: usize,
    sinterbox_label_depth: f32,
    sinterbox_group_name: String,
    sinterbox_group_name_edited: bool,
    sinterbox_transparent: bool,
    sinterbox_single_copies: usize,
    sinterbox_single_gap: f32,
    color_picker_open: bool,
    color_picker_rgb: [u8; 3],
    color_picker_undo_pushed: bool,
    pending_keep_delete: Option<KeepOrDeleteAction>,
    export_stl_dialog_open: bool,
    export_stl_selected_only: bool,
    export_stl_prefix: String,
    export_stl_suffix: String,
    export_stl_include_id: bool,
    export_stl_apply_transform: bool,
    drag_state: Option<DragState>,
    rotate_drag: Option<RotateDragState>,
    drag_zone: Option<DragZoneState>,
    box_select: Option<BoxSelectState>,
    zoom_rect_mode: bool,
    zoom_rect: Option<(egui::Pos2, egui::Pos2)>,
    viewport_aspect: f64,
    load_task: Option<ActiveLoadTask>,
    below_zero_import_dialog: Vec<(InstanceId, f32)>,
    undo_stack: Vec<UndoSnapshot>,
    redo_stack: Vec<UndoSnapshot>,
    viewport_ctx_menu: Option<InstanceId>,
    compass_drag: CompassDrag,
    sample_counter: u32,
    manifold_cache: HashMap<PartId, ManifoldReport>,
    part_thumbs: HashMap<PartId, egui::TextureHandle>,
    expanded_parts: HashSet<PartId>,
    active_machine_slot: usize,
    machine_presets: [MachinePresetConfig; 3],
    custom_platform_open: bool,
    custom_name: String,
    custom_size_mm: [f32; 3],
    custom_wall_margin: f32,
    custom_min_z: f32,
    custom_clearance: f32,
    custom_bed_origin: BedOrigin,
    new_group_name: String,
    view_bar_rect: Option<egui::Rect>,
    repair_badge_rect: Option<egui::Rect>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoBuildZoneConfig {
    name: String,
    position: [f32; 3],
    rotation_deg: [f32; 3],
    clearance_mm: f32,
    enabled: bool,
    visible: bool,
    #[serde(default = "default_true")]
    locked: bool,
}

impl From<&NoBuildZone> for NoBuildZoneConfig {
    fn from(z: &NoBuildZone) -> Self {
        Self {
            name: z.name.clone(),
            position: z.position,
            rotation_deg: z.rotation_deg,
            clearance_mm: z.clearance_mm,
            enabled: z.enabled,
            visible: z.visible,
            locked: z.locked,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
enum BedOriginCfg {
    #[default]
    FrontLeft,
    Center,
}

impl From<BedOrigin> for BedOriginCfg {
    fn from(o: BedOrigin) -> Self {
        match o {
            BedOrigin::FrontLeft => Self::FrontLeft,
            BedOrigin::Center => Self::Center,
        }
    }
}

impl From<BedOriginCfg> for BedOrigin {
    fn from(o: BedOriginCfg) -> Self {
        match o {
            BedOriginCfg::FrontLeft => Self::FrontLeft,
            BedOriginCfg::Center => Self::Center,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct MachinePresetConfig {
    name: String,
    size_mm: [f32; 3],
    wall_margin_mm: f32,
    min_z_mm: f32,
    clearance_mm: f32,
    #[serde(default)]
    bed_origin: BedOriginCfg,
}

fn default_machine_presets() -> [MachinePresetConfig; 3] {
    [
        MachinePresetConfig {
            name: "EOS Formiga P110".to_owned(),
            size_mm: MachineProfile::FormigaP110.size_mm(),
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::FrontLeft,
        },
        MachinePresetConfig {
            name: "Farsoon HT403P".to_owned(),
            size_mm: MachineProfile::FarsoonHT403P.size_mm(),
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::Center,
        },
        MachinePresetConfig {
            name: "Custom SLS".to_owned(),
            size_mm: [250.0, 250.0, 300.0],
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::FrontLeft,
        },
    ]
}

fn nestcraft_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA"));
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_DATA_HOME").or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share").into_os_string())
    });
    base.map(|b| PathBuf::from(b).join("NestCraft"))
}

fn save_machine_presets_to_dir(dir: &Path, presets: &[MachinePresetConfig; 3]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    let json = serde_json::to_string_pretty(presets).map_err(|e| format!("Serialize error: {e}"))?;
    std::fs::write(dir.join("machines.json"), json)
        .map_err(|e| format!("Failed to write machines.json: {e}"))
}

fn load_machine_presets_from_dir(dir: &Path) -> Option<[MachinePresetConfig; 3]> {
    let bytes = std::fs::read(dir.join("machines.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn no_build_zone_dir_for_slot(slot: usize) -> Option<PathBuf> {
    nestcraft_data_dir().map(|d| d.join("no_build_zone").join(format!("slot_{}", slot.min(2))))
}

fn sanitize_ascii_filename_part(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii())
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .collect()
}

fn export_part_menu_label(selected_count: usize) -> &'static str {
    if selected_count > 1 {
        "Export Parts..."
    } else {
        "Export Part..."
    }
}

#[derive(Clone, Copy)]
enum ColorMenuChoice {
    OpenPicker,
    Set(Option<[u8; 3]>),
}

fn show_color_submenu(ui: &mut egui::Ui) -> Option<ColorMenuChoice> {
    const PRESETS: &[(&str, [u8; 3])] = &[
        ("Red", [224, 72, 72]),
        ("Orange", [235, 140, 52]),
        ("Yellow", [235, 200, 56]),
        ("Green", [72, 188, 108]),
        ("Cyan", [56, 188, 212]),
        ("Blue", [72, 136, 236]),
        ("Purple", [168, 104, 232]),
        ("White", [232, 234, 238]),
    ];
    let mut choice = None;
    ui.menu_button("Color", |ui| {
        if ui.button("Color Picker...").clicked() {
            choice = Some(ColorMenuChoice::OpenPicker);
            ui.close();
        }
        ui.separator();
        for &(label, rgb) in PRESETS {
            let swatch = egui::RichText::new("■").color(egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
            ui.horizontal(|ui| {
                ui.label(swatch);
                if ui.button(label).clicked() {
                    choice = Some(ColorMenuChoice::Set(Some(rgb)));
                    ui.close();
                }
            });
        }
        ui.separator();
        if ui.button("Reset Default").clicked() {
            choice = Some(ColorMenuChoice::Set(None));
            ui.close();
        }
    });
    choice
}

const THUMB_PX: usize = 20;

fn render_part_thumbnail(mesh: &TriMesh) -> egui::ColorImage {
    let mut rgba = vec![0_u8; THUMB_PX * THUMB_PX * 4];
    if mesh.positions.is_empty() || mesh.indices.is_empty() {
        return egui::ColorImage::from_rgba_unmultiplied([THUMB_PX, THUMB_PX], &rgba);
    }
    let right = [0.707_106_77_f32, -0.707_106_77, 0.0];
    let up = [-0.408_248_3_f32, -0.408_248_3, 0.816_496_6];
    let fwd = [0.577_350_26_f32, 0.577_350_26, 0.577_350_26];
    let light = [0.35_f32, -0.25, 0.90];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];

    let mut min_u = f32::INFINITY;
    let mut max_u = f32::NEG_INFINITY;
    let mut min_v = f32::INFINITY;
    let mut max_v = f32::NEG_INFINITY;
    let p_step = (mesh.positions.len() / 2000).max(1);
    for p in mesh.positions.iter().step_by(p_step) {
        let u = dot(*p, right);
        let v = dot(*p, up);
        min_u = min_u.min(u);
        max_u = max_u.max(u);
        min_v = min_v.min(v);
        max_v = max_v.max(v);
    }
    let span = (max_u - min_u).max(max_v - min_v).max(1e-4);
    let pad = 1.5_f32;
    let usable = THUMB_PX as f32 - 2.0 * pad;
    let scale = usable / span;
    let cx = (min_u + max_u) * 0.5;
    let cy = (min_v + max_v) * 0.5;
    let half = THUMB_PX as f32 * 0.5;
    let project = |p: [f32; 3]| -> [f32; 3] {
        [
            half + (dot(p, right) - cx) * scale,
            half - (dot(p, up) - cy) * scale,
            dot(p, fwd),
        ]
    };

    let mut zbuf = [f32::NEG_INFINITY; THUMB_PX * THUMB_PX];
    let t_step = (mesh.indices.len() / 500).max(1);
    for (tri_i, tri) in mesh.indices.iter().enumerate().step_by(t_step) {
        let Some(&p0) = mesh.positions.get(tri[0] as usize) else { continue };
        let Some(&p1) = mesh.positions.get(tri[1] as usize) else { continue };
        let Some(&p2) = mesh.positions.get(tri[2] as usize) else { continue };
        let a = project(p0);
        let b = project(p1);
        let c = project(p2);
        let area = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
        if area.abs() < 1e-4 {
            continue;
        }
        let inv_area = 1.0 / area;
        let n = mesh.tri_normals.get(tri_i).copied().unwrap_or([0.0, 0.0, 1.0]);
        let shade = (0.38 + 0.62 * dot(n, light).abs()).clamp(0.0, 1.0);
        let lum = (shade * 255.0).round() as u8;

        let x0 = (a[0].min(b[0]).min(c[0]).floor() as isize).clamp(0, THUMB_PX as isize - 1) as usize;
        let x1 = (a[0].max(b[0]).max(c[0]).ceil() as isize).clamp(0, THUMB_PX as isize - 1) as usize;
        let y0 = (a[1].min(b[1]).min(c[1]).floor() as isize).clamp(0, THUMB_PX as isize - 1) as usize;
        let y1 = (a[1].max(b[1]).max(c[1]).ceil() as isize).clamp(0, THUMB_PX as isize - 1) as usize;
        for py in y0..=y1 {
            let fy = py as f32 + 0.5;
            for px in x0..=x1 {
                let fx = px as f32 + 0.5;
                let w0 = ((b[0] - fx) * (c[1] - fy) - (b[1] - fy) * (c[0] - fx)) * inv_area;
                let w1 = ((c[0] - fx) * (a[1] - fy) - (c[1] - fy) * (a[0] - fx)) * inv_area;
                let w2 = 1.0 - w0 - w1;
                if w0 >= -1e-3 && w1 >= -1e-3 && w2 >= -1e-3 {
                    let z = w0 * a[2] + w1 * b[2] + w2 * c[2];
                    let idx = py * THUMB_PX + px;
                    if z > zbuf[idx] {
                        zbuf[idx] = z;
                        let off = idx * 4;
                        rgba[off] = lum;
                        rgba[off + 1] = lum;
                        rgba[off + 2] = lum;
                        rgba[off + 3] = 255;
                    }
                }
            }
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([THUMB_PX, THUMB_PX], &rgba)
}

fn format_export_stl_stem(
    prefix: &str,
    part_name: &str,
    instance_id: u32,
    include_id: bool,
    suffix: &str,
) -> String {
    let clean_prefix = sanitize_ascii_filename_part(prefix);
    let clean_suffix = sanitize_ascii_filename_part(suffix);
    let clean_name = sanitize_ascii_filename_part(part_name);
    let base = if include_id {
        format!("{clean_prefix}{clean_name}_{instance_id}{clean_suffix}")
    } else {
        format!("{clean_prefix}{clean_name}{clean_suffix}")
    };
    if base.is_empty() {
        format!("part_{instance_id}")
    } else {
        base
    }
}

fn save_no_build_zone_config_to_dir(dir: &Path, zone: &NoBuildZone) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    let cfg = NoBuildZoneConfig::from(zone);
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| format!("Serialize error: {e}"))?;
    std::fs::write(dir.join("config.json"), json)
        .map_err(|e| format!("Failed to write no-build zone config: {e}"))
}

fn load_no_build_zone_from_dir(dir: &Path) -> Option<NoBuildZone> {
    let stl_bytes = std::fs::read(dir.join("zone.stl")).ok()?;
    let mesh = stl::parse_stl(&stl_bytes).ok()?;
    let cfg_bytes = std::fs::read(dir.join("config.json")).ok()?;
    let cfg: NoBuildZoneConfig = serde_json::from_slice(&cfg_bytes).ok()?;
    Some(NoBuildZone {
        name: cfg.name,
        mesh: Arc::new(mesh),
        position: cfg.position,
        rotation_deg: cfg.rotation_deg,
        clearance_mm: cfg.clearance_mm,
        enabled: cfg.enabled,
        visible: cfg.visible,
        locked: cfg.locked,
    })
}

const VIEW_CYCLE: [StandardView; 7] = [
    StandardView::Iso,
    StandardView::Top,
    StandardView::Bottom,
    StandardView::Front,
    StandardView::Back,
    StandardView::Right,
    StandardView::Left,
];

fn view_label(view: StandardView) -> &'static str {
    match view {
        StandardView::Iso => "Iso",
        StandardView::Top => "Top",
        StandardView::Bottom => "Bottom",
        StandardView::Front => "Front",
        StandardView::Back => "Back",
        StandardView::Right => "Right",
        StandardView::Left => "Left",
    }
}

fn distribute_compact_grid(total: usize, enabled: [bool; 3]) -> [usize; 3] {
    let total = total.max(1);
    let active: Vec<usize> = (0..3).filter(|&k| enabled[k]).collect();
    let mut out = [1usize; 3];
    match active.as_slice() {
        [] => out[0] = total,
        &[a] => out[a] = total,
        &[a, b] => {
            let mut best = (total, 1usize, total, total - 1);
            let limit = (total as f64).sqrt().ceil() as usize + 1;
            for x in 1..=limit {
                let y = total.div_ceil(x);
                let prod = x * y;
                let spread = x.abs_diff(y);
                let score = (prod, spread);
                if score < (best.2, best.3) {
                    best = (x.max(y), x.min(y), prod, spread);
                }
            }
            out[a] = best.0;
            out[b] = best.1;
        }
        _ => {
            let mut best = (total, 1usize, 1usize, total, total - 1);
            let limit = (total as f64).cbrt().ceil() as usize + 2;
            for z in 1..=limit {
                let rem = total.div_ceil(z);
                let y_lim = (rem as f64).sqrt().ceil() as usize + 1;
                for y in 1..=y_lim {
                    let x = total.div_ceil(y * z);
                    let prod = x * y * z;
                    let max_d = x.max(y).max(z);
                    let min_d = x.min(y).min(z);
                    let spread = max_d - min_d;
                    if (prod, spread) < (best.3, best.4) {
                        let mut dims = [x, y, z];
                        dims.sort_unstable_by(|a, b| b.cmp(a));
                        best = (dims[0], dims[1], dims[2], prod, spread);
                    }
                }
            }
            out = [best.0, best.1, best.2];
        }
    }
    out
}

impl NestCraftApp {
    fn new() -> Self {
        let machine_presets = nestcraft_data_dir()
            .and_then(|d| load_machine_presets_from_dir(&d))
            .unwrap_or_else(default_machine_presets);
        let p0 = &machine_presets[0];
        let mut job = BuildJob::new(MachineProfile::custom(&p0.name, p0.size_mm));
        job.wall_margin_mm = p0.wall_margin_mm;
        job.min_z_mm = p0.min_z_mm;
        job.clearance_mm = p0.clearance_mm;
        job.bed_origin = p0.bed_origin.into();
        if let Some(dir) = no_build_zone_dir_for_slot(0) {
            job.no_build_zone = load_no_build_zone_from_dir(&dir);
        }
        let camera = viewport::default_camera_with_origin(&job.machine, job.bed_origin);
        let custom_name = p0.name.clone();
        let custom_size_mm = job.machine.size_mm();
        let custom_wall_margin = job.wall_margin_mm;
        let custom_min_z = job.min_z_mm;
        let custom_clearance = job.clearance_mm;
        let custom_bed_origin = job.bed_origin;
        Self {
            job,
            selected: None,
            selected_set: HashSet::new(),
            selection_anchor: None,
            camera,
            scene_cache: viewport::SceneCache::default(),
            scene_slot: Arc::new(Mutex::new(None)),
            moving_slot: Arc::new(Mutex::new(None)),
            hl_slot: Arc::new(Mutex::new(None)),
            empty_slot: Arc::new(Mutex::new(None)),
            scene_key: 1,
            hl_key: 0,
            moving_key: 1,
            moving_target: None,
            align_face_target: None,
            hovered_face: None,
            scene_dirty: true,
            status: "Ready — import an STL/3MF or add a sample part.".to_owned(),
            euler_edit: HashMap::new(),
            dup_dialog_open: false,
            dup_axes: [true, true, false],
            dup_counts: [2, 1, 1],
            dup_total: 2,
            dup_exact_total: false,
            dup_gap_mm: [custom_clearance; 3],
            move_dialog_open: false,
            move_mode_idx: 0,
            move_xyz: [0.0, 0.0, 0.0],
            scale_dialog_open: false,
            scale_uniform: true,
            scale_keep_open: false,
            scale_mode_idx: 0,
            scale_pct: [100.0, 100.0, 100.0],
            scale_abs_mm: [10.0, 10.0, 10.0],
            scale_factor: [1.0, 1.0, 1.0],
            scale_base_dims: [10.0, 10.0, 10.0],
            scale_initial_dims: [10.0, 10.0, 10.0],
            scale_initial_snapshot: None,
            scale_applied_in_session: 0,
            sinterbox_dialog_open: false,
            sinterbox_shape_idx: 0,
            sinterbox_inner_clearance: custom_clearance,
            sinterbox_window_mm: 4.0,
            sinterbox_window_auto: true,
            sinterbox_bar_mm: 0.80,
            sinterbox_bar_auto: true,
            sinterbox_label_enabled: true,
            sinterbox_label_text: String::new(),
            sinterbox_label_panel_idx: 0,
            sinterbox_label_depth: 0.8,
            sinterbox_group_name: String::new(),
            sinterbox_group_name_edited: false,
            sinterbox_transparent: true,
            sinterbox_single_copies: 1,
            sinterbox_single_gap: custom_clearance,
            color_picker_open: false,
            color_picker_rgb: [220, 220, 220],
            color_picker_undo_pushed: false,
            pending_keep_delete: None,
            export_stl_dialog_open: false,
            export_stl_selected_only: false,
            export_stl_prefix: String::new(),
            export_stl_suffix: String::new(),
            export_stl_include_id: true,
            export_stl_apply_transform: true,
            drag_state: None,
            rotate_drag: None,
            drag_zone: None,
            box_select: None,
            zoom_rect_mode: false,
            zoom_rect: None,
            viewport_aspect: 16.0 / 9.0,
            load_task: None,
            below_zero_import_dialog: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            viewport_ctx_menu: None,
            compass_drag: CompassDrag::default(),
            sample_counter: 0,
            manifold_cache: HashMap::new(),
            part_thumbs: HashMap::new(),
            expanded_parts: HashSet::new(),
            active_machine_slot: 0,
            machine_presets,
            custom_platform_open: false,
            custom_name,
            custom_size_mm,
            custom_wall_margin,
            custom_min_z,
            custom_clearance,
            custom_bed_origin,
            new_group_name: String::new(),
            view_bar_rect: None,
            repair_badge_rect: None,
        }
    }

    fn manifold_report(&mut self, part_id: PartId) -> Option<ManifoldReport> {
        if let Some(report) = self.manifold_cache.get(&part_id) {
            return Some(report.clone());
        }
        let mesh = self.job.part(part_id)?.mesh.clone();
        let report = manifold::analyze_mesh(&mesh);
        self.manifold_cache.insert(part_id, report.clone());
        Some(report)
    }

    fn faulty_placed_instance_ids(&mut self) -> Vec<InstanceId> {
        let pairs: Vec<(InstanceId, PartId)> = self.job.instances.iter().map(|i| (i.id, i.part_id)).collect();
        pairs
            .into_iter()
            .filter_map(|(iid, pid)| {
                self.manifold_report(pid)
                    .filter(|r| !r.is_watertight_manifold())
                    .map(|_| iid)
            })
            .collect()
    }

    fn apply_machine_slot(&mut self, slot: usize) {
        let idx = slot.min(2);
        self.active_machine_slot = idx;
        let p = &self.machine_presets[idx];
        self.job.machine = MachineProfile::custom(&p.name, p.size_mm);
        self.job.wall_margin_mm = p.wall_margin_mm;
        self.job.min_z_mm = p.min_z_mm;
        self.job.clearance_mm = p.clearance_mm;
        self.job.bed_origin = p.bed_origin.into();
        self.job.no_build_zone = no_build_zone_dir_for_slot(idx).and_then(|d| load_no_build_zone_from_dir(&d));
        self.camera = viewport::default_camera_with_origin(&self.job.machine, self.job.bed_origin);
        self.mark_dirty();
        self.status = format!("Active machine: {}", self.job.machine.name());
    }

    fn open_custom_platform_dialog(&mut self) {
        let p = &self.machine_presets[self.active_machine_slot.min(2)];
        self.custom_name = p.name.clone();
        self.custom_size_mm = self.job.machine.size_mm();
        self.custom_wall_margin = self.job.wall_margin_mm;
        self.custom_min_z = self.job.min_z_mm;
        self.custom_clearance = self.job.clearance_mm;
        self.custom_bed_origin = self.job.bed_origin;
        self.custom_platform_open = true;
    }

    fn split_part_shells(&mut self, part_id: PartId) {
        let Some(proto) = self.job.part(part_id).cloned() else {
            return;
        };
        let Ok(shells) = manifold::split_shells(&proto.mesh) else {
            self.status = format!("Failed to split shells for '{}'", proto.name);
            return;
        };
        if shells.len() <= 1 {
            self.status = format!("'{}' has only 1 shell.", proto.name);
            return;
        }
        let count = shells.len();
        for (idx, shell_mesh) in shells.into_iter().enumerate() {
            let name = format!("{} [Shell {}]", proto.name, idx + 1);
            let pid = self.job.add_part(name, Arc::new(shell_mesh));
            if let Ok(iid) = self.job.add_instance_on_floor_next(pid) {
                self.select_instance(Some(iid));
            }
        }
        self.mark_dirty();
        self.status = format!("Split '{}' into {count} shell parts.", proto.name);
    }

    fn request_shells_to_parts(&mut self, ids: Vec<InstanceId>) {
        if ids.is_empty() {
            return;
        }
        let any_multi_shell = ids.iter().any(|id| {
            self.job
                .instances
                .iter()
                .find(|i| i.id == *id)
                .and_then(|inst| self.job.part(inst.part_id))
                .and_then(|proto| manifold::split_shells(&proto.mesh).ok())
                .is_some_and(|shells| shells.len() > 1)
        });
        if !any_multi_shell {
            self.status = "Selected part(s) have only 1 shell.".to_owned();
            return;
        }
        self.pending_keep_delete = Some(KeepOrDeleteAction::SplitShells(ids));
    }

    fn shells_to_parts_for(&mut self, ids: Vec<InstanceId>) {
        self.shells_to_parts_for_with_keep(ids, false);
    }

    fn shells_to_parts_for_with_keep(&mut self, ids: Vec<InstanceId>, keep_source: bool) {
        if ids.is_empty() {
            return;
        }
        self.push_undo();
        let mut split_sources = 0usize;
        let mut created_all = Vec::new();
        for id in ids {
            if let Ok(created) = self.job.split_instance_shells_with_options(id, keep_source)
                && !created.is_empty()
            {
                split_sources += 1;
                if !keep_source {
                    self.euler_edit.remove(&id);
                    self.selected_set.remove(&id);
                }
                created_all.extend(created);
            }
        }
        if created_all.is_empty() {
            self.undo_stack.pop();
            self.status = "Selected part(s) have only 1 shell.".to_owned();
            return;
        }
        self.selected_set.clear();
        for &id in &created_all {
            self.selected_set.insert(id);
        }
        self.selected = created_all.last().copied();
        self.selection_anchor = self.selected;
        self.mark_dirty();
        self.status = format!(
            "Split {split_sources} part(s) into {} shell parts{}.",
            created_all.len(),
            if keep_source { " (kept original)" } else { "" }
        );
    }

    fn request_merge_selected_parts(&mut self) {
        if self.selected_set.len() < 2 {
            self.status = "Select at least 2 parts to merge.".to_owned();
            return;
        }
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        self.pending_keep_delete = Some(KeepOrDeleteAction::MergeSelected(ids));
    }

    fn merge_selected_parts(&mut self) {
        if self.selected_set.len() < 2 {
            self.status = "Select at least 2 parts to merge.".to_owned();
            return;
        }
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        self.merge_selected_parts_with_keep(&ids, false);
    }

    fn merge_selected_parts_with_keep(&mut self, ids: &[InstanceId], keep_sources: bool) {
        if ids.len() < 2 {
            self.status = "Select at least 2 parts to merge.".to_owned();
            return;
        }
        self.push_undo();
        match self.job.merge_instances_with_options(ids, keep_sources) {
            Ok(merged_id) => {
                if !keep_sources {
                    for id in ids {
                        self.euler_edit.remove(id);
                    }
                }
                self.select_instance(Some(merged_id));
                self.mark_dirty();
                self.status = format!(
                    "Merged {} parts into #{}{}",
                    ids.len(),
                    merged_id.0,
                    if keep_sources { " (kept originals)" } else { "" }
                );
            }
            Err(e) => {
                self.undo_stack.pop();
                self.status = format!("Merge failed: {e}");
            }
        }
    }

    fn group_selected_parts(&mut self, name: String) {
        if self.selected_set.is_empty() {
            return;
        }
        self.push_undo();
        let gid = self.job.create_group(name);
        let sel_ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        let mut assigned = 0usize;
        for sel in sel_ids {
            if self.job.set_instance_group(sel, Some(gid)) {
                assigned += 1;
            }
        }
        self.mark_dirty();
        self.status = format!("Grouped {assigned} part(s)");
    }

    fn ungroup_selected_parts(&mut self) {
        let grouped_ids: Vec<InstanceId> = self
            .job
            .instances
            .iter()
            .filter(|i| self.selected_set.contains(&i.id) && i.group_id.is_some())
            .map(|i| i.id)
            .collect();
        if grouped_ids.is_empty() {
            return;
        }
        self.push_undo();
        let mut changed = 0usize;
        for id in grouped_ids {
            if self.job.set_instance_group(id, None) {
                changed += 1;
            }
        }
        if changed > 0 {
            self.mark_dirty();
            self.status = format!("Ungrouped {changed} part(s)");
        }
    }

    fn select_instance(&mut self, id: Option<InstanceId>) {
        self.selected_set.clear();
        if let Some(iid) = id {
            self.selected_set.insert(iid);
            if let Some(inst) = self.job.instances.iter().find(|i| i.id == iid) {
                self.euler_edit.insert(iid, inst.euler_deg());
            }
        }
        self.selected = id;
        self.selection_anchor = id;
        self.mark_dirty();
    }

    fn toggle_select_instance(&mut self, id: InstanceId) {
        if self.selected_set.contains(&id) {
            self.selected_set.remove(&id);
            if self.selected == Some(id) {
                self.selected = self.selected_set.iter().copied().next();
            }
        } else {
            self.selected_set.insert(id);
            self.selected = Some(id);
            if let Some(inst) = self.job.instances.iter().find(|i| i.id == id) {
                self.euler_edit.insert(id, inst.euler_deg());
            }
        }
        self.selection_anchor = Some(id);
        self.mark_dirty();
    }

    fn select_tree_range(&mut self, ordered_ids: &[InstanceId], target: InstanceId) {
        let anchor = self.selection_anchor.or(self.selected).unwrap_or(target);
        if let (Some(a), Some(b)) = (
            ordered_ids.iter().position(|&id| id == anchor),
            ordered_ids.iter().position(|&id| id == target),
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            self.selected_set.clear();
            for &id in &ordered_ids[lo..=hi] {
                self.selected_set.insert(id);
            }
            self.selected = Some(target);
            self.selection_anchor = Some(anchor);
            if let Some(inst) = self.job.instances.iter().find(|i| i.id == target) {
                self.euler_edit.insert(target, inst.euler_deg());
            }
            self.mark_dirty();
        } else {
            self.select_instance(Some(target));
        }
    }

    fn add_sample_part(&mut self) {
        self.sample_counter += 1;
        let idx = self.sample_counter;
        if let Ok(mesh) = create_sample_bracket_mesh(idx) {
            self.push_undo();
            let name = format!("Sample Bracket #{idx}");
            let pid = self.job.add_part(name.clone(), Arc::new(mesh));
            if let Ok(iid) = self.job.add_instance_on_floor_next(pid) {
                self.select_instance(Some(iid));
                self.status = format!("Added {name}");
            }
        }
    }

    fn rotate_selected_90(&mut self, axis: usize) {
        self.rotate_selected_by(axis, 90.0);
    }

    fn rotate_selected_by(&mut self, axis: usize, deg: f32) {
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        self.push_undo();
        for sel_id in ids {
            let new_euler = if let Some(inst) = self.job.instances.iter().find(|i| i.id == sel_id) {
                let euler = self.euler_edit.entry(sel_id).or_insert_with(|| inst.euler_deg());
                euler[axis] = (euler[axis] + deg).rem_euclid(360.0);
                Some(*euler)
            } else {
                None
            };
            if let Some(euler) = new_euler {
                self.job.set_instance_euler_around_center(sel_id, euler);
                self.job.clamp_instance_above_floor(sel_id);
            }
        }
        self.mark_dirty();
    }

    fn open_move_dialog(&mut self) {
        if self.selected_set.is_empty() {
            return;
        }
        if self.move_mode_idx == 1
            && let Some(primary) = self.selected
            && let Some(inst) = self.job.instances.iter().find(|i| i.id == primary)
        {
            self.move_xyz = inst.translation();
        }
        self.move_dialog_open = true;
    }

    fn selected_primary_dims(&self) -> [f32; 3] {
        self.selected
            .or_else(|| self.selected_set.iter().copied().next())
            .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
            .and_then(|inst| {
                self.job.part(inst.part_id).map(|p| {
                    let wb = inst.world_bounds(p.mesh.bounds);
                    [
                        (wb[1][0] - wb[0][0]).max(0.01),
                        (wb[1][1] - wb[0][1]).max(0.01),
                        (wb[1][2] - wb[0][2]).max(0.01),
                    ]
                })
            })
            .unwrap_or([10.0, 10.0, 10.0])
    }

    fn open_scale_dialog(&mut self) {
        if self.selected_set.is_empty() {
            self.status = "Select part(s) to scale".to_owned();
            return;
        }
        let dims = self.selected_primary_dims();
        self.scale_pct = [100.0, 100.0, 100.0];
        self.scale_factor = [1.0, 1.0, 1.0];
        self.scale_abs_mm = dims;
        self.scale_base_dims = dims;
        self.scale_initial_dims = dims;
        self.scale_initial_snapshot = Some(self.snapshot());
        self.scale_applied_in_session = 0;
        self.scale_dialog_open = true;
    }

    fn reset_scale_in_dialog(&mut self) {
        if self.scale_applied_in_session > 0
            && let Some(initial) = self.scale_initial_snapshot.clone()
        {
            self.push_undo();
            self.restore_snapshot(initial);
            self.scale_applied_in_session = 0;
            self.status = "Reset scale to original dimensions".to_owned();
        }
        self.scale_pct = [100.0, 100.0, 100.0];
        self.scale_factor = [1.0, 1.0, 1.0];
        self.scale_abs_mm = self.scale_initial_dims;
        self.scale_base_dims = self.scale_initial_dims;
    }

    fn apply_scale_selection(&mut self, factors: [f32; 3]) {
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        self.push_undo();
        let mut scaled = 0usize;
        for id in ids {
            match self.job.scale_instance(id, factors) {
                Ok(pid) => {
                    self.scene_cache.parts.remove(&pid);
                    self.manifold_cache.remove(&pid);
                    self.part_thumbs.remove(&pid);
                    scaled += 1;
                }
                Err(e) => {
                    self.undo_stack.pop();
                    self.status = format!("Scale failed: {e}");
                    return;
                }
            }
        }
        if scaled > 0 {
            self.mark_dirty();
            self.status = format!(
                "Scaled {scaled} part(s) by ({:.3}×, {:.3}×, {:.3}×)",
                factors[0], factors[1], factors[2]
            );
        }
    }

    fn apply_move_selection(&mut self, absolute: bool, xyz: [f32; 3]) {
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let delta = if absolute {
            let Some(anchor_id) = self.selected.or_else(|| ids.first().copied()) else {
                return;
            };
            let Some(anchor) = self.job.instances.iter().find(|i| i.id == anchor_id) else {
                return;
            };
            let cur = anchor.translation();
            [xyz[0] - cur[0], xyz[1] - cur[1], xyz[2] - cur[2]]
        } else {
            xyz
        };
        self.push_undo();

        let mut moved = 0usize;
        for id in ids {
            if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == id) {
                let mut pos = inst.translation();
                pos[0] += delta[0];
                pos[1] += delta[1];
                pos[2] += delta[2];
                inst.set_translation(pos);
                self.job.clamp_instance_above_floor(id);
                moved += 1;
            }
        }
        if moved > 0 {
            self.mark_dirty();
            self.status = if absolute {
                format!("Moved {moved} part(s) to ({:.1}, {:.1}, {:.1})", xyz[0], xyz[1], xyz[2])
            } else {
                format!("Moved {moved} part(s) by ({:.1}, {:.1}, {:.1})", delta[0], delta[1], delta[2])
            };
        }
    }

    fn open_duplicate_dialog(&mut self) {
        if self.selected.is_none() {
            return;
        }
        self.dup_gap_mm = [self.job.clearance_mm; 3];
        self.dup_dialog_open = true;
    }

    fn select_group_instances(&mut self, gid: GroupId) {
        let members: Vec<InstanceId> = self
            .job
            .instances
            .iter()
            .filter(|i| i.group_id == Some(gid))
            .map(|i| i.id)
            .collect();
        self.selected_set.clear();
        for &id in &members {
            self.selected_set.insert(id);
            if let Some(inst) = self.job.instances.iter().find(|i| i.id == id) {
                self.euler_edit.insert(id, inst.euler_deg());
            }
        }
        self.selected = members.first().copied();
        self.selection_anchor = self.selected;
        self.mark_dirty();
        self.status = format!("Selected {} part(s) in group", members.len());
    }

    fn open_sinterbox_dialog(&mut self) {
        if self.selected_set.is_empty() {
            self.status = "Select part(s) to create a Sinterbox".to_owned();
            return;
        }
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        match sinterbox::analyze_sinterbox_inputs(&self.job, &ids) {
            Ok((_bounds, min_dim, default_name)) => {
                let rec_win = sinterbox::recommended_window_size_mm(min_dim);
                let rec_bar = sinterbox::recommended_bar_thickness_mm(rec_win);
                self.sinterbox_inner_clearance = self.job.clearance_mm.max(0.5);
                self.sinterbox_window_mm = rec_win;
                self.sinterbox_window_auto = true;
                self.sinterbox_bar_mm = rec_bar;
                self.sinterbox_bar_auto = true;
                self.sinterbox_label_enabled = true;
                self.sinterbox_label_text = default_name.clone();
                self.sinterbox_label_panel_idx = 0;
                self.sinterbox_label_depth = 0.8;
                self.sinterbox_group_name = format!("Sinterbox: {default_name}");
                self.sinterbox_group_name_edited = false;
                self.sinterbox_transparent = true;
                self.sinterbox_single_copies = 1;
                self.sinterbox_single_gap = self.job.clearance_mm.max(0.0);
                self.sinterbox_dialog_open = true;
            }
            Err(e) => {
                self.status = format!("Sinterbox unavailable: {e}");
            }
        }
    }

    fn current_sinterbox_config(&self) -> SinterboxConfig {
        let shape = if self.sinterbox_shape_idx == 1 {
            CageShape::TightFit
        } else {
            CageShape::Box
        };
        let label_panel = match self.sinterbox_label_panel_idx {
            1 => LabelPanel::TopPosZ,
            2 => LabelPanel::FrontNegY,
            3 => LabelPanel::RightPosX,
            4 => LabelPanel::BackPosY,
            5 => LabelPanel::LeftNegX,
            6 => LabelPanel::BottomNegZ,
            _ => LabelPanel::AutoLargest,
        };
        SinterboxConfig {
            shape,
            inner_clearance_mm: self.sinterbox_inner_clearance,
            window_size_mm: self.sinterbox_window_mm,
            bar_thickness_mm: self.sinterbox_bar_mm,
            label_enabled: self.sinterbox_label_enabled,
            label_text: self.sinterbox_label_text.clone(),
            label_panel,
            label_depth_mm: self.sinterbox_label_depth,
            group_name: self.sinterbox_group_name.clone(),
            cage_transparent: self.sinterbox_transparent,
        }
    }

    fn apply_create_sinterbox(&mut self) {
        let mut target_ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        if target_ids.is_empty() {
            return;
        }
        self.push_undo();

        if target_ids.len() == 1 && self.sinterbox_single_copies > 1 {
            let primary = target_ids[0];
            let grid_counts = distribute_compact_grid(self.sinterbox_single_copies, [true, true, false]);
            let gap = [self.sinterbox_single_gap.max(0.0); 3];
            match self.job.duplicate_instance_grid(
                primary,
                grid_counts,
                gap,
                Some(self.sinterbox_single_copies.saturating_sub(1)),
            ) {
                Ok(extra) => target_ids.extend(extra),
                Err(e) => {
                    self.undo_stack.pop();
                    self.status = format!("Sinterbox multiply failed: {e}");
                    return;
                }
            }
        }

        let cfg = self.current_sinterbox_config();
        match sinterbox::create_sinterbox(
            &mut self.job,
            &target_ids,
            &cfg,
            Some(solidcraft_look::fonts::PLEX_SANS),
        ) {
            Ok(res) => {
                self.selected_set.clear();
                for &id in &target_ids {
                    self.selected_set.insert(id);
                }
                self.selected_set.insert(res.cage_instance_id);
                self.selected = Some(res.cage_instance_id);
                self.selection_anchor = Some(res.cage_instance_id);
                self.mark_dirty();
                let shape_str = match cfg.shape {
                    CageShape::Box => "Box",
                    CageShape::TightFit => "Tight Fit",
                };
                self.status = format!(
                    "Created {shape_str} Sinterbox ({:.1}×{:.1}×{:.1} mm, window {:.1} mm, bar {:.2} mm)",
                    res.metrics.outer_size_mm[0],
                    res.metrics.outer_size_mm[1],
                    res.metrics.outer_size_mm[2],
                    cfg.window_size_mm,
                    cfg.bar_thickness_mm,
                );
            }
            Err(e) => {
                if let Some(snap) = self.undo_stack.pop() {
                    self.restore_snapshot(snap);
                }
                self.status = format!("Sinterbox failed: {e}");
            }
        }
    }

    fn snapshot(&self) -> UndoSnapshot {
        UndoSnapshot {
            job: self.job.clone(),
            selected: self.selected,
            selected_set: self.selected_set.clone(),
            selection_anchor: self.selection_anchor,
        }
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(self.snapshot());
        self.redo_stack.clear();
    }

    fn restore_snapshot(&mut self, snap: UndoSnapshot) {
        self.job = snap.job;
        self.selected = snap.selected;
        self.selected_set = snap.selected_set;
        self.selection_anchor = snap.selection_anchor;
        self.euler_edit.clear();
        self.drag_state = None;
        self.drag_zone = None;
        self.moving_target = None;
        self.clear_align_face_mode();
        self.below_zero_import_dialog.clear();
        self.mark_dirty();
    }

    fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            self.status = "Nothing to undo".to_owned();
            return false;
        };
        let cur = self.snapshot();
        self.redo_stack.push(cur);
        self.restore_snapshot(prev);
        self.status = format!("Undo ({} step(s) left)", self.undo_stack.len());
        true
    }

    fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            self.status = "Nothing to redo".to_owned();
            return false;
        };
        let cur = self.snapshot();
        self.undo_stack.push(cur);
        self.restore_snapshot(next);
        self.status = format!("Redo ({} step(s) left)", self.redo_stack.len());
        true
    }

    fn duplicate_selected(&mut self) {
        let Some(sel) = self.selected else {
            return;
        };
        self.push_undo();
        let counts = [
            if self.dup_axes[0] { self.dup_counts[0].max(1) } else { 1 },
            if self.dup_axes[1] { self.dup_counts[1].max(1) } else { 1 },
            if self.dup_axes[2] { self.dup_counts[2].max(1) } else { 1 },
        ];
        let max_new = self.dup_exact_total.then(|| self.dup_total.saturating_sub(1));
        if let Ok(ids) = self.job.duplicate_instance_grid(sel, counts, self.dup_gap_mm, max_new) {
            if let Some(last) = ids.last().copied() {
                self.select_instance(Some(last));
            } else {
                self.mark_dirty();
            }
            let created = ids.len();
            self.status = format!("Duplicated {created} instance(s)");
        }
    }

    fn start_align_face_mode(&mut self, target: InstanceId) {
        self.clear_align_face_mode();
        self.align_face_target = Some(target);
        self.status = "Align mode: hover a face to highlight and click to snap it to -Z (Esc to cancel)".to_owned();
    }

    fn clear_align_face_mode(&mut self) {
        self.align_face_target = None;
        self.hovered_face = None;
        if let Ok(mut guard) = self.hl_slot.lock() {
            *guard = Some(GpuScene::default());
        }
        self.hl_key = self.hl_key.wrapping_add(1).max(1);
    }

    fn delete_selected(&mut self) {
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        self.push_undo();
        let mut removed = 0usize;
        for id in ids {
            if self.job.remove_instance(id) {
                self.euler_edit.remove(&id);
                removed += 1;
            }
        }
        self.selected_set.clear();
        self.selected = None;
        self.selection_anchor = None;
        if removed > 0 {
            self.mark_dirty();
            self.status = if removed == 1 {
                "Removed selected instance".to_owned()
            } else {
                format!("Removed {removed} selected instances")
            };
        }
    }

    fn nudge_selected(&mut self, delta: [f32; 3]) {
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        self.push_undo();
        for sel_id in ids {
            if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == sel_id) {
                let mut pos = inst.translation();
                pos[0] += delta[0];
                pos[1] += delta[1];
                pos[2] += delta[2];
                inst.set_translation(pos);
                if delta[2] < 0.0 {
                    self.job.clamp_instance_above_floor(sel_id);
                }
            }
        }
        self.mark_dirty();
    }

    fn toggle_selected_hidden(&mut self) {
        let Some(primary) = self.selected else {
            return;
        };
        self.push_undo();
        let currently_hidden = self
            .job
            .instances
            .iter()
            .find(|i| i.id == primary)
            .is_some_and(|i| i.hidden);
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        let mut changed = 0usize;
        for id in ids {
            if self.job.set_instance_hidden(id, !currently_hidden) {
                changed += 1;
            }
        }
        if changed > 0 {
            self.mark_dirty();
            self.status = if !currently_hidden {
                format!("Hidden {changed} instance(s)")
            } else {
                format!("Shown {changed} instance(s)")
            };
        }
    }

    fn toggle_selected_transparent(&mut self) {
        let Some(primary) = self.selected else {
            return;
        };
        self.push_undo();
        let currently_transparent = self
            .job
            .instances
            .iter()
            .find(|i| i.id == primary)
            .is_some_and(|i| i.transparent);
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        let mut changed = 0usize;
        for id in ids {
            if self.job.set_instance_transparent(id, !currently_transparent) {
                changed += 1;
            }
        }
        if changed > 0 {
            self.mark_dirty();
            self.status = if currently_transparent {
                format!("Set {changed} instance(s) opaque")
            } else {
                format!("Set {changed} instance(s) 50% transparent")
            };
        }
    }

    fn set_selected_color(&mut self, color: Option<[u8; 3]>) {
        if self.selected_set.is_empty() {
            return;
        }
        self.push_undo();
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        let mut changed = 0usize;
        for id in ids {
            if self.job.set_instance_color(id, color) {
                changed += 1;
            }
        }
        if changed > 0 {
            self.mark_dirty();
            self.status = match color {
                Some([r, g, b]) => format!("Set color #{r:02X}{g:02X}{b:02X} on {changed} instance(s)"),
                None => format!("Reset color on {changed} instance(s)"),
            };
        }
    }

    fn open_color_picker(&mut self) {
        let fallback = {
            let t = Tokens::get();
            [t.text.r(), t.text.g(), t.text.b()]
        };
        let current = self
            .selected
            .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
            .and_then(|i| i.color)
            .unwrap_or(fallback);
        self.color_picker_rgb = current;
        self.color_picker_undo_pushed = false;
        self.color_picker_open = true;
    }

    fn draw_color_picker_modal(&mut self, ctx: &egui::Context) {
        if !self.color_picker_open {
            return;
        }
        let mut open = self.color_picker_open;
        let mut close_now = false;
        let mut reset_now = false;
        let mut new_rgb = None;

        egui::Window::new("Part Color")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                let mut c = egui::Color32::from_rgb(
                    self.color_picker_rgb[0],
                    self.color_picker_rgb[1],
                    self.color_picker_rgb[2],
                );
                if egui::color_picker::color_picker_color32(ui, &mut c, egui::color_picker::Alpha::Opaque) {
                    new_rgb = Some([c.r(), c.g(), c.b()]);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Done", Button::Primary).clicked() {
                        close_now = true;
                    }
                    if button(ui, "Reset Default", Button::Small).clicked() {
                        reset_now = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close_now = true;
        }

        if let Some(rgb) = new_rgb {
            self.color_picker_rgb = rgb;
            if !self.color_picker_undo_pushed {
                self.push_undo();
                self.color_picker_undo_pushed = true;
            }
            let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
            for id in ids {
                self.job.set_instance_color(id, Some(rgb));
            }
            self.mark_dirty();
            self.status = format!("Set color #{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
        }

        if reset_now {
            self.set_selected_color(None);
            self.color_picker_open = false;
            self.color_picker_undo_pushed = false;
        } else if close_now || !open {
            self.color_picker_open = false;
            self.color_picker_undo_pushed = false;
        }
    }

    fn toggle_selected_locked(&mut self) {
        let Some(primary) = self.selected else {
            return;
        };
        self.push_undo();
        let target_locked = !self
            .job
            .instances
            .iter()
            .find(|i| i.id == primary)
            .is_some_and(|i| i.locked);
        let mut changed = 0usize;
        for inst in &mut self.job.instances {
            if self.selected_set.contains(&inst.id) {
                inst.locked = target_locked;
                changed += 1;
            }
        }
        if changed > 0 {
            self.status = if target_locked {
                format!("Locked {changed} instance(s)")
            } else {
                format!("Unlocked {changed} instance(s)")
            };
        }
    }

    fn current_standard_view(&self) -> Option<StandardView> {
        const TOL: f64 = 0.05;
        if let Some(v) = self.camera.aligned_standard_view(TOL) {
            return Some(v);
        }
        let (iso_yaw, iso_pitch) = StandardView::Iso.angles();
        let dyaw = (self.camera.yaw - iso_yaw).rem_euclid(std::f64::consts::TAU);
        let dyaw = dyaw.min(std::f64::consts::TAU - dyaw);
        let dpitch = (self.camera.pitch - iso_pitch).abs();
        let droll = self.camera.roll.rem_euclid(std::f64::consts::TAU);
        let droll = droll.min(std::f64::consts::TAU - droll);
        if dyaw <= TOL && dpitch <= TOL && droll <= TOL {
            Some(StandardView::Iso)
        } else {
            None
        }
    }

    fn set_standard_view(&mut self, view: StandardView) {
        self.camera.set_view(view);
        self.status = format!("View: {}", view_label(view));
    }

    fn cycle_standard_view(&mut self) {
        let next = match self.current_standard_view().and_then(|cur| VIEW_CYCLE.iter().position(|&v| v == cur)) {
            Some(idx) => VIEW_CYCLE[(idx + 1) % VIEW_CYCLE.len()],
            None => StandardView::Iso,
        };
        self.set_standard_view(next);
    }

    fn arrow_nudge_axes(&self) -> ([f32; 3], [f32; 3]) {
        match self.current_standard_view() {
            Some(StandardView::Front | StandardView::Back | StandardView::Left | StandardView::Right) => {
                let (right, _, _) = self.camera.basis();
                let horiz = if right.x.abs() >= right.y.abs() {
                    [right.x.signum() as f32, 0.0, 0.0]
                } else {
                    [0.0, right.y.signum() as f32, 0.0]
                };
                (horiz, [0.0, 0.0, 1.0])
            }
            _ => ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input()
            || self.custom_platform_open
            || self.dup_dialog_open
            || self.move_dialog_open
            || self.scale_dialog_open
            || self.sinterbox_dialog_open
            || self.color_picker_open
            || self.export_stl_dialog_open
            || !self.below_zero_import_dialog.is_empty()
        {
            return;
        }
        ctx.input(|i| {
            let cmd = i.modifiers.command;
            let shift = i.modifiers.shift;
            let alt = i.modifiers.alt;

            if cmd && !shift && i.key_pressed(egui::Key::Z) {
                self.undo();
                return;
            }
            if (cmd && i.key_pressed(egui::Key::Y)) || (cmd && shift && i.key_pressed(egui::Key::Z)) {
                self.redo();
                return;
            }
            if cmd && shift && i.key_pressed(egui::Key::S) {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("NestCraft Bed", &["nestc"])
                    .set_file_name("bed.nestc")
                    .save_file()
                {
                    self.save_bed_file(&path);
                }
                return;
            }
            if cmd && !shift && i.key_pressed(egui::Key::S) {
                self.open_scale_dialog();
                return;
            }
            if cmd && i.key_pressed(egui::Key::D) {
                self.open_duplicate_dialog();
                return;
            }
            if !cmd && !alt && !shift && i.key_pressed(egui::Key::M) {
                self.open_move_dialog();
                return;
            }
            if !cmd && !alt && i.key_pressed(egui::Key::Escape) {
                if self.align_face_target.is_some() {
                    self.clear_align_face_mode();
                    self.status = "Cancelled face alignment".to_owned();
                    return;
                }
                if self.zoom_rect_mode || self.zoom_rect.is_some() {
                    self.zoom_rect_mode = false;
                    self.zoom_rect = None;
                    self.status = "Cancelled zoom to selection".to_owned();
                    return;
                }
            }
            if !cmd && !alt && (i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
                self.delete_selected();
            }
            if cmd && !shift && !alt && i.key_pressed(egui::Key::ArrowDown) {
                let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                if !ids.is_empty() {
                    self.push_undo();
                    let mut dropped = false;
                    for id in ids {
                        dropped |= self.job.drop_instance_to_floor(id);
                    }
                    if dropped {
                        self.mark_dirty();
                        self.status = "Dropped selection to Min Z".to_owned();
                    } else {
                        self.undo_stack.pop();
                    }
                }
                return;
            }
            let rot_step_45 = if shift { -45.0 } else { 45.0 };
            if !cmd && !alt && i.key_pressed(egui::Key::X) {
                self.rotate_selected_by(0, rot_step_45);
            }
            if !cmd && !alt && i.key_pressed(egui::Key::Y) {
                self.rotate_selected_by(1, rot_step_45);
            }
            if !cmd && !alt && i.key_pressed(egui::Key::Z) {
                self.rotate_selected_by(2, rot_step_45);
            }
            if !cmd && !alt && i.key_pressed(egui::Key::C)
                && let Some(sel) = self.selected
            {
                self.push_undo();
                if self.job.center_instance_xy(sel) {
                    self.mark_dirty();
                    self.status = "Centered part in XY".to_owned();
                } else {
                    self.undo_stack.pop();
                }
            }
            if !cmd && i.key_pressed(egui::Key::R) {
                let axis = if shift {
                    0
                } else if alt {
                    1
                } else {
                    2
                };
                self.rotate_selected_90(axis);
            }
            if !cmd && !alt && i.key_pressed(egui::Key::H) {
                self.toggle_selected_hidden();
            }
            if !cmd && !alt && i.key_pressed(egui::Key::L) {
                self.toggle_selected_locked();
            }
            if !cmd && !alt && !shift && i.key_pressed(egui::Key::Space) {
                self.cycle_standard_view();
            }
            if !cmd && !alt && i.key_pressed(egui::Key::F) {
                self.camera = viewport::default_camera_with_origin(&self.job.machine, self.job.bed_origin);
            }

            let step = if shift {
                10.0
            } else if cmd {
                0.1
            } else {
                1.0
            };
            let (horiz, vert) = self.arrow_nudge_axes();
            if i.key_pressed(egui::Key::ArrowLeft) {
                self.nudge_selected([-horiz[0] * step, -horiz[1] * step, -horiz[2] * step]);
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                self.nudge_selected([horiz[0] * step, horiz[1] * step, horiz[2] * step]);
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                self.nudge_selected([vert[0] * step, vert[1] * step, vert[2] * step]);
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                self.nudge_selected([-vert[0] * step, -vert[1] * step, -vert[2] * step]);
            }
            if i.key_pressed(egui::Key::PageUp) {
                self.nudge_selected([0.0, 0.0, step]);
            }
            if i.key_pressed(egui::Key::PageDown) {
                self.nudge_selected([0.0, 0.0, -step]);
            }

            if !cmd && !alt {
                if i.key_pressed(egui::Key::Num1) {
                    self.camera = self.camera.looking_from([0.0, -1.0, 0.0].into());
                } else if i.key_pressed(egui::Key::Num3) {
                    self.camera = self.camera.looking_from([1.0, 0.0, 0.0].into());
                } else if i.key_pressed(egui::Key::Num7) {
                    self.camera = self.camera.looking_from([0.0, 0.0, 1.0].into());
                } else if i.key_pressed(egui::Key::Num0) {
                    self.camera = viewport::default_camera_with_origin(&self.job.machine, self.job.bed_origin);
                }
            }
        });
    }

    fn mark_dirty(&mut self) {
        self.scene_dirty = true;
    }

    fn import_no_build_zone_stl(&mut self, path: &Path) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.status = format!("Failed to read {}: {e}", path.display());
                return;
            }
        };
        let mesh = match stl::parse_stl(&bytes) {
            Ok(m) => Arc::new(m),
            Err(e) => {
                self.status = format!("No-Build Zone STL error: {e}");
                return;
            }
        };
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("No-Build Zone")
            .to_owned();
        let zone = NoBuildZone::new(name.clone(), mesh);
        if let Some(dir) = no_build_zone_dir_for_slot(self.active_machine_slot) {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                self.status = format!("Failed to create {}: {e}", dir.display());
                return;
            }
            if let Err(e) = std::fs::write(dir.join("zone.stl"), &bytes) {
                self.status = format!("Failed to save {}: {e}", dir.display());
                return;
            }
            if let Err(e) = save_no_build_zone_config_to_dir(&dir, &zone) {
                self.status = e;
                return;
            }
        }
        self.job.no_build_zone = Some(zone);
        self.mark_dirty();
        self.status = format!("Saved No-Build Zone '{name}'");
    }

    fn persist_no_build_zone(&mut self) {
        if let (Some(dir), Some(zone)) = (no_build_zone_dir_for_slot(self.active_machine_slot), &self.job.no_build_zone)
            && let Err(e) = save_no_build_zone_config_to_dir(&dir, zone)
        {
            self.status = e;
        }
    }

    fn save_bed_file(&mut self, path: &Path) {
        match nestc::write_nestc(&self.job) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, bytes) {
                    self.status = format!("Save bed failed: {e}");
                } else {
                    self.status = format!("Saved bed to {}", path.display());
                }
            }
            Err(e) => {
                self.status = format!("Save bed error: {e}");
            }
        }
    }

    fn apply_loaded_bed(&mut self, job: BuildJob, path_display: &str) {
        self.job = job;
        self.selected = None;
        self.selected_set.clear();
        self.selection_anchor = None;
        self.euler_edit.clear();
        self.scene_cache = viewport::SceneCache::default();
        self.drag_state = None;
        self.drag_zone = None;
        self.moving_target = None;
        self.clear_align_face_mode();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.manifold_cache.clear();
        self.part_thumbs.clear();
        self.expanded_parts.clear();
        self.camera = viewport::default_camera_with_origin(&self.job.machine, self.job.bed_origin);
        self.mark_dirty();
        self.status = format!("Opened bed {path_display}");
    }

    #[cfg(test)]
    fn open_bed_file(&mut self, path: &Path) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.status = format!("Open bed failed: {e}");
                return;
            }
        };
        match nestc::read_nestc(&bytes) {
            Ok(job) => {
                let display = path.display().to_string();
                self.apply_loaded_bed(job, &display);
            }
            Err(e) => {
                self.status = format!("Open bed error: {e}");
            }
        }
    }

    fn start_import_files(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        if paths.is_empty() {
            return;
        }
        let total = paths.len();
        let (tx, rx) = mpsc::channel();
        let ctx_bg = ctx.clone();
        self.load_task = Some(ActiveLoadTask {
            rx,
            completed: 0,
            total,
            current_label: format!("Starting import ({total} file(s))..."),
            imported_files: 0,
            imported_items: 0,
            below_zero_instances: Vec::new(),
            pushed_undo: false,
            errors: Vec::new(),
        });

        let workers = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(total)
            .clamp(1, 16);
        let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
        let completed = Arc::new(AtomicUsize::new(0));

        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let completed = Arc::clone(&completed);
            let tx = tx.clone();
            let ctx_bg = ctx_bg.clone();
            std::thread::spawn(move || {
                loop {
                    let next_path = queue.lock().ok().and_then(|mut q| q.pop_front());
                    let Some(path) = next_path else {
                        break;
                    };
                    let file_name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("file")
                        .to_owned();
                    let outcome = load_single_file_outcome(&path);
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = tx.send(LoadProgressMsg {
                        completed: done,
                        total,
                        current_label: format!("Loaded {file_name}"),
                        outcome: Some(outcome),
                    });
                    ctx_bg.request_repaint();
                }
            });
        }
    }

    fn start_open_bed(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.start_import_files(vec![path], ctx);
    }

    fn poll_load_task(&mut self, ctx: &egui::Context) {
        let Some(mut task) = self.load_task.take() else {
            return;
        };
        let mut disconnected = false;
        let mut opened_bed = false;
        let mut last_imported_id = None;
        let mut any_imported = false;
        loop {
            match task.rx.try_recv() {
                Ok(msg) => {
                    task.completed = msg.completed.max(task.completed);
                    task.total = msg.total;
                    task.current_label = msg.current_label;
                    if let Some(outcome) = msg.outcome {
                        match outcome {
                            BackgroundLoadOutcome::Imported { name, kind } => {
                                if !task.pushed_undo {
                                    self.push_undo();
                                    task.pushed_undo = true;
                                }
                                task.imported_files += 1;
                                match kind {
                                    ImportedKind::Stl(mesh, shared_tris) => {
                                        let pid = self.job.add_part(name, mesh);
                                        self.scene_cache.parts.insert(pid, shared_tris);
                                        if let Ok(iid) = self.job.add_instance(pid, IDENTITY_TRANSFORM) {
                                            if let Some(min_z) = self.job.instance_min_z(iid)
                                                && min_z < -1e-4
                                            {
                                                task.below_zero_instances.push((iid, min_z));
                                            }
                                            last_imported_id = Some(iid);
                                            task.imported_items += 1;
                                            any_imported = true;
                                        }
                                    }
                                    ImportedKind::ThreeMf(items) => {
                                        for (item, shared_tris) in items {
                                            let pid = self.job.add_part(item.name, item.mesh);
                                            self.scene_cache.parts.insert(pid, shared_tris);
                                            if let Ok(iid) = self.job.add_instance(pid, item.transform) {
                                                if let Some(min_z) = self.job.instance_min_z(iid)
                                                    && min_z < -1e-4
                                                {
                                                    task.below_zero_instances.push((iid, min_z));
                                                }
                                                last_imported_id = Some(iid);
                                                task.imported_items += 1;
                                                any_imported = true;
                                            }
                                        }
                                    }
                                }
                            }
                            BackgroundLoadOutcome::OpenedBed { job, path_display } => {
                                self.apply_loaded_bed(job, &path_display);
                                opened_bed = true;
                            }
                            BackgroundLoadOutcome::Failed(err) => {
                                task.errors.push(err);
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if let Some(iid) = last_imported_id {
            self.select_instance(Some(iid));
        } else if any_imported {
            self.mark_dirty();
        }

        if disconnected || task.completed >= task.total {
            if !opened_bed {
                if !task.below_zero_instances.is_empty() {
                    self.below_zero_import_dialog.extend(task.below_zero_instances);
                }
                if task.imported_items > 0 {
                    self.status = if task.errors.is_empty() {
                        format!("Imported {} item(s) from {} file(s)", task.imported_items, task.imported_files)
                    } else {
                        format!(
                            "Imported {} item(s) from {} file(s); {} failed ({})",
                            task.imported_items,
                            task.imported_files,
                            task.errors.len(),
                            task.errors[0]
                        )
                    };
                } else if let Some(first_err) = task.errors.first() {
                    self.status = first_err.clone();
                }
            }
            self.load_task = None;
        } else {
            ctx.request_repaint();
            self.load_task = Some(task);
        }
    }

    fn export_individual_stls(&self, dir: &Path) -> Result<usize, String> {
        let instances: Vec<&nestcraft_core::model::PartInstance> = self
            .job
            .instances
            .iter()
            .filter(|inst| {
                if self.export_stl_selected_only {
                    self.selected_set.contains(&inst.id)
                } else {
                    self.job.is_instance_visible(inst)
                }
            })
            .collect();
        if instances.is_empty() {
            return Err("No matching parts to export".to_owned());
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
        let mut used_names: HashSet<String> = HashSet::new();
        let mut count = 0usize;
        for inst in instances {
            let Some(proto) = self.job.part(inst.part_id) else {
                continue;
            };
            let mut stem = format_export_stl_stem(
                &self.export_stl_prefix,
                &proto.name,
                inst.id.0,
                self.export_stl_include_id,
                &self.export_stl_suffix,
            );
            if !used_names.insert(stem.to_ascii_lowercase()) {
                let mut n = 2usize;
                loop {
                    let candidate = format!("{stem}_{n}");
                    if used_names.insert(candidate.to_ascii_lowercase()) {
                        stem = candidate;
                        break;
                    }
                    n += 1;
                }
            }
            let tf = self.export_stl_apply_transform.then_some(&inst.transform);
            let bytes = stl::write_binary_stl(&proto.mesh, tf).map_err(|e| format!("STL write error: {e}"))?;
            let out_path = dir.join(format!("{stem}.stl"));
            std::fs::write(&out_path, bytes)
                .map_err(|e| format!("Failed to write {}: {e}", out_path.display()))?;
            count += 1;
        }
        Ok(count)
    }

    fn clear_no_build_zone(&mut self) {
        self.job.no_build_zone = None;
        if let Some(dir) = no_build_zone_dir_for_slot(self.active_machine_slot) {
            let _ = std::fs::remove_file(dir.join("zone.stl"));
            let _ = std::fs::remove_file(dir.join("config.json"));
        }
        self.mark_dirty();
        self.status = "Cleared No-Build Zone".to_owned();
    }

    #[cfg(test)]
    fn import_single_file(&mut self, path: &Path) -> Result<(usize, String), String> {
        let bytes = std::fs::read(path).map_err(|_| format!("Failed to read {}", path.display()))?;
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();

        if ext == "nestc" {
            self.open_bed_file(path);
            Ok((self.job.instances.len(), format!("Opened bed {}", path.display())))
        } else if ext == "3mf" {
            let items = threemf::read_threemf(&bytes).map_err(|e| format!("3MF import error: {e}"))?;
            let count = items.len();
            for item in items {
                let pid = self.job.add_part(item.name, item.mesh);
                if let Ok(iid) = self.job.add_instance(pid, item.transform) {
                    if let Some(min_z) = self.job.instance_min_z(iid)
                        && min_z < -1e-4
                    {
                        self.below_zero_import_dialog.push((iid, min_z));
                    }
                    self.select_instance(Some(iid));
                }
            }
            Ok((count, format!("Imported {count} build item(s) from {}", path.display())))
        } else if ext == "stl" {
            let mesh = stl::parse_stl(&bytes).map_err(|e| format!("STL import error: {e}"))?;
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("part")
                .to_owned();
            let pid = self.job.add_part(name.clone(), Arc::new(mesh));
            if let Ok(iid) = self.job.add_instance(pid, IDENTITY_TRANSFORM) {
                if let Some(min_z) = self.job.instance_min_z(iid)
                    && min_z < -1e-4
                {
                    self.below_zero_import_dialog.push((iid, min_z));
                }
                self.select_instance(Some(iid));
            }
            Ok((1, format!("Imported '{name}'")))
        } else {
            Err(format!("Unsupported file '{}' (expected .stl or .3mf)", path.display()))
        }
    }

    #[cfg(test)]
    fn import_files<P: AsRef<Path>>(&mut self, paths: &[P]) {
        if paths.is_empty() {
            return;
        }
        if paths.len() == 1 {
            match self.import_single_file(paths[0].as_ref()) {
                Ok((count, msg)) => {
                    self.status = msg;
                    if count > 0 {
                        self.mark_dirty();
                    }
                }
                Err(err) => self.status = err,
            }
            return;
        }

        let mut imported_files = 0usize;
        let mut imported_items = 0usize;
        let mut errors = Vec::new();

        for path in paths {
            match self.import_single_file(path.as_ref()) {
                Ok((count, _)) => {
                    imported_files += 1;
                    imported_items += count;
                }
                Err(err) => errors.push(err),
            }
        }

        if imported_items > 0 {
            self.mark_dirty();
        }
        self.status = if errors.is_empty() {
            format!("Imported {imported_items} item(s) from {imported_files} file(s)")
        } else if imported_files > 0 {
            format!(
                "Imported {imported_items} item(s) from {imported_files} file(s); {} failed ({})",
                errors.len(),
                errors[0]
            )
        } else {
            format!("Import failed for {} file(s): {}", errors.len(), errors[0])
        };
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        if dropped.is_empty() {
            return;
        }
        let paths: Vec<std::path::PathBuf> = dropped
            .into_iter()
            .map(|f| f.path().to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !paths.is_empty() {
            self.start_import_files(paths, ctx);
        }
    }

    fn draw_drop_overlay(&self, ctx: &egui::Context, rect: egui::Rect, t: &Tokens) {
        let hovered = ctx.input(|i| i.raw.hovered_files.len());
        if hovered == 0 {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("nestcraft_drop_overlay"),
        ));
        let fill = egui::Color32::from_rgba_unmultiplied(t.panel.r(), t.panel.g(), t.panel.b(), 210);
        painter.rect_filled(rect, 0.0, fill);
        painter.rect_stroke(
            rect.shrink(12.0),
            8.0,
            egui::Stroke::new(2.0, t.form_ink),
            egui::StrokeKind::Inside,
        );
        let label = if hovered == 1 {
            "Drop STL or 3MF file to import".to_owned()
        } else {
            format!("Drop {hovered} STL or 3MF files to import")
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(t.font_heading + 4.0),
            t.form_ink,
        );
    }

    fn export_threemf_file(&mut self, path: &Path) {
        match threemf::write_threemf(&self.job) {
            Ok(bytes) => match std::fs::write(path, bytes) {
                Ok(()) => self.status = format!("Exported 3MF to {}", path.display()),
                Err(e) => self.status = format!("Write error: {e}"),
            },
            Err(e) => self.status = format!("3MF export error: {e}"),
        }
    }

    fn start_moving_layer(&mut self, target: viewport::DragExclude) {
        let tokens = Tokens::get();
        let scene = viewport::build_moving_scene(&self.job, &self.selected_set, &tokens, &mut self.scene_cache, &target);
        if let Ok(mut guard) = self.moving_slot.lock() {
            *guard = Some(scene);
        }
        self.moving_key = self.moving_key.wrapping_add(1).max(1);
        self.moving_target = Some(target);
        self.mark_dirty();
    }

    fn current_moving_layer(&self) -> Option<MovingLayer> {
        let target = self.moving_target.as_ref()?;
        let delta = match target {
            viewport::DragExclude::Instances(_) => {
                let ds = self.drag_state.as_ref()?;
                let (_, start_tr) = ds.instances.iter().find(|(id, _)| *id == ds.anchor_id)?;
                let inst = self.job.instances.iter().find(|i| i.id == ds.anchor_id)?;
                let cur = inst.translation();
                [cur[0] - start_tr[0], cur[1] - start_tr[1], cur[2] - start_tr[2]]
            }
            viewport::DragExclude::NoBuildZone => {
                let dz = self.drag_zone?;
                let zone = self.job.no_build_zone.as_ref()?;
                [
                    zone.position[0] - dz.start_position[0],
                    zone.position[1] - dz.start_position[1],
                    zone.position[2] - dz.start_position[2],
                ]
            }
        };
        Some(MovingLayer {
            key: self.moving_key,
            slot: self.moving_slot.clone(),
            transform: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [delta[0], delta[1], delta[2], 1.0],
            ],
        })
    }

    fn sync_scene_if_dirty(&mut self) {
        if !self.scene_dirty {
            return;
        }
        if let Some(viewport::DragExclude::Instances(ids)) = &self.moving_target
            && !ids.iter().any(|id| self.job.instances.iter().any(|i| i.id == *id))
        {
            self.moving_target = None;
            self.drag_state = None;
        }
        let tokens = Tokens::get();
        let scene = viewport::build_gpu_scene_with_exclude(
            &self.job,
            &self.selected_set,
            &tokens,
            &mut self.scene_cache,
            self.moving_target.as_ref(),
        );
        if let Ok(mut guard) = self.scene_slot.lock() {
            *guard = Some(scene);
        }
        self.scene_key = self.scene_key.wrapping_add(1).max(1);
        self.scene_dirty = false;
    }

    fn draw_top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let chrome = Container::new(ctx, Surface::Chrome);
        let top_panel = chrome.edge("sls_top_bar", Edge::Top).exact_size(44.0).resizable(false);
        chrome.show_edge(ui, top_panel, chrome.edge_frame(t.toolbar), |ui| {
            scope(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("sls_top_scroll")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.label(egui::RichText::new("NestCraft").strong().color(t.text));
                            StageBar::separator(ui);

                            let mut machine_idx = self.active_machine_slot.min(2);
                            let machine_changed = ui
                                .push_id("machine_seg", |ui| {
                                    segmented(
                                        ui,
                                        &mut machine_idx,
                                        &["Formiga P110", "Farsoon HT403P", "Custom"],
                                        Segments {
                                            dense: true,
                                            intrinsic: true,
                                            workspace_accent: true,
                                            ..Default::default()
                                        },
                                    )
                                })
                                .inner;
                            if machine_changed {
                                self.apply_machine_slot(machine_idx);
                                if machine_idx == 2 {
                                    self.open_custom_platform_dialog();
                                }
                            }
                            if button(ui, "Configure...", Button::Small)
                                .backing_on_hover_text(
                                    "Configure dimensions, wall distance, Min Z, and clearance for the active machine",
                                )
                                .clicked()
                            {
                                self.open_custom_platform_dialog();
                            }

                            StageBar::separator(ui);
                            if icon_label_button(ui, "Open Bed", "open", self.load_task.is_none(), false)
                                .backing_on_hover_text("Open a saved .nestc bed project")
                                .clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .add_filter("NestCraft Bed", &["nestc"])
                                    .pick_file()
                            {
                                self.start_open_bed(path, ctx);
                            }
                            if icon_label_button(ui, "Save Bed", "save", true, false)
                                .backing_on_hover_text("Save current bed, parts, and settings as .nestc (Ctrl+Shift+S)")
                                .clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .add_filter("NestCraft Bed", &["nestc"])
                                    .set_file_name("bed.nestc")
                                    .save_file()
                            {
                                self.save_bed_file(&path);
                            }
                            if icon_label_button(ui, "Import", "sls_import_mesh", self.load_task.is_none(), false)
                                .backing_on_hover_text("Import STL or 3MF meshes into the build chamber")
                                .clicked()
                                && let Some(paths) = rfd::FileDialog::new()
                                    .add_filter("3D Mesh", &["stl", "STL", "3mf", "3MF", "nestc"])
                                    .pick_files()
                            {
                                self.start_import_files(paths, ctx);
                            }
                            if icon_label_button(ui, "Sample Part", "mesh_body", true, false)
                                .backing_on_hover_text("Insert a sample 30x20x25 mm bracket into the chamber")
                                .clicked()
                            {
                                self.add_sample_part();
                            }
                            if icon_label_button(ui, "Export 3MF", "export", !self.job.instances.is_empty(), false)
                                .backing_on_hover_text("Export build job to Production 3MF")
                                .clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .add_filter("3MF", &["3mf"])
                                    .set_file_name("build.3mf")
                                    .save_file()
                            {
                                self.export_threemf_file(&path);
                            }
                            if icon_label_button(ui, "Export STLs", "export", !self.job.instances.is_empty(), false)
                                .backing_on_hover_text(
                                    "Export parts as individual STL files with configurable prefix/suffix",
                                )
                                .clicked()
                            {
                                self.export_stl_dialog_open = true;
                            }

                            StageBar::separator(ui);
                            icon_label_button(ui, "Auto-Nest", "sls_auto_nest", false, false)
                                .backing_on_hover_text("3D EDT Auto-Nest (coming in Stage 2)");
                            if icon_label_button(ui, "Fit View", "fit", true, false)
                                .backing_on_hover_text("Reset camera to default isometric build view")
                                .clicked()
                            {
                                self.camera =
                                    viewport::default_camera_with_origin(&self.job.machine, self.job.bed_origin);
                            }
                            let dark = solidcraft_theme::is_dark();
                            if icon_label_button(
                                ui,
                                if dark { "Light" } else { "Dark" },
                                if dark { "render_sun" } else { "moon" },
                                true,
                                false,
                            )
                            .backing_on_hover_text("Toggle Dark / Light SolidCraft theme")
                            .clicked()
                            {
                                solidcraft_theme::set_dark(!dark);
                                solidcraft_theme::apply(ctx);
                                self.mark_dirty();
                            }
                            let _ = button(ui, "?", Button::Small).backing_on_hover_text(
                                "Shortcuts:\n\
                                 Ctrl+D: Duplicate   |   Ctrl+Z / Ctrl+Y: Undo / Redo   |   Del: Delete\n\
                                 X / Y / Z: Rotate +45° X / Y / Z (Shift: -45°)   |   R: +90° Z\n\
                                 Ctrl+Down: Drop Z=0   |   C: Center XY   |   H: Hide   |   L: Lock\n\
                                 Arrows / PgUp/PgDn: Nudge 1 mm (Shift: 10 mm, Ctrl: 0.1 mm)\n\
                                 1 / 3 / 7 / 0 / F: Front / Right / Top / Iso / Fit View",
                            );
                        });
                    });
            });
        });
    }

    fn add_aabb_to_bounds(bounds: &mut Bounds, aabb: [[f32; 3]; 2]) {
        bounds.add([f64::from(aabb[0][0]), f64::from(aabb[0][1]), f64::from(aabb[0][2])].into());
        bounds.add([f64::from(aabb[1][0]), f64::from(aabb[1][1]), f64::from(aabb[1][2])].into());
    }

    fn platform_bounds(&self) -> Bounds {
        let ([x0, y0], [x1, y1]) = self.job.bed_xy_bounds();
        let [_, _, h] = self.job.machine.size_mm();
        let mut bounds = Bounds::EMPTY;
        Self::add_aabb_to_bounds(&mut bounds, [[x0, y0, 0.0], [x1, y1, h.max(1.0)]]);
        bounds
    }

    fn all_parts_bounds(&self) -> Bounds {
        let mut bounds = Bounds::EMPTY;
        for inst in &self.job.instances {
            if !self.job.is_instance_visible(inst) {
                continue;
            }
            if let Some(proto) = self.job.part(inst.part_id) {
                Self::add_aabb_to_bounds(&mut bounds, inst.world_bounds(proto.mesh.bounds));
            }
        }
        bounds
    }

    fn selected_parts_bounds(&self) -> Bounds {
        let mut bounds = Bounds::EMPTY;
        for inst in &self.job.instances {
            if !self.selected_set.contains(&inst.id) {
                continue;
            }
            if let Some(proto) = self.job.part(inst.part_id) {
                Self::add_aabb_to_bounds(&mut bounds, inst.world_bounds(proto.mesh.bounds));
            }
        }
        bounds
    }

    fn zoom_to_all_parts(&mut self) {
        let bounds = self.all_parts_bounds();
        if self.camera.fit_aspect(&bounds, self.viewport_aspect) {
            self.status = "Zoomed to all parts".to_owned();
        }
    }

    fn zoom_to_selected(&mut self) {
        let bounds = self.selected_parts_bounds();
        if self.camera.fit_aspect(&bounds, self.viewport_aspect) {
            self.status = "Zoomed to selected part(s)".to_owned();
        }
    }

    fn zoom_to_platform(&mut self) {
        let bounds = self.platform_bounds();
        if self.camera.fit_aspect(&bounds, self.viewport_aspect) {
            self.status = "Zoomed to platform".to_owned();
        }
    }

    fn zoom_to_all(&mut self) {
        let mut bounds = self.platform_bounds();
        let parts = self.all_parts_bounds();
        if !parts.is_empty() {
            bounds.add(parts.min);
            bounds.add(parts.max);
        }
        if let Some(zone) = &self.job.no_build_zone
            && zone.visible
        {
            Self::add_aabb_to_bounds(&mut bounds, zone.world_bounds());
        }
        if self.camera.fit_aspect(&bounds, self.viewport_aspect) {
            self.status = "Zoomed to all".to_owned();
        }
    }

    fn zoom_to_screen_rect(&mut self, viewport_rect: egui::Rect, sel_rect: egui::Rect) {
        let w = f64::from(viewport_rect.width().max(1.0));
        let h = f64::from(viewport_rect.height().max(1.0));
        let half_h = self.camera.half_height();
        let half_w = half_h * (w / h);
        let dx_ndc = f64::from(sel_rect.center().x - viewport_rect.center().x) / (w * 0.5);
        let dy_ndc = -f64::from(sel_rect.center().y - viewport_rect.center().y) / (h * 0.5);
        let (right, up, _) = self.camera.basis();
        self.camera.target = self.camera.target + right * (dx_ndc * half_w) + up * (dy_ndc * half_h);
        let scale = f64::from(sel_rect.width() / viewport_rect.width().max(1.0))
            .max(f64::from(sel_rect.height() / viewport_rect.height().max(1.0)))
            .clamp(0.02, 1.0);
        self.camera.distance = (self.camera.distance * scale).clamp(15.0, 5000.0);
        self.status = "Zoomed to selection rectangle".to_owned();
    }

    fn draw_view_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let chrome = Container::new(ctx, Surface::Chrome);
        let view_panel = chrome.edge("sls_view_bar", Edge::Top).exact_size(36.0).resizable(false);
        chrome.show_edge(ui, view_panel, chrome.edge_frame(t.toolbar), |ui| {
            scope(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("sls_view_scroll")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.label(egui::RichText::new("View").strong().color(t.text));
                            StageBar::separator(ui);

                            let has_visible_parts = self.job.instances.iter().any(|i| self.job.is_instance_visible(i));
                            if icon_label_button(ui, "Zoom to All Parts", "sls_zoom_parts", has_visible_parts, false)
                                .backing_on_hover_text("Frame all visible parts in the viewport")
                                .clicked()
                            {
                                self.zoom_to_all_parts();
                            }
                            if icon_label_button(
                                ui,
                                "Zoom to Selected",
                                "sls_zoom_selected",
                                !self.selected_set.is_empty(),
                                false,
                            )
                            .backing_on_hover_text("Frame the currently selected part(s)")
                            .clicked()
                            {
                                self.zoom_to_selected();
                            }
                            if icon_label_button(ui, "Zoom to Platform", "sls_zoom_platform", true, false)
                                .backing_on_hover_text("Frame the machine build platform")
                                .clicked()
                            {
                                self.zoom_to_platform();
                            }
                            if icon_label_button(ui, "Zoom to All", "sls_zoom_all", true, false)
                                .backing_on_hover_text("Frame the platform and all visible parts")
                                .clicked()
                            {
                                self.zoom_to_all();
                            }
                            if icon_label_button(
                                ui,
                                "Zoom to Selection",
                                "sls_zoom_selection",
                                true,
                                self.zoom_rect_mode,
                            )
                            .backing_on_hover_text(
                                "Toggle rectangle zoom: right-drag a rectangle in the viewport to zoom into it (Esc to cancel)",
                            )
                            .clicked()
                            {
                                self.zoom_rect_mode = !self.zoom_rect_mode;
                                self.zoom_rect = None;
                                self.status = if self.zoom_rect_mode {
                                    "Right-drag a rectangle in the viewport to zoom (Esc to cancel)".to_owned()
                                } else {
                                    "Cancelled zoom to selection".to_owned()
                                };
                            }
                        });
                    });
            });
        });
    }

    fn draw_edit_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let chrome = Container::new(ctx, Surface::Chrome);
        let edit_panel = chrome.edge("sls_edit_bar", Edge::Top).exact_size(36.0).resizable(false);
        chrome.show_edge(ui, edit_panel, chrome.edge_frame(t.toolbar), |ui| {
            scope(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("sls_edit_scroll")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.label(egui::RichText::new("Edit").strong().color(t.text));
                            StageBar::separator(ui);

                            if icon_label_button(ui, "Move", "move", !self.selected_set.is_empty(), false)
                                .backing_on_hover_text("Move selected part(s) by relative offset or absolute position (M)")
                                .clicked()
                            {
                                self.open_move_dialog();
                            }
                            if icon_label_button(ui, "Scale", "resize", !self.selected_set.is_empty(), false)
                                .backing_on_hover_text("Scale selected part(s) uniformly or per-axis (Ctrl+S)")
                                .clicked()
                            {
                                self.open_scale_dialog();
                            }
                            if icon_label_button(ui, "Duplicate", "sls_duplicate", self.selected.is_some(), false)
                                .backing_on_hover_text("Duplicate selected part in X/Y/Z grid (Ctrl+D)")
                                .clicked()
                            {
                                self.open_duplicate_dialog();
                            }
                            if icon_label_button(ui, "Drop Z=0", "sls_orient_lock", self.selected.is_some(), false)
                                .backing_on_hover_text("Drop selected part to the build floor (Z)")
                                .clicked()
                                && let Some(sel) = self.selected
                                && self.job.drop_instance_to_floor(sel)
                            {
                                self.mark_dirty();
                                self.status = "Dropped part to Z = 0".to_owned();
                            }
                            if icon_label_button(ui, "Delete", "delete", self.selected.is_some(), true)
                                .backing_on_hover_text("Remove selected instance from build (Del)")
                                .clicked()
                            {
                                self.delete_selected();
                            }
                            if icon_label_button(
                                ui,
                                "Align",
                                "sls_orient_lock",
                                self.selected.is_some(),
                                self.align_face_target.is_some(),
                            )
                            .backing_on_hover_text("Select a face on the part to snap its normal to -Z (build floor)")
                            .clicked()
                                && let Some(sel) = self.selected
                            {
                                if self.align_face_target.is_some() {
                                    self.clear_align_face_mode();
                                } else {
                                    self.start_align_face_mode(sel);
                                }
                            }
                            if icon_label_button(ui, "Shells to Parts", "mesh_body", !self.selected_set.is_empty(), false)
                                .backing_on_hover_text("Split disconnected STL shells in selected part(s) into separate parts")
                                .clicked()
                            {
                                let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                                self.request_shells_to_parts(ids);
                            }
                            if icon_label_button(ui, "Merge Parts", "sls_auto_nest", self.selected_set.len() >= 2, false)
                                .backing_on_hover_text("Combine selected parts into a single merged mesh part")
                                .clicked()
                            {
                                self.request_merge_selected_parts();
                            }

                            StageBar::separator(ui);

                            const PLACEHOLDERS: [(&str, &str, &str); 6] = [
                                (
                                    "Shell",
                                    "mesh_body",
                                    "Hollow selected part with configurable wall thickness & drain holes (coming soon)",
                                ),
                                (
                                    "Labels",
                                    "sls_duplicate",
                                    "Add embossed or engraved part labels & serial numbers (coming soon)",
                                ),
                                (
                                    "Repair Mesh",
                                    "sls_orient_lock",
                                    "Auto-fix non-manifold edges, holes, and inverted normals (coming soon)",
                                ),
                                (
                                    "Boolean",
                                    "sls_auto_nest",
                                    "Union, subtract, or intersect selected parts (coming soon)",
                                ),
                                (
                                    "Cut / Split",
                                    "resize",
                                    "Slice selected part along a plane with optional alignment pins (coming soon)",
                                ),
                                (
                                    "Offset",
                                    "move",
                                    "Uniform surface offset / tolerance compensation (coming soon)",
                                ),
                            ];

                            for (label, icon, tip) in PLACEHOLDERS {
                                icon_label_button(ui, label, icon, false, false).backing_on_hover_text(tip);
                            }
                        });
                    });
            });
        });
    }

    fn draw_analyze_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let chrome = Container::new(ctx, Surface::Chrome);
        let analyze_panel = chrome.edge("sls_analyze_bar", Edge::Top).exact_size(36.0).resizable(false);
        chrome.show_edge(ui, analyze_panel, chrome.edge_frame(t.toolbar), |ui| {
            scope(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("sls_analyze_scroll")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.label(egui::RichText::new("Analyze").strong().color(t.text));
                            StageBar::separator(ui);

                            const PLACEHOLDERS: [(&str, &str, &str); 7] = [
                                (
                                    "Stair Highlight",
                                    "sls_orient_lock",
                                    "Highlight shallow-angle faces prone to layer stair-stepping (coming soon)",
                                ),
                                (
                                    "Measure",
                                    "resize",
                                    "Measure point-to-point distance, angles, and diameters (coming soon)",
                                ),
                                (
                                    "Wall Thickness",
                                    "mesh_body",
                                    "Check thin walls below critical SLS threshold (coming soon)",
                                ),
                                (
                                    "Up / Downskin",
                                    "sls_duplicate",
                                    "Analyze upward-facing and downward-facing surface angles (coming soon)",
                                ),
                                (
                                    "Shadow Area",
                                    "sls_zoom_all",
                                    "Project XY sintered shadow area and cross-section footprint (coming soon)",
                                ),
                                (
                                    "Collision Check",
                                    "sls_auto_nest",
                                    "Highlight part-to-part clearance violations and overlaps (coming soon)",
                                ),
                                (
                                    "Thermal Map",
                                    "render_sun",
                                    "Estimate sintered powder-bed thermal density hotspots (coming soon)",
                                ),
                            ];

                            for (label, icon, tip) in PLACEHOLDERS {
                                icon_label_button(ui, label, icon, false, false).backing_on_hover_text(tip);
                            }
                        });
                    });
            });
        });
    }

    fn draw_bottom_bar(&self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let chrome = Container::new(ctx, Surface::Chrome);
        let bottom_panel = chrome.edge("sls_status_bar", Edge::Bottom).exact_size(30.0).resizable(false);
        chrome.show_edge(ui, bottom_panel, chrome.edge_frame(t.dialog_footer), |ui| {
            scope(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    if let Some(task) = &self.load_task {
                        let frac = if task.total > 0 {
                            (task.completed as f32 / task.total as f32).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(170.0)
                                .text(format!("{}/{} — {}", task.completed, task.total, task.current_label)),
                        );
                    } else {
                        ui.label(egui::RichText::new(&self.status).size(t.font_small).color(t.text_dim));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let [_, _, wz] = self.job.machine.size_mm();
                        let max_z = self.job.max_z_mm();
                        let layers = (max_z / 0.1).ceil() as u32;
                        let metrics = format!(
                            "Parts: {} ({} unique)   |   Vol: {:.2} cm³   |   Max Z: {:.1} / {:.0} mm ({layers} layers @ 0.10 mm)   |   Density: {:.2}%",
                            self.job.instances.len(),
                            self.job.parts.len(),
                            self.job.total_part_volume_mm3() / 1000.0,
                            max_z,
                            wz,
                            self.job.packing_density_pct(),
                        );
                        ui.label(egui::RichText::new(metrics).size(t.font_small).color(t.text)).backing_on_hover_text(
                            "Build summary: total instances (unique parts) | total sintered volume (cm³) | max Z vs platform height (0.10 mm layers) | occupied cake density = volume / (W × D × max Z)",
                        );
                    });
                });
            });
        });
    }

    fn ensure_part_thumbnails(&mut self, ctx: &egui::Context) {
        let mut generated = 0usize;
        for proto in &self.job.parts {
            if self.part_thumbs.contains_key(&proto.id) {
                continue;
            }
            let img = render_part_thumbnail(&proto.mesh);
            let tex = ctx.load_texture(
                format!("part_thumb_{}", proto.id.0),
                img,
                egui::TextureOptions::LINEAR,
            );
            self.part_thumbs.insert(proto.id, tex);
            generated += 1;
            if generated >= 16 {
                ctx.request_repaint();
                break;
            }
        }
    }

    fn draw_parts_browser(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        enum TreeAction {
            Select(InstanceId),
            ToggleSelect(InstanceId),
            SelectRange(InstanceId),
            ToggleInstanceHidden(InstanceId, bool),
            ToggleInstanceTransparent(InstanceId, bool),
            ToggleInstanceLock(InstanceId, bool),
            SetColor(InstanceId, Option<[u8; 3]>),
            PickColor(InstanceId),
            Duplicate(InstanceId),
            DropToFloor(InstanceId),
            CenterXy(InstanceId),
            ExportParts(InstanceId),
            DeleteInstance(InstanceId),
            SetInstanceGroup(InstanceId, Option<GroupId>),
            GroupSelected(InstanceId),
            UngroupSelected(InstanceId),
            ToggleGroupHidden(GroupId, bool),
            DeleteGroup(GroupId),
            RemoveEmptyGroups,
            SplitShells(PartId),
            ShellsToParts(InstanceId),
            AlignFace(InstanceId),
            OpenScale(InstanceId),
            OpenSinterbox(InstanceId),
            OpenSinterboxGroup(GroupId),
            SelectGroup(GroupId),
            MergeSelected,
        }

        // Pre-populate manifold reports and tiny 20x20 thumbnails so the render pass reads them cleanly.
        let part_ids: Vec<PartId> = self.job.parts.iter().map(|p| p.id).collect();
        for pid in part_ids {
            let _ = self.manifold_report(pid);
        }
        self.ensure_part_thumbnails(ctx);

        let side_c = Container::new(ctx, Surface::Panel);
        let left_panel = side_c.edge("sls_parts_tree", Edge::Left).default_size(300.0);
        let left_frame = side_c
            .edge_frame(t.panel)
            .inner_margin(egui::Margin::symmetric(t.space_10 as i8, t.space_10 as i8));
        side_c.show_edge(ui, left_panel, left_frame, |ui| {
            scope(ui, |ui| {
                ui.label(egui::RichText::new("Build Parts Browser").strong().size(t.font_heading).color(t.form_ink));
                ui.add_space(t.space_4);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_group_name)
                            .desired_width(140.0)
                            .hint_text("New group name..."),
                    );
                    if button(ui, "+ Group", Button::Small)
                        .backing_on_hover_text("Create a project tree group (assigns selected part if any)")
                        .clicked()
                    {
                        let name = std::mem::take(&mut self.new_group_name);
                        if self.selected_set.is_empty() {
                            self.push_undo();
                            self.job.create_group(name);
                            self.status = "Created project group".to_owned();
                        } else {
                            self.group_selected_parts(name);
                        }
                    }
                    let has_empty_groups = self
                        .job
                        .groups
                        .iter()
                        .any(|g| self.job.instances.iter().all(|i| i.group_id != Some(g.id)));
                    if has_empty_groups
                        && button(ui, "Clean", Button::Small)
                            .backing_on_hover_text("Remove empty groups")
                            .clicked()
                    {
                        self.push_undo();
                        let removed = self.job.remove_empty_groups();
                        self.status = format!("Removed {removed} empty group(s)");
                    }
                });
                ui.add_space(t.space_4);
                ui.separator();
                ui.add_space(t.space_4);

                let mut actions: Vec<TreeAction> = Vec::new();
                let mut tree_order: Vec<InstanceId> = Vec::new();
                let footer_h = solidcraft_ui_kit::catalog_footer_height(ui) + t.space_12;
                let avail_h = (ui.available_height() - footer_h).max(120.0);
                let details_h = if self.selected.is_some() {
                    (avail_h * 0.54).clamp(180.0, 420.0)
                } else {
                    76.0
                };
                let scroll_h = (avail_h - details_h - t.space_12).max(60.0);
                ui.spacing_mut().item_spacing.y = t.space_4;
                egui::ScrollArea::vertical()
                    .id_salt("sls_parts_tree_scroll")
                    .max_height(scroll_h)
                    .show(ui, |ui| {
                        if self.job.parts.is_empty() && self.job.groups.is_empty() {
                            status_line(ui, "No parts loaded yet. Import an STL/3MF or add a Sample Part.", StatusTone::Dim);
                        }

                        if !self.job.groups.is_empty() {
                            for group in &self.job.groups {
                                let members: Vec<_> = self
                                    .job
                                    .instances
                                    .iter()
                                    .filter(|i| i.group_id == Some(group.id))
                                    .collect();
                                let header_resp = ui.horizontal(|ui| {
                                    let vis_label = if group.hidden { "○" } else { "●" };
                                    if ui
                                        .small_button(vis_label)
                                        .on_hover_text(if group.hidden { "Show group" } else { "Hide group" })
                                        .clicked()
                                    {
                                        actions.push(TreeAction::ToggleGroupHidden(group.id, !group.hidden));
                                    }
                                    ui.label(
                                        egui::RichText::new(format!("{} ({})", group.name, members.len()))
                                            .strong()
                                            .color(if group.hidden { t.form_dim } else { t.form_ink }),
                                    );
                                });
                                header_resp.response.context_menu(|ui| {
                                    if ui.button("Select Group").clicked() {
                                        actions.push(TreeAction::SelectGroup(group.id));
                                        ui.close();
                                    }
                                    if icon_label_button(ui, "Create Sinterbox for Group...", "sls_sinterbox", !members.is_empty(), false).clicked() {
                                        actions.push(TreeAction::OpenSinterboxGroup(group.id));
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.button(if group.hidden { "Show Group" } else { "Hide Group" }).clicked() {
                                        actions.push(TreeAction::ToggleGroupHidden(group.id, !group.hidden));
                                        ui.close();
                                    }
                                    if ui.button("Ungroup & Delete").clicked() {
                                        actions.push(TreeAction::DeleteGroup(group.id));
                                        ui.close();
                                    }
                                    if ui.button("Remove Empty Groups").clicked() {
                                        actions.push(TreeAction::RemoveEmptyGroups);
                                        ui.close();
                                    }
                                });
                                ui.indent(("grp", group.id.0), |ui| {
                                    if members.is_empty() {
                                        status_line(ui, "Empty group (right-click a part to assign)", StatusTone::Dim);
                                    }
                                    for inst in members {
                                        tree_order.push(inst.id);
                                        let part_name = self
                                            .job
                                            .part(inst.part_id)
                                            .map(|p| p.name.as_str())
                                            .unwrap_or("Part");
                                        let is_sel = self.selected_set.contains(&inst.id);
                                        let above_floor = self.job.is_instance_above_floor(inst.id);
                                        let in_nbz = self.job.is_instance_in_no_build_zone(inst.id);
                                        let mut tag = String::new();
                                        if inst.hidden {
                                            tag.push_str(" [HIDDEN]");
                                        }
                                        if inst.transparent {
                                            tag.push_str(" [50%]");
                                        }
                                        if inst.locked {
                                            tag.push_str(" [LOCK]");
                                        }
                                        if !above_floor {
                                            tag.push_str(" [Z<0]");
                                        }
                                        if in_nbz {
                                            tag.push_str(" [NBZ]");
                                        }
                                        let label = format!("#{} {part_name}{tag}", inst.id.0);
                                        let rich = if !above_floor || in_nbz {
                                            egui::RichText::new(label).color(t.form_error)
                                        } else if inst.hidden || group.hidden {
                                            egui::RichText::new(label).color(t.form_dim)
                                        } else {
                                            egui::RichText::new(label)
                                        };
                                        let resp = ui.selectable_label(is_sel, rich);
                                        if resp.clicked() {
                                            let mods = ui.input(|i| i.modifiers);
                                            if mods.shift {
                                                actions.push(TreeAction::SelectRange(inst.id));
                                            } else if mods.command {
                                                actions.push(TreeAction::ToggleSelect(inst.id));
                                            } else {
                                                actions.push(TreeAction::Select(inst.id));
                                            }
                                        }
                                        resp.context_menu(|ui| {
                                            if ui.button("Duplicate").clicked() {
                                                actions.push(TreeAction::Duplicate(inst.id));
                                                ui.close();
                                            }
                                            if ui.button("Drop to Z = 0").clicked() {
                                                actions.push(TreeAction::DropToFloor(inst.id));
                                                ui.close();
                                            }
                                            if ui.button("Center XY").clicked() {
                                                actions.push(TreeAction::CenterXy(inst.id));
                                                ui.close();
                                            }
                                            ui.separator();
                                            if ui.button(if inst.hidden { "Show" } else { "Hide" }).clicked() {
                                                actions.push(TreeAction::ToggleInstanceHidden(inst.id, !inst.hidden));
                                                ui.close();
                                            }
                                            if ui
                                                .button(if inst.transparent {
                                                    "Opaque"
                                                } else {
                                                    "Transparent (50%)"
                                                })
                                                .clicked()
                                            {
                                                actions.push(TreeAction::ToggleInstanceTransparent(
                                                    inst.id,
                                                    !inst.transparent,
                                                ));
                                                ui.close();
                                            }
                                            if ui.button(if inst.locked { "Unlock" } else { "Lock" }).clicked() {
                                                actions.push(TreeAction::ToggleInstanceLock(inst.id, !inst.locked));
                                                ui.close();
                                            }
                                            if let Some(choice) = show_color_submenu(ui) {
                                                match choice {
                                                    ColorMenuChoice::OpenPicker => {
                                                        actions.push(TreeAction::PickColor(inst.id));
                                                    }
                                                    ColorMenuChoice::Set(c) => {
                                                        actions.push(TreeAction::SetColor(inst.id, c));
                                                    }
                                                }
                                            }
                                            if ui.button("Group Selected").clicked() {
                                                actions.push(TreeAction::GroupSelected(inst.id));
                                                ui.close();
                                            }
                                            if ui.button("Ungroup").clicked() {
                                                actions.push(TreeAction::UngroupSelected(inst.id));
                                                ui.close();
                                            }
                                            ui.separator();
                                            if icon_label_button(ui, "Align Face to Z", "sls_orient_lock", true, false).clicked() {
                                        actions.push(TreeAction::AlignFace(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Scale... (Ctrl+S)").clicked() {
                                        actions.push(TreeAction::OpenScale(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Shells to Parts").clicked() {
                                                actions.push(TreeAction::ShellsToParts(inst.id));
                                                ui.close();
                                            }
                                            if ui
                                                .add_enabled(
                                                    self.selected_set.len() >= 2,
                                                    egui::Button::new("Merge Selected Parts"),
                                                )
                                                .clicked()
                                            {
                                                actions.push(TreeAction::MergeSelected);
                                                ui.close();
                                            }
                                            if icon_label_button(ui, "Create Sinterbox...", "sls_sinterbox", true, false).clicked() {
                                                actions.push(TreeAction::OpenSinterbox(inst.id));
                                                ui.close();
                                            }
                                            let export_count = if is_sel { self.selected_set.len() } else { 1 };
                                            if ui.button(export_part_menu_label(export_count)).clicked() {
                                                actions.push(TreeAction::ExportParts(inst.id));
                                                ui.close();
                                            }
                                            if ui.button("Delete").clicked() {
                                                actions.push(TreeAction::DeleteInstance(inst.id));
                                                ui.close();
                                            }
                                        });
                                    }
                                });
                            }
                            ui.separator();
                        }

                        let mut toggle_expand: Option<PartId> = None;
                        for proto in &self.job.parts {
                            let instances: Vec<&nestcraft_core::model::PartInstance> = self
                                .job
                                .instances
                                .iter()
                                .filter(|i| i.part_id == proto.id && i.group_id.is_none())
                                .collect();
                            if instances.is_empty() {
                                continue;
                            }
                            let count = instances.len();
                            let [dx, dy, dz] = proto.mesh.dimensions();
                            let tris = proto.mesh.indices.len();
                            let vol_cm3 = proto.mesh.volume_mm3 / 1000.0;
                            let report_info = self.manifold_cache.get(&proto.id).map(|report| {
                                let ok = report.is_watertight_manifold();
                                let line = if ok {
                                    format!("Watertight ({} shell)", report.shell_count)
                                } else {
                                    format!(
                                        "{} open · {} flipped · {} shells",
                                        report.boundary_edge_count, report.flipped_edge_count, report.shell_count
                                    )
                                };
                                (ok, report.shell_count > 1, line)
                            });
                            let tooltip = match &report_info {
                                Some((_, _, summary)) => format!(
                                    "{}\n{tris} tris · {dx:.1}×{dy:.1}×{dz:.1} mm · {vol_cm3:.2} cm³\n{summary}",
                                    proto.name
                                ),
                                None => format!(
                                    "{}\n{tris} tris · {dx:.1}×{dy:.1}×{dz:.1} mm · {vol_cm3:.2} cm³",
                                    proto.name
                                ),
                            };

                            if count == 1 {
                                let inst = instances[0];
                                tree_order.push(inst.id);
                                let is_sel = self.selected_set.contains(&inst.id);
                                let above_floor = self.job.is_instance_above_floor(inst.id);
                                let in_nbz = self.job.is_instance_in_no_build_zone(inst.id);
                                let visible = self.job.is_instance_visible(inst);
                                let mut tag = String::new();
                                if inst.hidden {
                                    tag.push_str(" [H]");
                                }
                                if inst.transparent {
                                    tag.push_str(" [50%]");
                                }
                                if inst.locked {
                                    tag.push_str(" [L]");
                                }
                                if !above_floor {
                                    tag.push_str(" [Z<0]");
                                }
                                if in_nbz {
                                    tag.push_str(" [NBZ]");
                                }
                                if report_info.as_ref().is_some_and(|(ok, _, _)| !ok) {
                                    tag.push_str(" [!]");
                                }
                                let label = format!("#{} {}{tag}", inst.id.0, proto.name);
                                let rich = if !above_floor || in_nbz {
                                    egui::RichText::new(label).size(t.font_small).color(t.form_error)
                                } else if !visible {
                                    egui::RichText::new(label).size(t.font_small).color(t.form_dim)
                                } else {
                                    egui::RichText::new(label).size(t.font_small)
                                };
                                let thumb_tint = if let Some([r, g, b]) = inst.color {
                                    egui::Color32::from_rgb(r, g, b)
                                } else if !visible {
                                    t.form_dim
                                } else {
                                    t.form_ink
                                };
                                let resp = ui
                                    .horizontal(|ui| {
                                        if ui
                                            .small_button(if inst.hidden { "○" } else { "●" })
                                            .on_hover_text(if inst.hidden { "Show" } else { "Hide" })
                                            .clicked()
                                        {
                                            actions.push(TreeAction::ToggleInstanceHidden(inst.id, !inst.hidden));
                                        }
                                        if let Some(tex) = self.part_thumbs.get(&proto.id) {
                                            ui.add(
                                                egui::Image::new(tex)
                                                    .fit_to_exact_size(egui::vec2(18.0, 18.0))
                                                    .tint(thumb_tint),
                                            );
                                        }
                                        ui.selectable_label(is_sel, rich)
                                    })
                                    .inner
                                    .on_hover_text(&tooltip);
                                if resp.clicked() {
                                    let mods = ui.input(|i| i.modifiers);
                                    if mods.shift {
                                        actions.push(TreeAction::SelectRange(inst.id));
                                    } else if mods.command {
                                        actions.push(TreeAction::ToggleSelect(inst.id));
                                    } else {
                                        actions.push(TreeAction::Select(inst.id));
                                    }
                                }
                                resp.context_menu(|ui| {
                                    if ui.button("Duplicate").clicked() {
                                        actions.push(TreeAction::Duplicate(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Drop to Z = 0").clicked() {
                                        actions.push(TreeAction::DropToFloor(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Center XY").clicked() {
                                        actions.push(TreeAction::CenterXy(inst.id));
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.button(if inst.hidden { "Show" } else { "Hide" }).clicked() {
                                        actions.push(TreeAction::ToggleInstanceHidden(inst.id, !inst.hidden));
                                        ui.close();
                                    }
                                    if ui
                                        .button(if inst.transparent {
                                            "Opaque"
                                        } else {
                                            "Transparent (50%)"
                                        })
                                        .clicked()
                                    {
                                        actions.push(TreeAction::ToggleInstanceTransparent(
                                            inst.id,
                                            !inst.transparent,
                                        ));
                                        ui.close();
                                    }
                                    if ui.button(if inst.locked { "Unlock" } else { "Lock" }).clicked() {
                                        actions.push(TreeAction::ToggleInstanceLock(inst.id, !inst.locked));
                                        ui.close();
                                    }
                                    if let Some(choice) = show_color_submenu(ui) {
                                        match choice {
                                            ColorMenuChoice::OpenPicker => {
                                                actions.push(TreeAction::PickColor(inst.id));
                                            }
                                            ColorMenuChoice::Set(c) => {
                                                actions.push(TreeAction::SetColor(inst.id, c));
                                            }
                                        }
                                    }
                                    if ui.button("Group Selected").clicked() {
                                        actions.push(TreeAction::GroupSelected(inst.id));
                                        ui.close();
                                    }
                                    if inst.group_id.is_some() && ui.button("Ungroup").clicked() {
                                        actions.push(TreeAction::UngroupSelected(inst.id));
                                        ui.close();
                                    }
                                    if !self.job.groups.is_empty() {
                                        ui.menu_button("Assign to Group", |ui| {
                                            for g in &self.job.groups {
                                                if ui.button(&g.name).clicked() {
                                                    actions.push(TreeAction::SetInstanceGroup(inst.id, Some(g.id)));
                                                    ui.close();
                                                }
                                            }
                                            if inst.group_id.is_some() && ui.button("Ungroup").clicked() {
                                                actions.push(TreeAction::SetInstanceGroup(inst.id, None));
                                                ui.close();
                                            }
                                        });
                                    }
                                    ui.separator();
                                    if icon_label_button(ui, "Align Face to Z", "sls_orient_lock", true, false).clicked() {
                                        actions.push(TreeAction::AlignFace(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Scale... (Ctrl+S)").clicked() {
                                        actions.push(TreeAction::OpenScale(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Shells to Parts").clicked() {
                                        actions.push(TreeAction::ShellsToParts(inst.id));
                                        ui.close();
                                    }
                                    if ui
                                        .add_enabled(
                                            self.selected_set.len() >= 2,
                                            egui::Button::new("Merge Selected Parts"),
                                        )
                                        .clicked()
                                    {
                                        actions.push(TreeAction::MergeSelected);
                                        ui.close();
                                    }
                                    if icon_label_button(ui, "Create Sinterbox...", "sls_sinterbox", true, false).clicked() {
                                        actions.push(TreeAction::OpenSinterbox(inst.id));
                                        ui.close();
                                    }
                                    let export_count = if is_sel { self.selected_set.len() } else { 1 };
                                    if ui.button(export_part_menu_label(export_count)).clicked() {
                                        actions.push(TreeAction::ExportParts(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Delete").clicked() {
                                        actions.push(TreeAction::DeleteInstance(inst.id));
                                        ui.close();
                                    }
                                });
                                continue;
                            }

                            let expanded = self.expanded_parts.contains(&proto.id);
                            let header_resp = ui.horizontal(|ui| {
                                let arrow = if expanded { "▾" } else { "▸" };
                                if ui.small_button(arrow).clicked() {
                                    toggle_expand = Some(proto.id);
                                }
                                if let Some(tex) = self.part_thumbs.get(&proto.id) {
                                    ui.add(
                                        egui::Image::new(tex)
                                            .fit_to_exact_size(egui::vec2(18.0, 18.0))
                                            .tint(t.form_ink),
                                    );
                                }
                                let title = format!("{} (×{count})", proto.name);
                                let mut text = egui::RichText::new(title).strong().size(t.font_small);
                                if report_info.as_ref().is_some_and(|(ok, _, _)| !ok) {
                                    text = text.color(t.form_error);
                                } else {
                                    text = text.color(t.form_ink);
                                }
                                if ui.selectable_label(false, text).on_hover_text(&tooltip).clicked() {
                                    toggle_expand = Some(proto.id);
                                }
                                if report_info.as_ref().is_some_and(|(_, multi_shell, _)| *multi_shell)
                                    && ui.small_button("Split").on_hover_text("Split shells into parts").clicked()
                                {
                                    actions.push(TreeAction::SplitShells(proto.id));
                                }
                            });
                            let _ = header_resp;

                            if expanded {
                                ui.indent(("proto_insts", proto.id.0), |ui| {
                                    for inst in instances {
                                        tree_order.push(inst.id);
                                        let is_sel = self.selected_set.contains(&inst.id);
                                        let above_floor = self.job.is_instance_above_floor(inst.id);
                                        let in_nbz = self.job.is_instance_in_no_build_zone(inst.id);
                                        let visible = self.job.is_instance_visible(inst);
                                        let pos = inst.translation();
                                        let mut tag = String::new();
                                        if inst.hidden {
                                            tag.push_str(" [H]");
                                        }
                                        if inst.transparent {
                                            tag.push_str(" [50%]");
                                        }
                                        if inst.locked {
                                            tag.push_str(" [L]");
                                        }
                                        if !above_floor {
                                            tag.push_str(" [Z<0]");
                                        }
                                        if in_nbz {
                                            tag.push_str(" [NBZ]");
                                        }
                                        let label = format!("#{} ({:.0}, {:.0}, {:.0}){tag}", inst.id.0, pos[0], pos[1], pos[2]);
                                        let rich = if !above_floor || in_nbz {
                                            egui::RichText::new(label).size(t.font_small).color(t.form_error)
                                        } else if !visible {
                                            egui::RichText::new(label).size(t.font_small).color(t.form_dim)
                                        } else {
                                            egui::RichText::new(label).size(t.font_small)
                                        };
                                        let row_resp = ui.horizontal(|ui| {
                                            if ui
                                                .small_button(if inst.hidden { "○" } else { "●" })
                                                .on_hover_text(if inst.hidden { "Show" } else { "Hide" })
                                                .clicked()
                                            {
                                                actions.push(TreeAction::ToggleInstanceHidden(inst.id, !inst.hidden));
                                            }
                                            ui.selectable_label(is_sel, rich)
                                        });
                                        let resp = row_resp.inner;
                                        if resp.clicked() {
                                            let mods = ui.input(|i| i.modifiers);
                                            if mods.shift {
                                                actions.push(TreeAction::SelectRange(inst.id));
                                            } else if mods.command {
                                                actions.push(TreeAction::ToggleSelect(inst.id));
                                            } else {
                                                actions.push(TreeAction::Select(inst.id));
                                            }
                                        }
                                        resp.context_menu(|ui| {
                                            if ui.button("Duplicate").clicked() {
                                                actions.push(TreeAction::Duplicate(inst.id));
                                                ui.close();
                                            }
                                            if ui.button("Drop to Z = 0").clicked() {
                                                actions.push(TreeAction::DropToFloor(inst.id));
                                                ui.close();
                                            }
                                            if ui.button("Center XY").clicked() {
                                                actions.push(TreeAction::CenterXy(inst.id));
                                                ui.close();
                                            }
                                            ui.separator();
                                            if ui.button(if inst.hidden { "Show" } else { "Hide" }).clicked() {
                                                actions.push(TreeAction::ToggleInstanceHidden(inst.id, !inst.hidden));
                                                ui.close();
                                            }
                                            if ui
                                                .button(if inst.transparent {
                                                    "Opaque"
                                                } else {
                                                    "Transparent (50%)"
                                                })
                                                .clicked()
                                            {
                                                actions.push(TreeAction::ToggleInstanceTransparent(
                                                    inst.id,
                                                    !inst.transparent,
                                                ));
                                                ui.close();
                                            }
                                            if ui.button(if inst.locked { "Unlock" } else { "Lock" }).clicked() {
                                                actions.push(TreeAction::ToggleInstanceLock(inst.id, !inst.locked));
                                                ui.close();
                                            }
                                            if let Some(choice) = show_color_submenu(ui) {
                                                match choice {
                                                    ColorMenuChoice::OpenPicker => {
                                                        actions.push(TreeAction::PickColor(inst.id));
                                                    }
                                                    ColorMenuChoice::Set(c) => {
                                                        actions.push(TreeAction::SetColor(inst.id, c));
                                                    }
                                                }
                                            }
                                            if !self.job.groups.is_empty() {
                                                ui.menu_button("Assign to Group", |ui| {
                                                    for g in &self.job.groups {
                                                        if ui.button(&g.name).clicked() {
                                                            actions.push(TreeAction::SetInstanceGroup(inst.id, Some(g.id)));
                                                            ui.close();
                                                        }
                                                    }
                                                    if inst.group_id.is_some() && ui.button("None (Ungroup)").clicked() {
                                                        actions.push(TreeAction::SetInstanceGroup(inst.id, None));
                                                        ui.close();
                                                    }
                                                });
                                            }
                                            ui.separator();
                                            if icon_label_button(ui, "Align Face to Z", "sls_orient_lock", true, false).clicked() {
                                        actions.push(TreeAction::AlignFace(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Scale... (Ctrl+S)").clicked() {
                                        actions.push(TreeAction::OpenScale(inst.id));
                                        ui.close();
                                    }
                                    if ui.button("Shells to Parts").clicked() {
                                                actions.push(TreeAction::ShellsToParts(inst.id));
                                                ui.close();
                                            }
                                            if ui
                                                .add_enabled(
                                                    self.selected_set.len() >= 2,
                                                    egui::Button::new("Merge Selected Parts"),
                                                )
                                                .clicked()
                                            {
                                                actions.push(TreeAction::MergeSelected);
                                                ui.close();
                                            }
                                            if icon_label_button(ui, "Create Sinterbox...", "sls_sinterbox", true, false).clicked() {
                                                actions.push(TreeAction::OpenSinterbox(inst.id));
                                                ui.close();
                                            }
                                            let export_count = if is_sel { self.selected_set.len() } else { 1 };
                                            if ui.button(export_part_menu_label(export_count)).clicked() {
                                                actions.push(TreeAction::ExportParts(inst.id));
                                                ui.close();
                                            }
                                            if ui.button("Delete").clicked() {
                                                actions.push(TreeAction::DeleteInstance(inst.id));
                                                ui.close();
                                            }
                                        });
                                    }
                                });
                            }
                        }
                        if let Some(pid) = toggle_expand
                            && !self.expanded_parts.remove(&pid)
                        {
                            self.expanded_parts.insert(pid);
                        }
                    });

                for action in actions {
                    if !matches!(
                        action,
                        TreeAction::Select(_)
                            | TreeAction::ToggleSelect(_)
                            | TreeAction::SelectRange(_)
                            | TreeAction::ExportParts(_)
                            | TreeAction::SetColor(_, _)
                            | TreeAction::PickColor(_)
                            | TreeAction::ShellsToParts(_)
                            | TreeAction::AlignFace(_)
                            | TreeAction::OpenScale(_)
                            | TreeAction::OpenSinterbox(_)
                            | TreeAction::OpenSinterboxGroup(_)
                            | TreeAction::SelectGroup(_)
                            | TreeAction::MergeSelected
                            | TreeAction::GroupSelected(_)
                            | TreeAction::UngroupSelected(_)
                    ) {
                        self.push_undo();
                    }
                    match action {
                        TreeAction::Select(id) => self.select_instance(Some(id)),
                        TreeAction::ToggleSelect(id) => self.toggle_select_instance(id),
                        TreeAction::SelectRange(id) => self.select_tree_range(&tree_order, id),
                        TreeAction::ToggleInstanceHidden(id, hidden) => {
                            if self.job.set_instance_hidden(id, hidden) {
                                self.mark_dirty();
                                self.status = if hidden {
                                    format!("Hidden instance #{}", id.0)
                                } else {
                                    format!("Shown instance #{}", id.0)
                                };
                            }
                        }
                        TreeAction::ToggleInstanceTransparent(id, transparent) => {
                            if self.job.set_instance_transparent(id, transparent) {
                                self.mark_dirty();
                                self.status = if transparent {
                                    format!("Set instance #{} 50% transparent", id.0)
                                } else {
                                    format!("Set instance #{} opaque", id.0)
                                };
                            }
                        }
                        TreeAction::ToggleInstanceLock(id, locked) => {
                            if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == id) {
                                inst.locked = locked;
                            }
                        }
                        TreeAction::SetColor(id, color) => {
                            if !self.selected_set.contains(&id) {
                                self.select_instance(Some(id));
                            }
                            self.set_selected_color(color);
                        }
                        TreeAction::PickColor(id) => {
                            if !self.selected_set.contains(&id) {
                                self.select_instance(Some(id));
                            }
                            self.open_color_picker();
                        }
                        TreeAction::Duplicate(id) => {
                            if let Ok(ids) = self.job.duplicate_instance(id, 1) {
                                self.select_instance(ids.last().copied());
                                self.mark_dirty();
                                self.status = format!("Duplicated #{}", id.0);
                            }
                        }
                        TreeAction::DropToFloor(id) => {
                            if self.job.drop_instance_to_floor(id) {
                                self.mark_dirty();
                                self.status = format!("Dropped #{} to Z = 0", id.0);
                            }
                        }
                        TreeAction::CenterXy(id) => {
                            if self.job.center_instance_xy(id) {
                                self.mark_dirty();
                                self.status = format!("Centered #{} in XY", id.0);
                            }
                        }
                        TreeAction::ExportParts(id) => {
                            if !self.selected_set.contains(&id) {
                                self.select_instance(Some(id));
                            }
                            self.export_stl_selected_only = true;
                            self.export_stl_dialog_open = true;
                        }
                        TreeAction::DeleteInstance(id) => {
                            if self.job.remove_instance(id) {
                                self.euler_edit.remove(&id);
                                self.selected_set.remove(&id);
                                if self.selected == Some(id) {
                                    self.selected = self.selected_set.iter().copied().next();
                                }
                                self.mark_dirty();
                                self.status = format!("Deleted instance #{}", id.0);
                            }
                        }
                        TreeAction::SetInstanceGroup(id, gid) => {
                            let _ = self.job.set_instance_group(id, gid);
                            self.mark_dirty();
                        }
                        TreeAction::GroupSelected(id) => {
                            if !self.selected_set.contains(&id) {
                                self.select_instance(Some(id));
                            }
                            self.group_selected_parts(String::new());
                        }
                        TreeAction::UngroupSelected(id) => {
                            if !self.selected_set.contains(&id) {
                                self.select_instance(Some(id));
                            }
                            self.ungroup_selected_parts();
                        }
                        TreeAction::ToggleGroupHidden(gid, hidden) => {
                            if self.job.set_group_hidden(gid, hidden) {
                                self.mark_dirty();
                            }
                        }
                        TreeAction::DeleteGroup(gid) => {
                            if self.job.remove_group(gid) {
                                self.mark_dirty();
                                self.status = "Removed project group".to_owned();
                            }
                        }
                        TreeAction::RemoveEmptyGroups => {
                            let removed = self.job.remove_empty_groups();
                            if removed > 0 {
                                self.status = format!("Removed {removed} empty group(s)");
                            } else {
                                self.undo_stack.pop();
                            }
                        }
                        TreeAction::SplitShells(pid) => {
                            self.split_part_shells(pid);
                        }
                        TreeAction::ShellsToParts(id) => {
                            let ids = if self.selected_set.contains(&id) {
                                self.selected_set.iter().copied().collect()
                            } else {
                                vec![id]
                            };
                            self.request_shells_to_parts(ids);
                        }
                        TreeAction::AlignFace(id) => {
                            self.select_instance(Some(id));
                            self.start_align_face_mode(id);
                        }
                        TreeAction::OpenScale(id) => {
                            if !self.selected_set.contains(&id) {
                                self.select_instance(Some(id));
                            }
                            self.open_scale_dialog();
                        }
                        TreeAction::OpenSinterbox(id) => {
                            if !self.selected_set.contains(&id) {
                                self.select_instance(Some(id));
                            }
                            self.open_sinterbox_dialog();
                        }
                        TreeAction::OpenSinterboxGroup(gid) => {
                            self.select_group_instances(gid);
                            self.open_sinterbox_dialog();
                        }
                        TreeAction::SelectGroup(gid) => {
                            self.select_group_instances(gid);
                        }
                        TreeAction::MergeSelected => {
                            self.request_merge_selected_parts();
                        }
                    }
                }

                ui.add_space(t.space_6);
                ui.separator();
                ui.add_space(t.space_6);
                egui::ScrollArea::vertical()
                    .id_salt("sls_part_details_scroll")
                    .max_height(details_h)
                    .show(ui, |ui| {
                        self.draw_selected_part_section(ui, t);
                    });
                ui.add_space(t.space_4);

                catalog_footer_with_tip(
                    ui,
                    &format!("{} instance(s) in chamber", self.job.instances.len()),
                    "Right-click an instance for actions; Shift+drag in viewport for Z.",
                );
            });
        });
    }

    fn draw_custom_platform_modal(&mut self, ctx: &egui::Context) {
        if !self.custom_platform_open {
            return;
        }
        let mut open = self.custom_platform_open;
        let mut apply = false;
        let mut reset_default = false;
        let mut cancel = false;
        let slot = self.active_machine_slot.min(2);
        egui::Window::new("Configure Machine Platform")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                row(ui, "Profile Name", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.custom_name).desired_width(180.0));
                });
                row(ui, "Width X", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.custom_size_mm[0])
                            .range(10.0..=2000.0)
                            .speed(1.0)
                            .suffix(" mm"),
                    );
                });
                row(ui, "Depth Y", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.custom_size_mm[1])
                            .range(10.0..=2000.0)
                            .speed(1.0)
                            .suffix(" mm"),
                    );
                });
                row(ui, "Height Z", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.custom_size_mm[2])
                            .range(10.0..=2000.0)
                            .speed(1.0)
                            .suffix(" mm"),
                    );
                });
                row(ui, "Wall Distance", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.custom_wall_margin)
                            .range(0.0..=50.0)
                            .speed(0.5)
                            .suffix(" mm"),
                    );
                });
                row(ui, "Min Z Floor", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.custom_min_z)
                            .range(0.0..=100.0)
                            .speed(0.5)
                            .suffix(" mm"),
                    );
                });
                row(ui, "Clearance", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.custom_clearance)
                            .range(0.5..=50.0)
                            .speed(0.25)
                            .suffix(" mm"),
                    );
                });
                row(ui, "Origin (0,0,0)", |ui| {
                    let mut orig_idx = match self.custom_bed_origin {
                        BedOrigin::FrontLeft => 0,
                        BedOrigin::Center => 1,
                    };
                    if ui
                        .push_id("cfg_origin_seg", |ui| {
                            segmented(
                                ui,
                                &mut orig_idx,
                                &["Front-Left", "Center"],
                                Segments {
                                    dense: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .inner
                    {
                        self.custom_bed_origin = if orig_idx == 1 {
                            BedOrigin::Center
                        } else {
                            BedOrigin::FrontLeft
                        };
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Apply", Button::Primary).clicked() {
                        apply = true;
                    }
                    if slot < 2 && button(ui, "Reset Default", Button::Small).clicked() {
                        reset_default = true;
                    }
                    if button(ui, "Cancel", Button::Small).clicked() {
                        cancel = true;
                    }
                });
            });
        if reset_default {
            let defaults = default_machine_presets();
            let d = &defaults[slot];
            self.custom_name = d.name.clone();
            self.custom_size_mm = d.size_mm;
            self.custom_wall_margin = d.wall_margin_mm;
            self.custom_min_z = d.min_z_mm;
            self.custom_clearance = d.clearance_mm;
            self.custom_bed_origin = d.bed_origin.into();
        } else if apply {
            let clean_machine = MachineProfile::custom(&self.custom_name, self.custom_size_mm);
            self.machine_presets[slot] = MachinePresetConfig {
                name: clean_machine.name().to_owned(),
                size_mm: clean_machine.size_mm(),
                wall_margin_mm: self.custom_wall_margin,
                min_z_mm: self.custom_min_z,
                clearance_mm: self.custom_clearance,
                bed_origin: self.custom_bed_origin.into(),
            };
            if let Some(dir) = nestcraft_data_dir()
                && let Err(e) = save_machine_presets_to_dir(&dir, &self.machine_presets)
            {
                self.status = e;
            }
            self.apply_machine_slot(slot);
            self.custom_platform_open = false;
        } else if cancel || !open {
            self.custom_platform_open = false;
        }
    }

    fn draw_duplicate_modal(&mut self, ctx: &egui::Context) {
        if !self.dup_dialog_open {
            return;
        }
        if self.selected.is_none() {
            self.dup_dialog_open = false;
            return;
        }
        let mut open = self.dup_dialog_open;
        let mut apply = false;
        let mut cancel = false;

        egui::Window::new("Duplicate Parts")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                let mut total = self.dup_total.max(1);
                let total_changed = row(ui, "Total Count", |ui| {
                    ui.add(egui::DragValue::new(&mut total).range(1..=500).speed(0.2)).changed()
                });
                if total_changed {
                    self.dup_total = total;
                    self.dup_exact_total = true;
                    self.dup_counts = distribute_compact_grid(self.dup_total, self.dup_axes);
                }

                ui.separator();
                let mut axis_changed = false;
                let mut count_changed = false;
                let labels = ["X Axis", "Y Axis", "Z Axis"];
                for (k, label) in labels.iter().enumerate() {
                    row(ui, label, |ui| {
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut self.dup_axes[k], "").changed() {
                                axis_changed = true;
                            }
                            ui.add_enabled_ui(self.dup_axes[k], |ui| {
                                if ui
                                    .add(egui::DragValue::new(&mut self.dup_counts[k]).range(1..=100).prefix("×"))
                                    .changed()
                                {
                                    count_changed = true;
                                }
                                ui.add(
                                    egui::DragValue::new(&mut self.dup_gap_mm[k])
                                        .range(0.0..=100.0)
                                        .speed(0.25)
                                        .prefix("gap ")
                                        .suffix(" mm"),
                                );
                            });
                        });
                    });
                }

                if axis_changed {
                    if !self.dup_axes.iter().any(|&a| a) {
                        self.dup_axes[0] = true;
                    }
                    self.dup_counts = distribute_compact_grid(self.dup_total, self.dup_axes);
                } else if count_changed {
                    self.dup_exact_total = false;
                    let nx = if self.dup_axes[0] { self.dup_counts[0].max(1) } else { 1 };
                    let ny = if self.dup_axes[1] { self.dup_counts[1].max(1) } else { 1 };
                    let nz = if self.dup_axes[2] { self.dup_counts[2].max(1) } else { 1 };
                    self.dup_total = nx * ny * nz;
                }

                let nx = if self.dup_axes[0] { self.dup_counts[0].max(1) } else { 1 };
                let ny = if self.dup_axes[1] { self.dup_counts[1].max(1) } else { 1 };
                let nz = if self.dup_axes[2] { self.dup_counts[2].max(1) } else { 1 };
                let final_total = if self.dup_exact_total { self.dup_total } else { nx * ny * nz };
                let new_copies = final_total.saturating_sub(1);
                status_line(
                    ui,
                    &format!("Grid: {nx} × {ny} × {nz} — creates {new_copies} new copy(s)"),
                    StatusTone::Dim,
                );

                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Duplicate", Button::Primary).clicked() {
                        apply = true;
                    }
                    if button(ui, "Cancel", Button::Small).clicked() {
                        cancel = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            apply = true;
        } else if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }

        if apply {
            self.duplicate_selected();
            self.dup_dialog_open = false;
        } else if cancel || !open {
            self.dup_dialog_open = false;
        }
    }

    fn draw_move_modal(&mut self, ctx: &egui::Context) {
        if !self.move_dialog_open {
            return;
        }
        if self.selected_set.is_empty() {
            self.move_dialog_open = false;
            return;
        }
        let mut open = self.move_dialog_open;
        let mut apply = false;
        let mut cancel = false;

        egui::Window::new("Move Selection")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                let mode_changed = row(ui, "Mode", |ui| {
                    ui.push_id("move_mode_seg", |ui| {
                        segmented(
                            ui,
                            &mut self.move_mode_idx,
                            &["Relative", "Absolute"],
                            Segments {
                                dense: true,
                                ..Default::default()
                            },
                        )
                    })
                    .inner
                });
                if mode_changed {
                    if self.move_mode_idx == 1 {
                        if let Some(primary) = self.selected
                            && let Some(inst) = self.job.instances.iter().find(|i| i.id == primary)
                        {
                            self.move_xyz = inst.translation();
                        }
                    } else {
                        self.move_xyz = [0.0, 0.0, 0.0];
                    }
                }

                let is_abs = self.move_mode_idx == 1;
                let (lx, ly, lz) = if is_abs { ("X", "Y", "Z") } else { ("ΔX", "ΔY", "ΔZ") };
                row(ui, lx, |ui| {
                    ui.add(egui::DragValue::new(&mut self.move_xyz[0]).speed(0.5).suffix(" mm"));
                });
                row(ui, ly, |ui| {
                    ui.add(egui::DragValue::new(&mut self.move_xyz[1]).speed(0.5).suffix(" mm"));
                });
                row(ui, lz, |ui| {
                    ui.add(egui::DragValue::new(&mut self.move_xyz[2]).speed(0.5).suffix(" mm"));
                });

                let count = self.selected_set.len();
                let note = if is_abs && count > 1 {
                    format!("Moves {count} parts together (anchor to target; Z >= Min Z)")
                } else if is_abs {
                    "Moves selected part to target coordinates (Z >= Min Z)".to_owned()
                } else {
                    format!("Offsets {count} selected part(s) by delta (Z >= Min Z)")
                };
                status_line(ui, &note, StatusTone::Dim);

                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Move", Button::Primary).clicked() {
                        apply = true;
                    }
                    if button(ui, "Cancel", Button::Small).clicked() {
                        cancel = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            apply = true;
        } else if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }

        if apply {
            self.apply_move_selection(self.move_mode_idx == 1, self.move_xyz);
            self.move_dialog_open = false;
        } else if cancel || !open {
            self.move_dialog_open = false;
        }
    }

    fn draw_scale_modal(&mut self, ctx: &egui::Context) {
        if !self.scale_dialog_open {
            return;
        }
        let mut open = self.scale_dialog_open;
        let mut apply_factors: Option<[f32; 3]> = None;
        let mut reset_scale = false;
        let mut undo_scale = false;
        let mut cancel = false;

        egui::Window::new("Scale Part(s)")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                let mut axis_mode = usize::from(!self.scale_uniform);
                row(ui, "Axis Mode", |ui| {
                    ui.push_id("scale_axis_mode", |ui| {
                        segmented(
                            ui,
                            &mut axis_mode,
                            &["Uniform", "Per-Axis"],
                            Segments {
                                dense: true,
                                ..Default::default()
                            },
                        )
                    })
                    .inner
                });
                self.scale_uniform = axis_mode == 0;

                row(ui, "Input Mode", |ui| {
                    ui.push_id("scale_input_mode", |ui| {
                        segmented(
                            ui,
                            &mut self.scale_mode_idx,
                            &["Percent (%)", "Absolute (mm)", "Factor (×)"],
                            Segments {
                                dense: true,
                                ..Default::default()
                            },
                        )
                    })
                    .inner
                });

                match self.scale_mode_idx {
                    0 => {
                        if self.scale_uniform {
                            row(ui, "Scale", |ui| {
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut self.scale_pct[0])
                                            .range(0.1..=10000.0)
                                            .speed(1.0)
                                            .suffix(" %"),
                                    )
                                    .changed()
                                {
                                    self.scale_pct[1] = self.scale_pct[0];
                                    self.scale_pct[2] = self.scale_pct[0];
                                }
                            });
                        } else {
                            for (k, lbl) in [(0, "X"), (1, "Y"), (2, "Z")] {
                                row(ui, lbl, |ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut self.scale_pct[k])
                                            .range(0.1..=10000.0)
                                            .speed(1.0)
                                            .suffix(" %"),
                                    );
                                });
                            }
                        }
                    }
                    1 => {
                        for (k, lbl) in [(0, "Size X"), (1, "Size Y"), (2, "Size Z")] {
                            row(ui, lbl, |ui| {
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut self.scale_abs_mm[k])
                                            .range(0.01..=5000.0)
                                            .speed(0.5)
                                            .suffix(" mm"),
                                    )
                                    .changed()
                                    && self.scale_uniform
                                {
                                    let ratio = self.scale_abs_mm[k] / self.scale_base_dims[k].max(1e-4);
                                    for j in 0..3 {
                                        if j != k {
                                            self.scale_abs_mm[j] = (self.scale_base_dims[j] * ratio).max(0.01);
                                        }
                                    }
                                }
                            });
                        }
                    }
                    _ => {
                        if self.scale_uniform {
                            row(ui, "Factor", |ui| {
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut self.scale_factor[0])
                                            .range(0.001..=1000.0)
                                            .speed(0.05)
                                            .suffix(" ×"),
                                    )
                                    .changed()
                                {
                                    self.scale_factor[1] = self.scale_factor[0];
                                    self.scale_factor[2] = self.scale_factor[0];
                                }
                            });
                        } else {
                            for (k, lbl) in [(0, "Factor X"), (1, "Factor Y"), (2, "Factor Z")] {
                                row(ui, lbl, |ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut self.scale_factor[k])
                                            .range(0.001..=1000.0)
                                            .speed(0.05)
                                            .suffix(" ×"),
                                    );
                                });
                            }
                        }
                    }
                }

                row(ui, "Units Quick", |ui| {
                    ui.horizontal(|ui| {
                        if button(ui, "in → mm (×25.4)", Button::Small)
                            .backing_on_hover_text("Convert imperial inches to millimetres (× 25.4)")
                            .clicked()
                        {
                            apply_factors = Some([25.4, 25.4, 25.4]);
                        }
                        if button(ui, "mm → in (÷25.4)", Button::Small)
                            .backing_on_hover_text("Convert millimetres to imperial inches (× 1/25.4)")
                            .clicked()
                        {
                            let inv = 1.0_f32 / 25.4;
                            apply_factors = Some([inv, inv, inv]);
                        }
                    });
                });

                if checkbox_row(ui, "scale_keep_open", "Keep Open (Test Final Dim)", self.scale_keep_open) {
                    self.scale_keep_open = !self.scale_keep_open;
                }
                let cur_dims = self.selected_primary_dims();
                status_line(
                    ui,
                    &format!("Current: {:.2} × {:.2} × {:.2} mm", cur_dims[0], cur_dims[1], cur_dims[2]),
                    StatusTone::Dim,
                );

                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Apply", Button::Primary).clicked() {
                        let f = match self.scale_mode_idx {
                            0 => [
                                self.scale_pct[0] / 100.0,
                                self.scale_pct[1] / 100.0,
                                self.scale_pct[2] / 100.0,
                            ],
                            1 => [
                                self.scale_abs_mm[0] / self.scale_base_dims[0].max(1e-4),
                                self.scale_abs_mm[1] / self.scale_base_dims[1].max(1e-4),
                                self.scale_abs_mm[2] / self.scale_base_dims[2].max(1e-4),
                            ],
                            _ => self.scale_factor,
                        };
                        apply_factors = Some(if self.scale_uniform { [f[0]; 3] } else { f });
                    }
                    if button(ui, "Reset Scale", Button::Small)
                        .backing_on_hover_text("Reset scale inputs and restore original dimensions")
                        .clicked()
                    {
                        reset_scale = true;
                    }
                    if button(ui, "Close", Button::Small).clicked() {
                        cancel = true;
                    }
                });
            });

        if ctx.input(|i| i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z)) {
            undo_scale = true;
        } else if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }

        if undo_scale {
            if self.scale_applied_in_session > 0 && self.undo() {
                self.scale_applied_in_session -= 1;
                let dims = self.selected_primary_dims();
                self.scale_pct = [100.0, 100.0, 100.0];
                self.scale_factor = [1.0, 1.0, 1.0];
                self.scale_abs_mm = dims;
                self.scale_base_dims = dims;
            } else {
                self.scale_dialog_open = false;
                self.scale_initial_snapshot = None;
                self.scale_applied_in_session = 0;
            }
        } else if reset_scale {
            self.reset_scale_in_dialog();
        } else if let Some(factors) = apply_factors {
            self.apply_scale_selection(factors);
            self.scale_applied_in_session += 1;
            if self.scale_keep_open {
                let dims = self.selected_primary_dims();
                self.scale_pct = [100.0, 100.0, 100.0];
                self.scale_factor = [1.0, 1.0, 1.0];
                self.scale_abs_mm = dims;
                self.scale_base_dims = dims;
            } else {
                self.scale_dialog_open = false;
                self.scale_initial_snapshot = None;
                self.scale_applied_in_session = 0;
            }
        } else if cancel || !open {
            self.scale_dialog_open = false;
            self.scale_initial_snapshot = None;
            self.scale_applied_in_session = 0;
        }
    }

    fn draw_sinterbox_modal(&mut self, ctx: &egui::Context) {
        if !self.sinterbox_dialog_open {
            return;
        }
        if self.selected_set.is_empty() {
            self.sinterbox_dialog_open = false;
            return;
        }

        let mut open = self.sinterbox_dialog_open;
        let mut apply = false;
        let mut cancel = false;
        let mut switch_to_dup = false;
        let selected_ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        let single_selected = selected_ids.len() == 1;

        egui::Window::new("Create Sinterbox")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                if single_selected {
                    status_line(
                        ui,
                        "Only 1 part is selected. Multiply into a batch before boxing?",
                        StatusTone::Warning,
                    );
                    row(ui, "Copies", |ui| {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.sinterbox_single_copies)
                                    .range(1..=200)
                                    .speed(0.2)
                                    .prefix("×"),
                            );
                            for preset in [1usize, 4, 6, 9, 12] {
                                let lbl = if preset == 1 {
                                    "1".to_owned()
                                } else {
                                    format!("×{preset}")
                                };
                                let tone = if self.sinterbox_single_copies == preset {
                                    Button::Primary
                                } else {
                                    Button::Small
                                };
                                if button(ui, &lbl, tone).clicked() {
                                    self.sinterbox_single_copies = preset;
                                }
                            }
                        });
                    });
                    if self.sinterbox_single_copies > 1 {
                        let grid = distribute_compact_grid(self.sinterbox_single_copies, [true, true, false]);
                        row(ui, "Part Gap", |ui| {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut self.sinterbox_single_gap)
                                        .range(0.0..=50.0)
                                        .speed(0.25)
                                        .suffix(" mm"),
                                );
                                status_line(
                                    ui,
                                    &format!("Grid: {} × {} × {}", grid[0], grid[1], grid[2]),
                                    StatusTone::Dim,
                                );
                            });
                        });
                    }
                    row(ui, "Custom Grid", |ui| {
                        if button(ui, "Open Duplicate Dialog...", Button::Small).clicked() {
                            switch_to_dup = true;
                        }
                    });
                    ui.separator();
                }

                row(ui, "Cage Shape", |ui| {
                    ui.push_id("sinterbox_shape_seg", |ui| {
                        segmented(
                            ui,
                            &mut self.sinterbox_shape_idx,
                            &["Box", "Tight Fit"],
                            Segments {
                                dense: true,
                                ..Default::default()
                            },
                        )
                    })
                    .inner
                });

                row(ui, "Inner Distance", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.sinterbox_inner_clearance)
                            .range(0.2..=50.0)
                            .speed(0.25)
                            .suffix(" mm"),
                    )
                    .backing_on_hover_text("Clearance between enclosed parts and inner cage walls");
                });

                let sim_grid = (single_selected && self.sinterbox_single_copies > 1).then(|| {
                    (
                        distribute_compact_grid(self.sinterbox_single_copies, [true, true, false]),
                        self.sinterbox_single_gap,
                    )
                });
                let cfg_preview = self.current_sinterbox_config();
                let metrics = sinterbox::preview_sinterbox_metrics(
                    &self.job,
                    &selected_ids,
                    &cfg_preview,
                    sim_grid,
                )
                .ok();

                row(ui, "Window Size", |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.sinterbox_window_mm)
                                    .range(1.0..=100.0)
                                    .speed(0.25)
                                    .suffix(" mm"),
                            )
                            .changed()
                        {
                            self.sinterbox_window_auto = false;
                            if self.sinterbox_bar_auto {
                                self.sinterbox_bar_mm =
                                    sinterbox::recommended_bar_thickness_mm(self.sinterbox_window_mm);
                            }
                        }
                        if let Some(ref m) = metrics
                            && button(ui, "Auto", Button::Small)
                                .backing_on_hover_text("Set diagonal-safe window size from smallest part XYZ")
                                .clicked()
                        {
                            self.sinterbox_window_mm = m.recommended_window_mm;
                            self.sinterbox_window_auto = true;
                            if self.sinterbox_bar_auto {
                                self.sinterbox_bar_mm =
                                    sinterbox::recommended_bar_thickness_mm(self.sinterbox_window_mm);
                            }
                        }
                    });
                });

                if let Some(ref m) = metrics {
                    if m.window_exceeds_safe_limit {
                        status_line(
                            ui,
                            &format!(
                                "Warning: {:.1} mm window diagonal ({:.1} mm) >= smallest part dim ({:.1} mm); part may slip out!",
                                self.sinterbox_window_mm,
                                self.sinterbox_window_mm * std::f32::consts::SQRT_2,
                                m.min_part_dim_mm
                            ),
                            StatusTone::Warning,
                        );
                    } else {
                        status_line(
                            ui,
                            &format!(
                                "Smallest part XYZ: {:.1} mm → Safe window: {:.1} mm",
                                m.min_part_dim_mm, m.recommended_window_mm
                            ),
                            StatusTone::Dim,
                        );
                    }
                }

                row(ui, "Bar Thickness", |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.sinterbox_bar_mm)
                                    .range(0.5..=5.0)
                                    .speed(0.05)
                                    .suffix(" mm"),
                            )
                            .changed()
                        {
                            self.sinterbox_bar_auto = false;
                        }
                        if button(ui, "Auto (0.8–1.0)", Button::Small)
                            .backing_on_hover_text("0.80 mm for small windows (prevents sintering shut) up to 1.00 mm")
                            .clicked()
                        {
                            self.sinterbox_bar_mm =
                                sinterbox::recommended_bar_thickness_mm(self.sinterbox_window_mm);
                            self.sinterbox_bar_auto = true;
                        }
                    });
                });

                ui.separator();
                if checkbox_row(ui, "sinterbox_label_en", "Add 3D Label", self.sinterbox_label_enabled) {
                    self.sinterbox_label_enabled = !self.sinterbox_label_enabled;
                }
                if self.sinterbox_label_enabled {
                    row(ui, "Label Text", |ui| {
                        if ui
                            .add(egui::TextEdit::singleline(&mut self.sinterbox_label_text).desired_width(180.0))
                            .changed()
                            && !self.sinterbox_group_name_edited
                        {
                            let trimmed = self.sinterbox_label_text.trim();
                            self.sinterbox_group_name = if trimmed.is_empty() {
                                format!("Sinterbox #{}", self.job.groups.len() + 1)
                            } else {
                                format!("Sinterbox: {trimmed}")
                            };
                        }
                    });
                    row(ui, "Label Panel", |ui| {
                        let options = [
                            "Auto (Largest)",
                            "Top (+Z)",
                            "Front (-Y)",
                            "Right (+X)",
                            "Back (+Y)",
                            "Left (-X)",
                            "Bottom (-Z)",
                        ];
                        let selected_lbl = options[self.sinterbox_label_panel_idx.min(options.len() - 1)];
                        egui::ComboBox::from_id_salt("sinterbox_panel_combo")
                            .selected_text(selected_lbl)
                            .show_ui(ui, |ui| {
                                for (idx, opt) in options.iter().enumerate() {
                                    ui.selectable_value(&mut self.sinterbox_label_panel_idx, idx, *opt);
                                }
                            });
                    });
                    row(ui, "Emboss Depth", |ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.sinterbox_label_depth)
                                .range(0.2..=3.0)
                                .speed(0.05)
                                .suffix(" mm"),
                        );
                    });
                }

                ui.separator();
                row(ui, "Group Name", |ui| {
                    if ui
                        .add(egui::TextEdit::singleline(&mut self.sinterbox_group_name).desired_width(180.0))
                        .changed()
                    {
                        self.sinterbox_group_name_edited = true;
                    }
                });
                if checkbox_row(ui, "sinterbox_transp", "50% Transparent Cage", self.sinterbox_transparent) {
                    self.sinterbox_transparent = !self.sinterbox_transparent;
                }

                if let Some(ref m) = metrics {
                    status_line(
                        ui,
                        &format!(
                            "Cage: {:.1}×{:.1}×{:.1} mm · Panel: {} ({:.1}×{:.1} mm)",
                            m.outer_size_mm[0],
                            m.outer_size_mm[1],
                            m.outer_size_mm[2],
                            m.chosen_panel.display_name(),
                            m.chosen_panel_size_mm[0],
                            m.chosen_panel_size_mm[1],
                        ),
                        StatusTone::Dim,
                    );
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Create Sinterbox", Button::Primary).clicked() {
                        apply = true;
                    }
                    if button(ui, "Cancel", Button::Small).clicked() {
                        cancel = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        } else if !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            apply = true;
        }

        if switch_to_dup {
            self.sinterbox_dialog_open = false;
            self.open_duplicate_dialog();
        } else if apply {
            self.apply_create_sinterbox();
            self.sinterbox_dialog_open = false;
        } else if cancel || !open {
            self.sinterbox_dialog_open = false;
        }
    }

    fn draw_export_stl_modal(&mut self, ctx: &egui::Context) {
        if !self.export_stl_dialog_open {
            return;
        }
        let mut open = self.export_stl_dialog_open;
        let mut do_export = false;
        let mut cancel = false;

        egui::Window::new("Export Individual STLs")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                let mut scope_idx = usize::from(self.export_stl_selected_only);
                row(ui, "Scope", |ui| {
                    ui.push_id("export_stl_scope", |ui| {
                        segmented(
                            ui,
                            &mut scope_idx,
                            &["All Visible", "Selected Only"],
                            Segments {
                                dense: true,
                                ..Default::default()
                            },
                        )
                    })
                    .inner
                });
                self.export_stl_selected_only = scope_idx == 1;

                row(ui, "Prefix (ASCII)", |ui| {
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.export_stl_prefix)
                                .desired_width(180.0)
                                .hint_text("e.g. batch1_"),
                        )
                        .changed()
                    {
                        self.export_stl_prefix = sanitize_ascii_filename_part(&self.export_stl_prefix);
                    }
                });
                row(ui, "Suffix (ASCII)", |ui| {
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.export_stl_suffix)
                                .desired_width(180.0)
                                .hint_text("e.g. _placed"),
                        )
                        .changed()
                    {
                        self.export_stl_suffix = sanitize_ascii_filename_part(&self.export_stl_suffix);
                    }
                });
                if checkbox_row(ui, "exp_stl_id", "Include Instance #ID", self.export_stl_include_id) {
                    self.export_stl_include_id = !self.export_stl_include_id;
                }
                if checkbox_row(ui, "exp_stl_tf", "Apply Bed Transform", self.export_stl_apply_transform) {
                    self.export_stl_apply_transform = !self.export_stl_apply_transform;
                }

                let sample_name = self
                    .job
                    .parts
                    .first()
                    .map(|p| p.name.as_str())
                    .unwrap_or("part");
                let sample_id = self.job.instances.first().map(|i| i.id.0).unwrap_or(1);
                let preview_stem = format_export_stl_stem(
                    &self.export_stl_prefix,
                    sample_name,
                    sample_id,
                    self.export_stl_include_id,
                    &self.export_stl_suffix,
                );
                status_line(ui, &format!("Preview: {preview_stem}.stl"), StatusTone::Dim);

                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Export to Folder...", Button::Primary).clicked() {
                        do_export = true;
                    }
                    if button(ui, "Cancel", Button::Small).clicked() {
                        cancel = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }

        if do_export {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                match self.export_individual_stls(&dir) {
                    Ok(n) => {
                        self.status = format!("Exported {n} STL file(s) to {}", dir.display());
                        self.export_stl_dialog_open = false;
                    }
                    Err(e) => {
                        self.status = e;
                    }
                }
            }
        } else if cancel || !open {
            self.export_stl_dialog_open = false;
        }
    }

    fn reset_below_zero_imported_parts(&mut self) {
        let items = std::mem::take(&mut self.below_zero_import_dialog);
        if items.is_empty() {
            return;
        }
        self.push_undo();
        let mut adjusted = 0usize;
        for (id, _) in items {
            if self.job.reset_instance_z_to_zero(id) {
                adjusted += 1;
            }
        }
        if adjusted > 0 {
            self.mark_dirty();
            self.status = format!("Reset Z = 0 (kept XY) for {adjusted} imported part(s)");
        }
    }

    fn draw_below_zero_import_modal(&mut self, ctx: &egui::Context) {
        if self.below_zero_import_dialog.is_empty() {
            return;
        }
        let count = self.below_zero_import_dialog.len();
        let lowest_z = self
            .below_zero_import_dialog
            .iter()
            .map(|(_, z)| *z)
            .fold(f32::INFINITY, f32::min);

        let mut open = true;
        let mut reset_zero = false;
        let mut keep_location = false;

        egui::Window::new("Imported Part(s) Below Z = 0")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                status_line(
                    ui,
                    &format!("{count} imported part(s) extend below Z = 0 (lowest Z: {lowest_z:.2} mm)."),
                    StatusTone::Error,
                );
                ui.add_space(4.0);
                ui.label("Keep the imported model location, or reset Z = 0 while keeping XY?");
                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Reset Z = 0 (Keep XY)", Button::Primary).clicked() {
                        reset_zero = true;
                    }
                    if button(ui, "Keep Model Location", Button::Small).clicked() {
                        keep_location = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            keep_location = true;
        }

        if reset_zero {
            self.reset_below_zero_imported_parts();
        } else if keep_location || !open {
            self.below_zero_import_dialog.clear();
            self.status = format!("Kept imported model location for {count} part(s) below Z = 0");
        }
    }

    fn draw_load_progress_modal(&self, ctx: &egui::Context) {
        let Some(task) = &self.load_task else {
            return;
        };
        let frac = if task.total > 0 {
            (task.completed as f32 / task.total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        egui::Window::new("Loading")
            .collapsible(false)
            .resizable(false)
            .title_bar(true)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.label(&task.current_label);
                ui.add_space(6.0);
                ui.add(
                    egui::ProgressBar::new(frac)
                        .show_percentage()
                        .animate(true)
                        .desired_width(320.0),
                );
                ui.add_space(4.0);
                status_line(
                    ui,
                    &format!("{} of {} file(s) processed", task.completed, task.total),
                    StatusTone::Dim,
                );
            });
    }

    fn draw_keep_delete_modal(&mut self, ctx: &egui::Context) {
        let Some(action) = self.pending_keep_delete.clone() else {
            return;
        };
        let (title, summary) = match &action {
            KeepOrDeleteAction::SplitShells(ids) => (
                "Split Shells",
                format!(
                    "Split {} selected part(s) into separate shell parts.\nKeep or delete the original part(s)?",
                    ids.len()
                ),
            ),
            KeepOrDeleteAction::MergeSelected(ids) => (
                "Merge Parts",
                format!(
                    "Merge {} selected parts into a single combined part.\nKeep or delete the original parts?",
                    ids.len()
                ),
            ),
        };
        let mut open = true;
        let mut choice: Option<bool> = None;
        let mut cancel = false;
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.label(summary);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if button(ui, "Keep Original", Button::Primary).clicked() {
                        choice = Some(true);
                    }
                    if button(ui, "Delete Original", Button::Small).clicked() {
                        choice = Some(false);
                    }
                    if button(ui, "Cancel", Button::Small).clicked() {
                        cancel = true;
                    }
                });
            });
        if let Some(keep) = choice {
            self.pending_keep_delete = None;
            match action {
                KeepOrDeleteAction::SplitShells(ids) => self.shells_to_parts_for_with_keep(ids, keep),
                KeepOrDeleteAction::MergeSelected(ids) => self.merge_selected_parts_with_keep(&ids, keep),
            }
        } else if cancel || !open {
            self.pending_keep_delete = None;
        }
    }

    fn draw_selected_part_section(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        ui.spacing_mut().item_spacing = egui::vec2(t.space_6, t.space_6);
        section(ui, "inst_sec", "Selected Part", true, |ui| {
            let Some(sel_id) = self.selected else {
                status_line(ui, "Select a part in the 3D view or Parts Browser.", StatusTone::Dim);
                return;
            };

            if self.selected_set.len() > 1 {
                let sel_vol_cm3: f64 = self
                    .job
                    .instances
                    .iter()
                    .filter(|i| self.selected_set.contains(&i.id))
                    .filter_map(|i| self.job.part(i.part_id).map(|p| p.mesh.volume_mm3))
                    .sum::<f64>()
                    / 1000.0;
                status_line(
                    ui,
                    &format!("{} selected ({sel_vol_cm3:.2} cm³ total)", self.selected_set.len()),
                    StatusTone::Dim,
                );
            }

            let above_floor = self.job.is_instance_above_floor(sel_id);
            let in_nbz = self.job.is_instance_in_no_build_zone(sel_id);
            let current_euler = self
                .job
                .instances
                .iter()
                .find(|i| i.id == sel_id)
                .map(|i| i.euler_deg());
            let part_metrics = self
                .job
                .instances
                .iter()
                .find(|i| i.id == sel_id)
                .and_then(|inst| {
                    self.job.part(inst.part_id).map(|p| {
                        let wb = inst.world_bounds(p.mesh.bounds);
                        (
                            [
                                wb[1][0] - wb[0][0],
                                wb[1][1] - wb[0][1],
                                wb[1][2] - wb[0][2],
                            ],
                            wb[0][2],
                            wb[1][2],
                            p.mesh.volume_mm3 / 1000.0,
                            p.mesh.surface_area_mm2 / 100.0,
                        )
                    })
                });

            if let Some((dims, min_z, inst_max_z, vol_cm3, area_cm2)) = part_metrics {
                value_row(
                    ui,
                    "Dimensions",
                    &format!("{:.1} × {:.1} × {:.1} mm", dims[0], dims[1], dims[2]),
                )
                .backing_on_hover_text("World-space bounding box size (X × Y × Z)");
                value_row(ui, "Volume", &format!("{vol_cm3:.2} cm³"))
                    .backing_on_hover_text("Part mesh volume in cubic centimetres");
                value_row(ui, "Surface Area", &format!("{area_cm2:.2} cm²"))
                    .backing_on_hover_text("Part mesh surface area in square centimetres");
                value_row(ui, "Z Span", &format!("{min_z:.2} .. {inst_max_z:.2} mm"))
                    .backing_on_hover_text("Lowest to highest Z coordinate (Z = 0 is the floor)");
                ui.add_space(t.space_4);
            }

            let mut drop_floor = false;
            let mut clamp_floor = false;
            let mut center_xy = false;
            let mut rot_step: Option<usize> = None;

            let part_color_opt = self
                .job
                .instances
                .iter()
                .find(|i| i.id == sel_id)
                .and_then(|i| i.color);
            let mut color_action: Option<Option<[u8; 3]>> = None;
            row(ui, "Color", |ui| {
                ui.horizontal(|ui| {
                    let mut rgb = part_color_opt.unwrap_or([t.text.r(), t.text.g(), t.text.b()]);
                    if ui.color_edit_button_srgb(&mut rgb).changed() {
                        color_action = Some(Some(rgb));
                    }
                    if part_color_opt.is_some()
                        && button(ui, "Reset", Button::Small)
                            .backing_on_hover_text("Reset part color to theme default")
                            .clicked()
                    {
                        color_action = Some(None);
                    }
                });
            });
            if let Some(new_color) = color_action {
                self.set_selected_color(new_color);
            }

            let mut applied_euler = None;
            if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == sel_id) {
                let mut pos = inst.translation();
                let mut changed = false;
                row(ui, "Position", |ui| {
                    ui.horizontal(|ui| {
                        changed |= ui
                            .add(egui::DragValue::new(&mut pos[0]).speed(0.5).prefix("X "))
                            .changed();
                        changed |= ui
                            .add(egui::DragValue::new(&mut pos[1]).speed(0.5).prefix("Y "))
                            .changed();
                        changed |= ui
                            .add(egui::DragValue::new(&mut pos[2]).speed(0.5).prefix("Z "))
                            .changed();
                    });
                });

                row(ui, "Quick Place", |ui| {
                    ui.horizontal(|ui| {
                        if button(ui, "Drop Z=0", Button::Small)
                            .backing_on_hover_text("Align lowest vertex to build floor (Z = 0)")
                            .clicked()
                        {
                            drop_floor = true;
                        }
                        if button(ui, "Center XY", Button::Small)
                            .backing_on_hover_text("Center instance horizontally in chamber")
                            .clicked()
                        {
                            center_xy = true;
                        }
                    });
                });

                let euler = self
                    .euler_edit
                    .entry(sel_id)
                    .or_insert_with(|| current_euler.unwrap_or([0.0, 0.0, 0.0]));
                let mut rot_changed = false;
                row(ui, "Rotation", |ui| {
                    ui.horizontal(|ui| {
                        rot_changed |= ui
                            .add(egui::DragValue::new(&mut euler[0]).speed(1.0).suffix("°"))
                            .changed();
                        rot_changed |= ui
                            .add(egui::DragValue::new(&mut euler[1]).speed(1.0).suffix("°"))
                            .changed();
                        rot_changed |= ui
                            .add(egui::DragValue::new(&mut euler[2]).speed(1.0).suffix("°"))
                            .changed();
                    });
                });

                row(ui, "Step +90°", |ui| {
                    ui.horizontal(|ui| {
                        if button(ui, "+90° X", Button::Small).clicked() {
                            rot_step = Some(0);
                        }
                        if button(ui, "+90° Y", Button::Small).clicked() {
                            rot_step = Some(1);
                        }
                        if button(ui, "+90° Z", Button::Small).clicked() {
                            rot_step = Some(2);
                        }
                    });
                });

                if rot_changed {
                    applied_euler = Some(*euler);
                    clamp_floor = true;
                    changed = true;
                } else if changed {
                    inst.set_translation(pos);
                }

                let mut orient_idx = match inst.constraint {
                    RotationConstraint::Free => 0,
                    RotationConstraint::ZOnly => 1,
                    RotationConstraint::Fixed => 2,
                };
                let orient_changed = row(ui, "Orientation", |ui| {
                    ui.push_id("orient_seg", |ui| {
                        segmented(
                            ui,
                            &mut orient_idx,
                            &["Free", "Z-Only", "Fixed"],
                            Segments {
                                dense: true,
                                ..Default::default()
                            },
                        )
                    })
                    .inner
                });
                if orient_changed {
                    inst.constraint = match orient_idx {
                        1 => RotationConstraint::ZOnly,
                        2 => RotationConstraint::Fixed,
                        _ => RotationConstraint::Free,
                    };
                }

                if checkbox_row(ui, "lock_inst", "Lock Placement", inst.locked) {
                    inst.locked = !inst.locked;
                }

                status_line(
                    ui,
                    if !above_floor {
                        "Below Z = 0 floor!"
                    } else if in_nbz {
                        "Intersects No-Build Zone!"
                    } else {
                        "On or above Z = 0 floor"
                    },
                    if above_floor && !in_nbz {
                        StatusTone::Dim
                    } else {
                        StatusTone::Error
                    },
                );

                if changed {
                    self.mark_dirty();
                }
            }

            if let Some(new_euler) = applied_euler {
                self.job.set_instance_euler_around_center(sel_id, new_euler);
            }
            if drop_floor && self.job.drop_instance_to_floor(sel_id) {
                self.mark_dirty();
            }
            if clamp_floor && self.job.clamp_instance_above_floor(sel_id) {
                self.mark_dirty();
            }
            if center_xy && self.job.center_instance_xy(sel_id) {
                self.mark_dirty();
            }
            if let Some(axis) = rot_step {
                self.rotate_selected_90(axis);
            }
        });
    }

    fn draw_inspector(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let side_c = Container::new(ctx, Surface::Panel);
        let right_panel = side_c.edge("sls_inspector", Edge::Right).default_size(310.0);
        let right_frame = side_c
            .edge_frame(t.panel)
            .inner_margin(egui::Margin::symmetric(t.space_10 as i8, t.space_10 as i8));
        side_c.show_edge(ui, right_panel, right_frame, |ui| {
            scope(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(t.space_6, t.space_6);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let [wx, wy, wz] = self.job.machine.size_mm();
                    let max_z = self.job.max_z_mm();
                    let layers = (max_z / 0.1).ceil() as u32;
                    let total_vol_cm3 = self.job.total_part_volume_mm3() / 1000.0;
                    let total_area_cm2: f64 = self
                        .job
                        .instances
                        .iter()
                        .filter_map(|inst| self.job.part(inst.part_id).map(|p| p.mesh.surface_area_mm2))
                        .sum::<f64>()
                        / 100.0;
                    let occupied_density = self.job.packing_density_pct();
                    let full_chamber_vol_cm3 = self.job.machine.volume_mm3() / 1000.0;
                    let full_chamber_density = if full_chamber_vol_cm3 > 1e-6 {
                        (total_vol_cm3 / full_chamber_vol_cm3) * 100.0
                    } else {
                        0.0
                    };
                    let below_floor_count = self
                        .job
                        .instances
                        .iter()
                        .filter(|inst| !self.job.is_instance_above_floor(inst.id))
                        .count();
                    let in_nbz_count = self
                        .job
                        .instances
                        .iter()
                        .filter(|inst| self.job.is_instance_in_no_build_zone(inst.id))
                        .count();

                    section(ui, "build_summary_sec", "Build Summary", true, |ui| {
                        value_row(
                            ui,
                            "Instances",
                            &format!("{} ({} unique)", self.job.instances.len(), self.job.parts.len()),
                        )
                        .backing_on_hover_text("Placed part instances and unique part geometries");
                        value_row(ui, "Part Volume", &format!("{total_vol_cm3:.2} cm³"))
                            .backing_on_hover_text("Total sintered volume across all placed instances");
                        value_row(ui, "Surface Area", &format!("{total_area_cm2:.2} cm²"))
                            .backing_on_hover_text("Total triangle surface area across all placed instances");
                        value_row(ui, "Z Height", &format!("{max_z:.2} / {wz:.0} mm ({layers} layers)"))
                            .backing_on_hover_text(
                                "Highest Z across all parts vs platform height (layer count at 0.10 mm recoater step)",
                            );
                        value_row(ui, "Density", &format!("{occupied_density:.2}%"))
                            .backing_on_hover_text(format!(
                                "Occupied cake density = total volume / (W × D × max Z).\nFull-chamber density (at H = {wz:.0} mm): {full_chamber_density:.2}%.\nTypical PA12 SLS target: 8–15%."
                            ));
                        if below_floor_count > 0 {
                            status_line(
                                ui,
                                &format!("{below_floor_count} instance(s) below Z = 0 floor!"),
                                StatusTone::Error,
                            );
                        }
                        if in_nbz_count > 0 {
                            status_line(
                                ui,
                                &format!("{in_nbz_count} instance(s) in No-Build Zone!"),
                                StatusTone::Error,
                            );
                        }
                    });

                    ui.add_space(t.space_12);
                    section(ui, "chamber_sec", "Build Chamber", true, |ui| {
                        value_row(ui, "Machine", self.job.machine.name())
                            .backing_on_hover_text("Reference SLS machine profile");
                        value_row(ui, "Envelope", &format!("{wx:.0} × {wy:.0} × {wz:.0} mm"))
                            .backing_on_hover_text(format!(
                                "Reference platform W × D × H ({full_chamber_vol_cm3:.0} cm³ total chamber volume)"
                            ));
                        row(ui, "Origin (0,0,0)", |ui| {
                            let mut orig_idx = match self.job.bed_origin {
                                BedOrigin::FrontLeft => 0,
                                BedOrigin::Center => 1,
                            };
                            if ui
                                .push_id("insp_origin_seg", |ui| {
                                    segmented(
                                        ui,
                                        &mut orig_idx,
                                        &["Front-Left", "Center"],
                                        Segments {
                                            dense: true,
                                            ..Default::default()
                                        },
                                    )
                                })
                                .inner
                            {
                                self.job.bed_origin = if orig_idx == 1 {
                                    BedOrigin::Center
                                } else {
                                    BedOrigin::FrontLeft
                                };
                                self.camera =
                                    viewport::default_camera_with_origin(&self.job.machine, self.job.bed_origin);
                                self.mark_dirty();
                            }
                        });
                        row(ui, "Wall Distance", |ui| {
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.job.wall_margin_mm)
                                        .range(0.0..=50.0)
                                        .speed(0.5)
                                        .suffix(" mm"),
                                )
                                .backing_on_hover_text("Reference clearance from X/Y platform walls")
                                .changed()
                            {
                                self.mark_dirty();
                            }
                        });
                        row(ui, "Min Z", |ui| {
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.job.min_z_mm)
                                        .range(0.0..=100.0)
                                        .speed(0.5)
                                        .suffix(" mm"),
                                )
                                .backing_on_hover_text("Minimum allowed Z floor height (default 0.0 mm)")
                                .changed()
                            {
                                self.mark_dirty();
                            }
                        });
                        row(ui, "Clearance", |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.job.clearance_mm)
                                    .range(0.5..=25.0)
                                    .speed(0.25)
                                    .suffix(" mm"),
                            )
                            .backing_on_hover_text("Default part-to-part gap for placement & grid duplication");
                        });
                    });

                    ui.add_space(t.space_12);
                    section(ui, "nbz_sec", "No-Build Zone", false, |ui| {
                        let mut pick_stl = false;
                        let mut clear_zone = false;
                        let mut zone_changed = false;
                        row(ui, "Zone STL", |ui| {
                            ui.horizontal(|ui| {
                                if button(ui, "Import STL...", Button::Small)
                                    .backing_on_hover_text(
                                        "Import an STL mesh and save it permanently as the machine No-Build Zone",
                                    )
                                    .clicked()
                                {
                                    pick_stl = true;
                                }
                                if self.job.no_build_zone.is_some()
                                    && button(ui, "Clear", Button::Small)
                                        .backing_on_hover_text("Remove the saved No-Build Zone")
                                        .clicked()
                                {
                                    clear_zone = true;
                                }
                            });
                        });

                        if let Some(zone) = &mut self.job.no_build_zone {
                            let [lo, hi] = zone.mesh.bounds;
                            value_row(ui, "Name", &zone.name);
                            value_row(
                                ui,
                                "Size",
                                &format!("{:.1} × {:.1} × {:.1} mm", hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]),
                            );
                            if checkbox_row(ui, "nbz_enabled", "Enable Keep-Out", zone.enabled) {
                                zone.enabled = !zone.enabled;
                                zone_changed = true;
                            }
                            if checkbox_row(ui, "nbz_visible", "Show in Viewport", zone.visible) {
                                zone.visible = !zone.visible;
                                zone_changed = true;
                            }
                            if checkbox_row(ui, "nbz_locked", "Lock Dragging", zone.locked) {
                                zone.locked = !zone.locked;
                                zone_changed = true;
                            }
                            row(ui, "Quick Place", |ui| {
                                ui.horizontal(|ui| {
                                    if button(ui, "Drop Min Z", Button::Small)
                                        .backing_on_hover_text("Align lowest Z of No-Build Zone to Min Z floor")
                                        .clicked()
                                    {
                                        let wb = zone.world_bounds();
                                        zone.position[2] += self.job.min_z_mm.max(0.0) - wb[0][2];
                                        zone_changed = true;
                                    }
                                    if button(ui, "Center XY", Button::Small)
                                        .backing_on_hover_text("Center No-Build Zone horizontally in platform")
                                        .clicked()
                                    {
                                        let wb = zone.world_bounds();
                                        let [bcx, bcy] = match self.job.bed_origin {
                                            BedOrigin::FrontLeft => [wx * 0.5, wy * 0.5],
                                            BedOrigin::Center => [0.0, 0.0],
                                        };
                                        zone.position[0] += bcx - (wb[0][0] + wb[1][0]) * 0.5;
                                        zone.position[1] += bcy - (wb[0][1] + wb[1][1]) * 0.5;
                                        zone_changed = true;
                                    }
                                });
                            });
                            row(ui, "Position", |ui| {
                                ui.horizontal(|ui| {
                                    zone_changed |= ui
                                        .add(egui::DragValue::new(&mut zone.position[0]).speed(0.5).prefix("X "))
                                        .changed();
                                    zone_changed |= ui
                                        .add(egui::DragValue::new(&mut zone.position[1]).speed(0.5).prefix("Y "))
                                        .changed();
                                    zone_changed |= ui
                                        .add(egui::DragValue::new(&mut zone.position[2]).speed(0.5).prefix("Z "))
                                        .changed();
                                });
                            });
                            row(ui, "Rotation", |ui| {
                                ui.horizontal(|ui| {
                                    zone_changed |= ui
                                        .add(egui::DragValue::new(&mut zone.rotation_deg[0]).speed(1.0).suffix("°"))
                                        .changed();
                                    zone_changed |= ui
                                        .add(egui::DragValue::new(&mut zone.rotation_deg[1]).speed(1.0).suffix("°"))
                                        .changed();
                                    zone_changed |= ui
                                        .add(egui::DragValue::new(&mut zone.rotation_deg[2]).speed(1.0).suffix("°"))
                                        .changed();
                                });
                            });
                            row(ui, "Clearance", |ui| {
                                zone_changed |= ui
                                    .add(
                                        egui::DragValue::new(&mut zone.clearance_mm)
                                            .range(0.0..=50.0)
                                            .speed(0.25)
                                            .suffix(" mm"),
                                    )
                                    .backing_on_hover_text("Extra keep-out margin around the No-Build Zone STL")
                                    .changed();
                            });
                        } else {
                            status_line(ui, "No keep-out STL configured.", StatusTone::Dim);
                        }

                        if pick_stl
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("STL Mesh", &["stl", "STL"])
                                .pick_file()
                        {
                            self.import_no_build_zone_stl(&path);
                        }
                        if clear_zone {
                            self.clear_no_build_zone();
                        } else if zone_changed {
                            self.mark_dirty();
                            self.persist_no_build_zone();
                        }
                    });
                });
            });
        });
    }

    fn start_instance_drag(
        &mut self,
        anchor_id: InstanceId,
        rect: egui::Rect,
        pos: egui::Pos2,
        anchor_pos: [f32; 3],
    ) {
        let drag_instances: Vec<(InstanceId, [f32; 3])> = self
            .job
            .instances
            .iter()
            .filter(|i| self.selected_set.contains(&i.id) && !i.locked)
            .map(|i| (i.id, i.translation()))
            .collect();
        if !drag_instances.is_empty()
            && let Some(hit_pt) = viewport::intersect_camera_plane(&self.camera, rect, pos, anchor_pos)
        {
            self.push_undo();
            let drag_ids: Vec<InstanceId> = drag_instances.iter().map(|(id, _)| *id).collect();
            self.drag_state = Some(DragState {
                anchor_id,
                instances: drag_instances,
                start_hit: hit_pt,
                plane_z: anchor_pos[2],
            });
            self.start_moving_layer(viewport::DragExclude::Instances(drag_ids));
        }
    }

    fn draw_viewport(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let central = Container::new(ctx, Surface::Viewport).central();
        central.show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            self.viewport_aspect = f64::from(rect.width() / rect.height().max(1.0));
            let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());

            let compass_centre = egui::pos2(rect.right() - 62.0, rect.top() + 62.0);
            let home_centre = egui::pos2(compass_centre.x - 37.0, compass_centre.y - 37.0);
            let over_compass = resp
                .hover_pos()
                .is_some_and(|p| p.distance(compass_centre) <= 52.0 || p.distance(home_centre) <= 14.0);
            let over_view_bar = resp.hover_pos().is_some_and(|p| {
                self.view_bar_rect.is_some_and(|r| r.contains(p))
                    || self.repair_badge_rect.is_some_and(|r| r.contains(p))
            });

            let centerpoints = viewport::selected_centerpoints(&self.job, &self.selected_set, &self.camera, rect);
            let rot_handle = viewport::selected_rotation_handle(&self.job, &self.selected_set, &self.camera, rect);
            let pointer_hover = resp.hover_pos().filter(|_| !over_compass && !over_view_bar && self.align_face_target.is_none() && !self.zoom_rect_mode);
            let hovered_rot = rot_handle.filter(|(p, _)| pointer_hover.is_some_and(|hp| hp.distance(*p) <= 12.0));
            let hovered_center = if hovered_rot.is_none() {
                centerpoints
                    .iter()
                    .find(|c| pointer_hover.is_some_and(|hp| hp.distance(c.screen_pos) <= 9.0))
                    .copied()
            } else {
                None
            };
            if self.drag_state.is_some() || self.rotate_drag.is_some() {
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if hovered_rot.is_some() || hovered_center.is_some() {
                ctx.set_cursor_icon(egui::CursorIcon::Grab);
            }

            if !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && self.zoom_rect_mode
                && resp.drag_started_by(egui::PointerButton::Secondary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                self.viewport_ctx_menu = None;
                self.zoom_rect = Some((pos, pos));
            }

            if resp.dragged_by(egui::PointerButton::Middle) {
                let delta = resp.drag_delta();
                self.camera.pan(f64::from(delta.x), f64::from(delta.y), f64::from(rect.height().max(1.0)));
            } else if resp.dragged_by(egui::PointerButton::Secondary) {
                self.viewport_ctx_menu = None;
                if let Some((_, cur)) = &mut self.zoom_rect {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        *cur = pos;
                    }
                } else if !self.zoom_rect_mode {
                    let delta = resp.drag_delta();
                    if ui.input(|i| i.modifiers.shift) {
                        self.camera.pan(f64::from(delta.x), f64::from(delta.y), f64::from(rect.height().max(1.0)));
                    } else {
                        self.camera.orbit_unlimited_through_roll(f64::from(-delta.x) * 0.75, f64::from(delta.y) * 0.75);
                    }
                }
            }

            if resp.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll.abs() > 0.01 {
                    let factor = (1.0 - f64::from(scroll) * 0.0015).clamp(0.5, 1.5);
                    self.camera.zoom_at(factor, None);
                    self.camera.distance = self.camera.distance.clamp(15.0, 5000.0);
                }
            }

            if !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && resp.drag_started_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                if let Some((_, pivot)) = hovered_rot {
                    let rot_instances: Vec<(InstanceId, [[f32; 4]; 3], RotationConstraint)> = self
                        .job
                        .instances
                        .iter()
                        .filter(|i| {
                            self.selected_set.contains(&i.id)
                                && !i.locked
                                && i.constraint != RotationConstraint::Fixed
                        })
                        .map(|i| (i.id, i.transform, i.constraint))
                        .collect();
                    if !rot_instances.is_empty() {
                        self.push_undo();
                        let rot_ids: Vec<InstanceId> = rot_instances.iter().map(|(id, _, _)| *id).collect();
                        self.rotate_drag = Some(RotateDragState {
                            pivot,
                            instances: rot_instances,
                            accum_dx: 0.0,
                            accum_dy: 0.0,
                        });
                        self.start_moving_layer(viewport::DragExclude::Instances(rot_ids));
                    }
                } else if let Some(ch) = hovered_center {
                    self.start_instance_drag(ch.id, rect, pos, ch.world_pos);
                } else {
                    let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                    if let Some(iid) = hit {
                        if !self.selected_set.contains(&iid) {
                            self.select_instance(Some(iid));
                        }
                        if let Some(inst) = self.job.instances.iter().find(|i| i.id == iid)
                            && !inst.locked
                        {
                            let tr = inst.translation();
                            self.start_instance_drag(iid, rect, pos, tr);
                        }
                    } else if viewport::pick_no_build_zone(&self.job, &self.camera, rect, pos) {
                        if let Some(zone) = &self.job.no_build_zone
                            && !zone.locked
                        {
                            let zone_pos = zone.position;
                            let plane_z = zone_pos[2];
                            if let Some(hit_pt) = viewport::intersect_camera_plane(&self.camera, rect, pos, zone_pos) {
                                self.push_undo();
                                self.drag_zone = Some(DragZoneState {
                                    start_position: zone_pos,
                                    start_hit: hit_pt,
                                    plane_z,
                                });
                                self.start_moving_layer(viewport::DragExclude::NoBuildZone);
                            }
                        }
                    } else {
                        self.box_select = Some(BoxSelectState {
                            start: pos,
                            current: pos,
                            additive: ui.input(|i| i.modifiers.command),
                        });
                    }
                }
            }

            if resp.dragged_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                if let Some(rd) = &mut self.rotate_drag {
                    let delta = resp.drag_delta();
                    rd.accum_dx += delta.x;
                    rd.accum_dy += delta.y;
                    let (cam_right, cam_up, _) = self.camera.basis();
                    let cr = cam_right.to_f32();
                    let cu = cam_up.to_f32();
                    let ctrl = ui.input(|i| i.modifiers.command);
                    let alt = ui.input(|i| i.modifiers.alt);
                    let sensitivity = 0.012_f32;
                    let raw_vec = [
                        cu[0] * rd.accum_dx + cr[0] * rd.accum_dy,
                        cu[1] * rd.accum_dx + cr[1] * rd.accum_dy,
                        cu[2] * rd.accum_dx + cr[2] * rd.accum_dy,
                    ];
                    let raw_mag = (raw_vec[0] * raw_vec[0] + raw_vec[1] * raw_vec[1] + raw_vec[2] * raw_vec[2]).sqrt();
                    let mut angle = raw_mag * sensitivity;
                    if alt {
                        let step = 10.0_f32.to_radians();
                        angle = (angle / step).round() * step;
                    }
                    let free_axis = if ctrl && raw_mag > 1e-6 {
                        snap_to_nearest_axis(raw_vec)
                    } else {
                        raw_vec
                    };
                    let pivot = rd.pivot;
                    for (id, orig, constraint) in &rd.instances {
                        let (axis, inst_angle) = match constraint {
                            RotationConstraint::ZOnly => {
                                let mut za = rd.accum_dx * sensitivity;
                                if alt {
                                    let step = 10.0_f32.to_radians();
                                    za = (za / step).round() * step;
                                }
                                ([0.0, 0.0, 1.0], za)
                            }
                            RotationConstraint::Free => (free_axis, angle),
                            RotationConstraint::Fixed => continue,
                        };
                        if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == *id) {
                            inst.transform = rotate_transform_around_pivot(*orig, pivot, axis, inst_angle);
                            self.euler_edit.remove(id);
                        }
                    }
                } else if let Some(bs) = &mut self.box_select {
                    bs.current = pos;
                } else if let Some(dz) = self.drag_zone {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if shift {
                        if let Some(z_now) = viewport::intersect_vertical_plane(&self.camera, rect, pos, dz.start_hit)
                            && let Some(zone) = &mut self.job.no_build_zone
                        {
                            zone.position[2] = dz.start_position[2] + (z_now - dz.plane_z);
                        }
                    } else if let Some(hit_now) = viewport::intersect_camera_plane(&self.camera, rect, pos, dz.start_hit)
                        && let Some(zone) = &mut self.job.no_build_zone
                    {
                        zone.position[0] = dz.start_position[0] + (hit_now[0] - dz.start_hit[0]);
                        zone.position[1] = dz.start_position[1] + (hit_now[1] - dz.start_hit[1]);
                        zone.position[2] = dz.start_position[2] + (hit_now[2] - dz.start_hit[2]);
                    }
                } else if let Some(ds) = &self.drag_state {
                    let shift = ui.input(|i| i.modifiers.shift);
                    let raw_delta = if shift {
                        viewport::intersect_vertical_plane(&self.camera, rect, pos, ds.start_hit)
                            .map(|z_now| [0.0, 0.0, z_now - ds.plane_z])
                    } else {
                        viewport::intersect_camera_plane(&self.camera, rect, pos, ds.start_hit).map(|hit_now| {
                            [
                                hit_now[0] - ds.start_hit[0],
                                hit_now[1] - ds.start_hit[1],
                                hit_now[2] - ds.start_hit[2],
                            ]
                        })
                    };
                    if let Some(mut delta) = raw_delta {
                        let floor_z = self.job.min_z_mm.max(0.0);
                        let mut min_allowed_dz = f32::NEG_INFINITY;
                        for (id, start_tr) in &ds.instances {
                            if let Some(inst) = self.job.instances.iter().find(|i| i.id == *id)
                                && let Some(proto) = self.job.part(inst.part_id)
                            {
                                let cur_tr = inst.translation();
                                let cur_min_z = inst.world_bounds(proto.mesh.bounds)[0][2];
                                let start_min_z = cur_min_z - cur_tr[2] + start_tr[2];
                                min_allowed_dz = min_allowed_dz.max(floor_z - start_min_z);
                            }
                        }
                        if min_allowed_dz.is_finite() {
                            delta[2] = delta[2].max(min_allowed_dz);
                        }
                        for (id, start_tr) in &ds.instances {
                            if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == *id) {
                                inst.set_translation([
                                    start_tr[0] + delta[0],
                                    start_tr[1] + delta[1],
                                    start_tr[2] + delta[2],
                                ]);
                            }
                        }
                    }
                }
            }

            if resp.drag_stopped() {
                if self.rotate_drag.take().is_some() {
                    self.mark_dirty();
                    self.status = "Rotated selected part(s)".to_owned();
                }
                if let Some(bs) = self.box_select.take() {
                    let sel_rect = egui::Rect::from_two_pos(bs.start, bs.current);
                    if sel_rect.width() > 3.0 || sel_rect.height() > 3.0 {
                        let hits = viewport::instances_in_screen_rect(&self.job, &self.camera, rect, sel_rect);
                        if !bs.additive {
                            self.selected_set.clear();
                        }
                        for id in &hits {
                            self.selected_set.insert(*id);
                        }
                        self.selected = if bs.additive {
                            hits.last().copied().or(self.selected)
                        } else {
                            hits.last().copied()
                        };
                        self.selection_anchor = self.selected;
                        self.mark_dirty();
                    }
                }
                if let Some((start, end)) = self.zoom_rect.take() {
                    let sel_rect = egui::Rect::from_two_pos(start, end);
                    if sel_rect.width() > 4.0 && sel_rect.height() > 4.0 {
                        self.zoom_to_screen_rect(rect, sel_rect);
                        self.zoom_rect_mode = false;
                    }
                }
                if self.drag_zone.take().is_some() {
                    self.persist_no_build_zone();
                }
                self.drag_state = None;
                if self.moving_target.take().is_some() {
                    self.mark_dirty();
                }
            }

            if !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && self.align_face_target.is_some()
                && let Some(pos) = resp.hover_pos()
            {
                if let Some(pick) = viewport::pick_face(&self.job, &self.camera, rect, pos, self.align_face_target) {
                    if self.hovered_face != Some((pick.instance_id, pick.tri_index)) {
                        let hl = viewport::build_face_highlight_scene(&pick, t);
                        if let Ok(mut guard) = self.hl_slot.lock() {
                            *guard = Some(hl);
                        }
                        self.hl_key = self.hl_key.wrapping_add(1).max(1);
                        self.hovered_face = Some((pick.instance_id, pick.tri_index));
                    }
                } else if self.hovered_face.is_some() {
                    if let Ok(mut guard) = self.hl_slot.lock() {
                        *guard = Some(GpuScene::default());
                    }
                    self.hl_key = self.hl_key.wrapping_add(1).max(1);
                    self.hovered_face = None;
                }
            }

            if !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && resp.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                self.viewport_ctx_menu = None;
                if let Some(target_id) = self.align_face_target {
                    if let Some(pick) = viewport::pick_face(&self.job, &self.camera, rect, pos, Some(target_id)) {
                        self.push_undo();
                        self.job.align_instance_face_to_neg_z(pick.instance_id, pick.local_normal);
                        self.job.clamp_instance_above_floor(pick.instance_id);
                        self.euler_edit.remove(&pick.instance_id);
                        self.mark_dirty();
                        self.clear_align_face_mode();
                        self.status = "Aligned selected face to -Z".to_owned();
                    }
                } else {
                    let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                    if ui.input(|i| i.modifiers.command) {
                        if let Some(iid) = hit {
                            self.toggle_select_instance(iid);
                        }
                    } else {
                        self.select_instance(hit);
                    }
                }
            }

            if !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && resp.secondary_clicked()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                if let Some(iid) = hit {
                    if !self.selected_set.contains(&iid) {
                        self.select_instance(Some(iid));
                    }
                    self.viewport_ctx_menu = Some(iid);
                } else {
                    self.viewport_ctx_menu = None;
                }
            }

            if self.viewport_ctx_menu.is_some() {
                let mut open_move = false;
                let mut open_scale = false;
                let mut open_sinterbox = false;
                let mut open_dup = false;
                let mut drop_floor = false;
                let mut center_xy = false;
                let mut rot_z = false;
                let mut toggle_hide = false;
                let mut toggle_transparent = false;
                let mut toggle_lock = false;
                let mut color_choice: Option<ColorMenuChoice> = None;
                let mut group_sel = false;
                let mut ungroup_sel = false;
                let mut assign_group: Option<GroupId> = None;
                let mut align_face = false;
                let mut shells_to_parts = false;
                let mut merge_parts = false;
                let mut export_sel = false;
                let mut delete_sel = false;

                let any_in_group = self
                    .job
                    .instances
                    .iter()
                    .any(|i| self.selected_set.contains(&i.id) && i.group_id.is_some());

                let is_hidden = self
                    .selected
                    .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
                    .is_some_and(|i| i.hidden);
                let is_transparent = self
                    .selected
                    .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
                    .is_some_and(|i| i.transparent);
                let is_locked = self
                    .selected
                    .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
                    .is_some_and(|i| i.locked);

                resp.context_menu(|ui| {
                    if ui.button("Move... (M)").clicked() {
                        open_move = true;
                        ui.close();
                    }
                    if ui.button("Scale... (Ctrl+S)").clicked() {
                        open_scale = true;
                        ui.close();
                    }
                    if ui.button("Duplicate... (Ctrl+D)").clicked() {
                        open_dup = true;
                        ui.close();
                    }
                    if ui.button("Drop to Min Z (Ctrl+Down)").clicked() {
                        drop_floor = true;
                        ui.close();
                    }
                    if ui.button("Center XY (C)").clicked() {
                        center_xy = true;
                        ui.close();
                    }
                    if ui.button("Rotate +90° Z (R)").clicked() {
                        rot_z = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(if is_hidden { "Show (H)" } else { "Hide (H)" }).clicked() {
                        toggle_hide = true;
                        ui.close();
                    }
                    if ui
                        .button(if is_transparent {
                            "Opaque"
                        } else {
                            "Transparent (50%)"
                        })
                        .clicked()
                    {
                        toggle_transparent = true;
                        ui.close();
                    }
                    if ui.button(if is_locked { "Unlock (L)" } else { "Lock (L)" }).clicked() {
                        toggle_lock = true;
                        ui.close();
                    }
                    if let Some(choice) = show_color_submenu(ui) {
                        color_choice = Some(choice);
                    }
                    if ui.button("Group Selected").clicked() {
                        group_sel = true;
                        ui.close();
                    }
                    if any_in_group && ui.button("Ungroup").clicked() {
                        ungroup_sel = true;
                        ui.close();
                    }
                    if !self.job.groups.is_empty() {
                        ui.menu_button("Assign to Group", |ui| {
                            for g in &self.job.groups {
                                if ui.button(&g.name).clicked() {
                                    assign_group = Some(g.id);
                                    ui.close();
                                }
                            }
                        });
                    }
                    ui.separator();
                    if icon_label_button(ui, "Align Face to Z", "sls_orient_lock", true, false).clicked() {
                        align_face = true;
                        ui.close();
                    }
                    if ui.button("Shells to Parts").clicked() {
                        shells_to_parts = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.selected_set.len() >= 2,
                            egui::Button::new("Merge Selected Parts"),
                        )
                        .clicked()
                    {
                        merge_parts = true;
                        ui.close();
                    }
                    if icon_label_button(ui, "Create Sinterbox...", "sls_sinterbox", true, false).clicked() {
                        open_sinterbox = true;
                        ui.close();
                    }
                    if ui.button(export_part_menu_label(self.selected_set.len())).clicked() {
                        export_sel = true;
                        ui.close();
                    }
                    if ui.button("Delete (Del)").clicked() {
                        delete_sel = true;
                        ui.close();
                    }
                });

                if open_move {
                    self.viewport_ctx_menu = None;
                    self.open_move_dialog();
                } else if open_scale {
                    self.viewport_ctx_menu = None;
                    self.open_scale_dialog();
                } else if open_sinterbox {
                    self.viewport_ctx_menu = None;
                    self.open_sinterbox_dialog();
                } else if open_dup {
                    self.viewport_ctx_menu = None;
                    self.open_duplicate_dialog();
                } else if drop_floor {
                    self.viewport_ctx_menu = None;
                    self.push_undo();
                    let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                    for id in ids {
                        self.job.drop_instance_to_floor(id);
                    }
                    self.mark_dirty();
                    self.status = "Dropped selection to Min Z".to_owned();
                } else if center_xy {
                    self.viewport_ctx_menu = None;
                    if let Some(sel) = self.selected {
                        self.push_undo();
                        if self.job.center_instance_xy(sel) {
                            self.mark_dirty();
                            self.status = "Centered part in XY".to_owned();
                        } else {
                            self.undo_stack.pop();
                        }
                    }
                } else if rot_z {
                    self.viewport_ctx_menu = None;
                    self.rotate_selected_90(2);
                } else if toggle_hide {
                    self.viewport_ctx_menu = None;
                    self.toggle_selected_hidden();
                } else if toggle_transparent {
                    self.viewport_ctx_menu = None;
                    self.toggle_selected_transparent();
                } else if toggle_lock {
                    self.viewport_ctx_menu = None;
                    self.toggle_selected_locked();
                } else if let Some(choice) = color_choice {
                    self.viewport_ctx_menu = None;
                    match choice {
                        ColorMenuChoice::OpenPicker => self.open_color_picker(),
                        ColorMenuChoice::Set(c) => self.set_selected_color(c),
                    }
                } else if group_sel {
                    self.viewport_ctx_menu = None;
                    self.group_selected_parts(String::new());
                } else if ungroup_sel {
                    self.viewport_ctx_menu = None;
                    self.ungroup_selected_parts();
                } else if let Some(gid) = assign_group {
                    self.viewport_ctx_menu = None;
                    self.push_undo();
                    let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                    for id in ids {
                        let _ = self.job.set_instance_group(id, Some(gid));
                    }
                    self.mark_dirty();
                } else if align_face {
                    self.viewport_ctx_menu = None;
                    if let Some(sel) = self.selected {
                        self.start_align_face_mode(sel);
                    }
                } else if shells_to_parts {
                    self.viewport_ctx_menu = None;
                    let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                    self.shells_to_parts_for(ids);
                } else if merge_parts {
                    self.viewport_ctx_menu = None;
                    self.merge_selected_parts();
                } else if export_sel {
                    self.viewport_ctx_menu = None;
                    self.export_stl_selected_only = true;
                    self.export_stl_dialog_open = true;
                } else if delete_sel {
                    self.viewport_ctx_menu = None;
                    self.delete_selected();
                }
            }

            self.sync_scene_if_dirty();

            let painter = ui.painter_at(rect);
            paint_background(&painter, rect, t.viewport_top, t.viewport_bottom);

            let ppp = ctx.pixels_per_point();
            let size_px = [(rect.width() * ppp).max(1.0), (rect.height() * ppp).max(1.0)];
            let aspect = f64::from(rect.width() / rect.height().max(1.0));
            let [w, d, h] = self.job.machine.size_mm();
            let radius = f64::from((w * w + d * d + h * h).sqrt()) * 2.0;
            let vp = self.camera.view_proj(aspect, radius);
            let e = self.camera.eye().to_f32();
            let back = self.camera.back().to_f32();

            let cb = ViewportCallback(Frame {
                product_lights: None,
                product: false,
                upload_error: None,
                key: self.scene_key,
                slot: self.scene_slot.clone(),
                sketch_key: 0,
                sketch_slot: self.empty_slot.clone(),
                hl_key: self.hl_key,
                hl_slot: self.hl_slot.clone(),
                pv_key: 0,
                pv_slot: self.empty_slot.clone(),
                pv_hide: true,
                preview_bounds: None,
                view_proj: vp.to_f32(),
                turned_depth: Some(vp.turned_depth_f32()),
                back,
                size_px,
                clip: Default::default(),
                clip_hatch: Default::default(),
                cap: [0.0; 4],
                images: Vec::new(),
                clouds: Vec::new(),
                analysis: [0.0; 8],
                analysis_colors: [0.0; 36],
                access: None,
                outline: true,
                eye: [e[0], e[1], e[2], 1.0],
                ao_radius: 0.0,
                ao_samples: 0,
                edge_px: 0.0,
                ssaa: 1.0,
                hover_tint: [0.08, 0.08, 0.08],
                selection_tint: [0.93, 0.70, 0.22],
                hatch: [[0.5, 0.5, 0.5]; solidcraft_viewport::hatch::HATCH_COLOURS],
                sky: [0.0, 0.0, 1.0, 1.0],
                moving: self.current_moving_layer(),
                motions: Default::default(),
                ground: None,
            });
            painter.add(egui_wgpu::Callback::new_paint_callback(rect, cb));

            if let Some(bs) = self.box_select {
                let sel_rect = egui::Rect::from_two_pos(bs.start, bs.current);
                painter.rect(
                    sel_rect,
                    0.0,
                    t.accent.gamma_multiply(0.16),
                    egui::Stroke::new(1.5, t.accent),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some((start, end)) = self.zoom_rect {
                let sel_rect = egui::Rect::from_two_pos(start, end);
                painter.rect(
                    sel_rect,
                    0.0,
                    t.accent.gamma_multiply(0.14),
                    egui::Stroke::new(1.5, t.accent),
                    egui::StrokeKind::Inside,
                );
            }

            if !centerpoints.is_empty() || rot_handle.is_some() {
                let p = ui.painter_at(rect);
                for ch in &centerpoints {
                    let is_hovered = hovered_center.is_some_and(|h| h.id == ch.id);
                    let fill = if ch.valid { t.accent } else { t.error };
                    let size = if is_hovered { 9.0 } else { 7.0 };
                    p.rect(
                        egui::Rect::from_center_size(ch.screen_pos, egui::vec2(size, size)),
                        1.0,
                        fill,
                        egui::Stroke::new(1.5, t.panel),
                        egui::StrokeKind::Outside,
                    );
                }
                if let Some((rot_pos, _)) = rot_handle {
                    let active = self.rotate_drag.is_some() || hovered_rot.is_some();
                    let r = if active { 10.5 } else { 9.0 };
                    let bg = if active { t.accent } else { t.panel.gamma_multiply(0.92) };
                    let fg = if active { t.selected_ink } else { t.accent };
                    p.circle(rot_pos, r, bg, egui::Stroke::new(1.5, t.accent));
                    // Draw a curved rotation arc + arrowhead inside the handle.
                    let arc_r = r * 0.54;
                    let steps = 12usize;
                    let start_a = -0.35_f32;
                    let end_a = 4.05_f32;
                    let pts: Vec<egui::Pos2> = (0..=steps)
                        .map(|i| {
                            let a = start_a + (end_a - start_a) * (i as f32 / steps as f32);
                            egui::pos2(rot_pos.x + arc_r * a.cos(), rot_pos.y + arc_r * a.sin())
                        })
                        .collect();
                    p.add(egui::Shape::line(pts, egui::Stroke::new(1.6, fg)));
                    let tip = egui::pos2(rot_pos.x + arc_r * end_a.cos(), rot_pos.y + arc_r * end_a.sin());
                    let tang = egui::vec2(-end_a.sin(), end_a.cos());
                    let rad = egui::vec2(end_a.cos(), end_a.sin());
                    p.add(egui::Shape::convex_polygon(
                        vec![tip + tang * 2.5, tip - tang * 1.8 + rad * 2.6, tip - tang * 1.8 - rad * 2.4],
                        fg,
                        egui::Stroke::NONE,
                    ));
                }
            }

            if rect.width() >= 140.0 && rect.height() >= 140.0 {
                let nav = view_compass::orbit(ui, &mut self.compass_drag, ui.rect_contains_pointer(rect), |p| {
                    p.distance(compass_centre) <= 49.5
                });
                if let Some(step) = nav.step {
                    self.camera.orbit_unlimited_through_roll(-step.dx, step.dy);
                }
                let (right, up, back_v) = self.camera.basis();
                let compass = Compass {
                    clip: rect,
                    centre: compass_centre,
                    axes: ViewAxes {
                        right: right.into(),
                        up: up.into(),
                        back: back_v.into(),
                    },
                    hover: ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p)),
                    orbiting: nav.orbiting,
                    snap_hint: false,
                    home_label: "Reset isometric view".to_owned(),
                };
                match view_compass::show(ui, &compass) {
                    Some(ViewAction::Snap { direction, .. }) => {
                        let dir = if self.camera.back().dot(direction.into()) > 0.9999 {
                            [-direction[0], -direction[1], -direction[2]]
                        } else {
                            direction
                        };
                        self.camera = self.camera.looking_from(dir.into());
                    }
                    Some(ViewAction::Home) => {
                        self.camera = viewport::default_camera_with_origin(&self.job.machine, self.job.bed_origin);
                    }
                    None => {}
                }
            }

            if rect.width() >= 260.0 && rect.height() >= 80.0 {
                let current_view = self.current_standard_view();
                let bar_max_rect = egui::Rect::from_min_max(
                    rect.left_top() + egui::vec2(10.0, 10.0),
                    egui::pos2((rect.right() - 120.0).max(rect.left() + 220.0), rect.top() + 46.0),
                );
                let bar_resp = ui.scope_builder(egui::UiBuilder::new().max_rect(bar_max_rect), |ui| {
                    egui::Frame::new()
                        .fill(t.panel.gamma_multiply(0.92))
                        .stroke(egui::Stroke::new(1.0, t.border))
                        .corner_radius(t.radius_sm)
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let hint = current_view.map_or("Custom", view_label);
                                ui.label(egui::RichText::new(format!("View: {hint}")).small().strong().color(t.text))
                                    .backing_on_hover_text("Current camera view — press Space to cycle, or right-drag to orbit freely");
                                ui.separator();
                                for &v in &VIEW_CYCLE {
                                    let active = current_view == Some(v);
                                    let text = egui::RichText::new(view_label(v)).small().color(if active {
                                        t.accent
                                    } else {
                                        t.text_dim
                                    });
                                    if ui
                                        .selectable_label(active, text)
                                        .backing_on_hover_text(format!("Switch to {} view (Space cycles)", view_label(v)))
                                        .clicked()
                                    {
                                        self.set_standard_view(v);
                                    }
                                }
                            });
                        })
                        .response
                });
                self.view_bar_rect = Some(bar_resp.inner.rect);
            } else {
                self.view_bar_rect = None;
            }

            let faulty_ids = self.faulty_placed_instance_ids();
            if !faulty_ids.is_empty() && rect.width() >= 180.0 && rect.height() >= 120.0 {
                let badge_max_rect = egui::Rect::from_min_max(
                    rect.right_bottom() - egui::vec2(220.0, 52.0),
                    rect.right_bottom() - egui::vec2(12.0, 12.0),
                );
                let badge_resp = ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(badge_max_rect)
                        .layout(egui::Layout::right_to_left(egui::Align::Max)),
                    |ui| {
                        let frame_resp = egui::Frame::new()
                            .fill(t.panel.gamma_multiply(0.94))
                            .stroke(egui::Stroke::new(1.25, t.error))
                            .corner_radius(t.radius_sm)
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (icon_rect, _) =
                                        ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::hover());
                                    let p = ui.painter();
                                    let tri = vec![
                                        egui::pos2(icon_rect.center().x, icon_rect.top() + 1.0),
                                        egui::pos2(icon_rect.right() - 1.0, icon_rect.bottom() - 1.0),
                                        egui::pos2(icon_rect.left() + 1.0, icon_rect.bottom() - 1.0),
                                    ];
                                    p.add(egui::Shape::convex_polygon(
                                        tri,
                                        t.error,
                                        egui::Stroke::new(1.0, t.error),
                                    ));
                                    p.text(
                                        icon_rect.center() + egui::vec2(0.0, 1.5),
                                        egui::Align2::CENTER_CENTER,
                                        "!",
                                        egui::FontId::proportional(11.0),
                                        t.selected_ink,
                                    );
                                    ui.label(
                                        egui::RichText::new(format!("{} mesh error(s)", faulty_ids.len()))
                                            .small()
                                            .strong()
                                            .color(t.error),
                                    );
                                });
                            });
                        let click_resp = ui.interact(
                            frame_resp.response.rect,
                            ui.scope_id().with("sls_repair_gate_badge"),
                            egui::Sense::click(),
                        );
                        let tip = format!(
                            "{} placed part(s) have non-manifold, open-edge, or winding errors. Click to select faulty part(s).",
                            faulty_ids.len()
                        );
                        if click_resp.backing_on_hover_text(tip).clicked() {
                            self.selected_set.clear();
                            for &id in &faulty_ids {
                                self.selected_set.insert(id);
                            }
                            self.selected = faulty_ids.last().copied();
                            self.selection_anchor = self.selected;
                            self.status = format!("Selected {} part(s) with mesh errors", faulty_ids.len());
                        }
                        frame_resp.response
                    },
                );
                self.repair_badge_rect = Some(badge_resp.inner.rect);
            } else {
                self.repair_badge_rect = None;
            }
        });
    }
}

impl eframe::App for NestCraftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let window_rect = ui.max_rect();
        solidcraft_theme::apply(&ctx);
        let t = Tokens::get();

        self.poll_load_task(&ctx);
        self.handle_shortcuts(&ctx);
        self.handle_dropped_files(&ctx);
        self.draw_top_bar(ui, &ctx, &t);
        self.draw_edit_bar(ui, &ctx, &t);
        self.draw_analyze_bar(ui, &ctx, &t);
        self.draw_view_bar(ui, &ctx, &t);
        self.draw_bottom_bar(ui, &ctx, &t);
        self.draw_parts_browser(ui, &ctx, &t);
        self.draw_inspector(ui, &ctx, &t);
        self.draw_viewport(ui, &ctx, &t);
        self.draw_custom_platform_modal(&ctx);
        self.draw_duplicate_modal(&ctx);
        self.draw_move_modal(&ctx);
        self.draw_scale_modal(&ctx);
        self.draw_sinterbox_modal(&ctx);
        self.draw_color_picker_modal(&ctx);
        self.draw_export_stl_modal(&ctx);
        self.draw_below_zero_import_modal(&ctx);
        self.draw_keep_delete_modal(&ctx);
        self.draw_load_progress_modal(&ctx);
        self.draw_drop_overlay(&ctx, window_rect, &t);
    }
}

fn rotate_transform_around_pivot(
    orig: [[f32; 4]; 3],
    pivot: [f32; 3],
    axis: [f32; 3],
    angle_rad: f32,
) -> [[f32; 4]; 3] {
    let len = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
    if len < 1e-6 || angle_rad.abs() < 1e-6 {
        return orig;
    }
    let (ux, uy, uz) = (axis[0] / len, axis[1] / len, axis[2] / len);
    let (s, c) = angle_rad.sin_cos();
    let t = 1.0 - c;
    let r = [
        [t * ux * ux + c, t * ux * uy - s * uz, t * ux * uz + s * uy],
        [t * ux * uy + s * uz, t * uy * uy + c, t * uy * uz - s * ux],
        [t * ux * uz - s * uy, t * uy * uz + s * ux, t * uz * uz + c],
    ];
    let mut out = [[0.0; 4]; 3];
    for row in 0..3 {
        for col in 0..3 {
            out[row][col] = r[row][0] * orig[0][col] + r[row][1] * orig[1][col] + r[row][2] * orig[2][col];
        }
        let dx = orig[0][3] - pivot[0];
        let dy = orig[1][3] - pivot[1];
        let dz = orig[2][3] - pivot[2];
        out[row][3] = pivot[row] + r[row][0] * dx + r[row][1] * dy + r[row][2] * dz;
    }
    out
}

fn snap_to_nearest_axis(v: [f32; 3]) -> [f32; 3] {
    let (ax, ay, az) = (v[0].abs(), v[1].abs(), v[2].abs());
    if ax >= ay && ax >= az {
        [v[0].signum(), 0.0, 0.0]
    } else if ay >= az {
        [0.0, v[1].signum(), 0.0]
    } else {
        [0.0, 0.0, v[2].signum()]
    }
}

fn create_sample_bracket_mesh(variant: u32) -> nestcraft_core::Result<TriMesh> {
    let scale = 1.0 + 0.15 * f32::from((variant % 3) as u8);
    let (w, d, h) = (30.0 * scale, 20.0 * scale, 25.0 * scale);
    let positions = vec![
        [0.0, 0.0, 0.0],
        [w, 0.0, 0.0],
        [w, d, 0.0],
        [0.0, d, 0.0],
        [0.0, 0.0, h],
        [w, 0.0, h],
        [w, d, h],
        [0.0, d, h],
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
    TriMesh::from_indexed(positions, indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_part_menu_label_pluralizes_for_multi_selection() {
        assert_eq!(export_part_menu_label(0), "Export Part...");
        assert_eq!(export_part_menu_label(1), "Export Part...");
        assert_eq!(export_part_menu_label(2), "Export Parts...");
    }

    #[test]
    fn view_zoom_helpers_frame_parts_platform_and_screen_rect() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        app.job.instances[0].set_translation([10.0, 10.0, 0.0]);

        app.zoom_to_selected();
        let selected_dist = app.camera.distance;

        app.zoom_to_platform();
        let platform_dist = app.camera.distance;
        assert!(platform_dist > selected_dist);

        app.zoom_to_all_parts();
        assert!((app.camera.distance - selected_dist).abs() < 1e-4);

        app.zoom_to_all();
        assert!(app.camera.distance >= platform_dist - 1e-4);

        let viewport = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let half_rect = egui::Rect::from_center_size(viewport.center(), egui::vec2(400.0, 300.0));
        let before_dist = app.camera.distance;
        app.zoom_to_screen_rect(viewport, half_rect);
        assert!((app.camera.distance - before_dist * 0.5).abs() < 1e-4);
        let _ = iid;
    }

    #[test]
    fn app_initializes_and_syncs_scene() {
        let mut app = NestCraftApp::new();
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert!(app.scene_slot.lock().is_ok_and(|g| g.is_some()));
    }

    #[test]
    fn dragging_instance_uses_moving_layer_without_dirtying_scene() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        let start = app.job.instances[0].translation();
        app.sync_scene_if_dirty();
        let key_before = app.scene_key;

        app.drag_state = Some(DragState {
            anchor_id: iid,
            instances: vec![(iid, start)],
            start_hit: start,
            plane_z: start[2],
        });
        app.start_moving_layer(viewport::DragExclude::Instances(vec![iid]));
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert_ne!(app.scene_key, key_before);
        let key_during_drag = app.scene_key;

        // Simulated drag frame: update translation without marking scene dirty.
        app.job.instances[0].set_translation([start[0] + 15.0, start[1] - 5.0, start[2] + 3.0]);
        assert!(!app.scene_dirty);
        app.sync_scene_if_dirty();
        assert_eq!(app.scene_key, key_during_drag);

        let mv = app.current_moving_layer().expect("moving layer active");
        assert_eq!(mv.key, app.moving_key);
        assert!((mv.transform[3][0] - 15.0).abs() < 1e-4);
        assert!((mv.transform[3][1] + 5.0).abs() < 1e-4);
        assert!((mv.transform[3][2] - 3.0).abs() < 1e-4);
    }

    #[test]
    fn sample_part_and_90_deg_rotation_update_scene() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        assert_eq!(app.job.instances.len(), 1);
        let iid = app.selected.expect("selected");
        app.rotate_selected_90(0);
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert!(app.job.drop_instance_to_floor(iid));
        assert!(app.job.is_instance_in_bounds(iid));
    }

    #[test]
    fn shortcut_helpers_nudge_duplicate_hide_lock_and_delete() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        assert!(app.selected.is_some());
        let before = app.job.instances[0].translation();
        app.nudge_selected([5.0, -3.0, 2.0]);
        let after = app.job.instances[0].translation();
        assert!((after[0] - (before[0] + 5.0)).abs() < 1e-4);
        assert!((after[1] - (before[1] - 3.0)).abs() < 1e-4);
        assert!((after[2] - (before[2] + 2.0)).abs() < 1e-4);

        app.toggle_selected_hidden();
        assert!(app.job.instances[0].hidden);
        app.toggle_selected_transparent();
        assert!(app.job.instances[0].transparent);
        app.toggle_selected_transparent();
        assert!(!app.job.instances[0].transparent);
        app.set_selected_color(Some([0x22, 0xAA, 0xFF]));
        assert_eq!(app.job.instances[0].color, Some([0x22, 0xAA, 0xFF]));
        app.set_selected_color(None);
        assert_eq!(app.job.instances[0].color, None);
        app.toggle_selected_locked();
        assert!(app.job.instances[0].locked);

        app.dup_axes = [true, false, false];
        app.dup_counts = [3, 1, 1];
        app.dup_exact_total = false;
        app.duplicate_selected();
        assert_eq!(app.job.instances.len(), 3);
        app.delete_selected();
        assert_eq!(app.job.instances.len(), 2);
    }

    #[test]
    fn import_files_loads_multiple_stls_and_reports_partial_failures() {
        let dir = std::env::temp_dir().join(format!("nestcraft_multi_import_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let ascii_stl = b"solid tri\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 10 0 0\nvertex 0 10 0\nendloop\nendfacet\nendsolid tri\n";
        let stl_a = dir.join("part_a.stl");
        let stl_b = dir.join("part_b.STL");
        let bad = dir.join("notes.txt");
        assert!(std::fs::write(&stl_a, ascii_stl).is_ok());
        assert!(std::fs::write(&stl_b, ascii_stl).is_ok());
        assert!(std::fs::write(&bad, b"not a mesh").is_ok());

        let mut app = NestCraftApp::new();
        app.scene_dirty = false;
        app.import_files(&[&stl_a, &stl_b]);
        assert_eq!(app.job.instances.len(), 2);
        assert!(app.scene_dirty);
        assert!(app.status.contains("Imported 2 item(s) from 2 file(s)"), "status: {}", app.status);

        app.import_files(&[&stl_a, &bad]);
        assert_eq!(app.job.instances.len(), 3);
        assert!(app.status.contains("1 failed"), "status: {}", app.status);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_cycle_and_hint_track_camera_orientation() {
        let mut app = NestCraftApp::new();
        assert_eq!(app.current_standard_view(), Some(StandardView::Iso));

        app.cycle_standard_view();
        assert_eq!(app.current_standard_view(), Some(StandardView::Top));
        app.cycle_standard_view();
        assert_eq!(app.current_standard_view(), Some(StandardView::Bottom));

        app.camera.orbit_unlimited_through_roll(40.0, 25.0);
        assert_eq!(app.current_standard_view(), None);

        app.cycle_standard_view();
        assert_eq!(app.current_standard_view(), Some(StandardView::Iso));
    }

    #[test]
    fn side_views_map_arrows_to_z_and_screen_horizontal() {
        let mut app = NestCraftApp::new();
        assert_eq!(app.arrow_nudge_axes(), ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]));

        app.set_standard_view(StandardView::Front);
        assert_eq!(app.arrow_nudge_axes(), ([1.0, 0.0, 0.0], [0.0, 0.0, 1.0]));

        app.set_standard_view(StandardView::Back);
        assert_eq!(app.arrow_nudge_axes(), ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]));

        app.set_standard_view(StandardView::Right);
        assert_eq!(app.arrow_nudge_axes(), ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0]));

        app.set_standard_view(StandardView::Left);
        assert_eq!(app.arrow_nudge_axes(), ([0.0, -1.0, 0.0], [0.0, 0.0, 1.0]));

        app.set_standard_view(StandardView::Top);
        assert_eq!(app.arrow_nudge_axes(), ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]));
    }

    #[test]
    fn compact_distribution_recalculates_grid_across_enabled_axes() {
        assert_eq!(distribute_compact_grid(6, [true, true, false]), [3, 2, 1]);
        assert_eq!(distribute_compact_grid(12, [true, true, true]), [3, 2, 2]);
        assert_eq!(distribute_compact_grid(5, [false, true, false]), [1, 5, 1]);
    }

    #[test]
    fn ctrl_click_and_shift_click_multi_select_in_tree() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        app.add_sample_part();
        let i3 = app.selected.expect("i3");
        app.add_sample_part();
        let i4 = app.selected.expect("i4");

        let order = vec![i1, i2, i3, i4];
        app.select_instance(Some(i1));
        assert_eq!(app.selected_set.len(), 1);

        // Shift+Click i3 selects range i1..=i3.
        app.select_tree_range(&order, i3);
        assert_eq!(app.selected_set, HashSet::from([i1, i2, i3]));

        // Ctrl+Click toggles i2 off and i4 on.
        app.toggle_select_instance(i2);
        assert_eq!(app.selected_set, HashSet::from([i1, i3]));
        app.toggle_select_instance(i4);
        assert_eq!(app.selected_set, HashSet::from([i1, i3, i4]));

        // Delete removes all selected instances.
        app.delete_selected();
        assert_eq!(app.job.instances.len(), 1);
        assert_eq!(app.job.instances[0].id, i2);
    }

    #[test]
    fn no_build_zone_config_and_stl_roundtrip_in_dir() {
        let dir = std::env::temp_dir().join(format!("nestcraft_nbz_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let ascii_stl = b"solid nbz\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 10 0 0\nvertex 0 10 0\nendloop\nendfacet\nendsolid nbz\n";
        assert!(std::fs::write(dir.join("zone.stl"), ascii_stl).is_ok());

        let mesh = Arc::new(stl::parse_stl(ascii_stl).expect("valid stl"));
        let mut zone = NoBuildZone::new("Recoater Keep-Out", mesh);
        zone.position = [15.0, 20.0, 5.0];
        zone.rotation_deg = [0.0, 0.0, 90.0];
        zone.clearance_mm = 3.5;
        zone.enabled = true;
        zone.visible = false;

        assert!(save_no_build_zone_config_to_dir(&dir, &zone).is_ok());
        let loaded = load_no_build_zone_from_dir(&dir).expect("loaded zone");
        assert_eq!(loaded.name, "Recoater Keep-Out");
        assert_eq!(loaded.position, [15.0, 20.0, 5.0]);
        assert_eq!(loaded.rotation_deg, [0.0, 0.0, 90.0]);
        assert!((loaded.clearance_mm - 3.5).abs() < 1e-4);
        assert!(loaded.enabled);
        assert!(!loaded.visible);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn machines_config_roundtrip_and_min_z_floor_work() {
        let dir = std::env::temp_dir().join(format!("nestcraft_machines_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut presets = default_machine_presets();
        presets[0].size_mm = [165.0, 215.0, 325.0];
        presets[0].wall_margin_mm = 7.5;
        presets[0].min_z_mm = 2.0;
        presets[0].clearance_mm = 3.0;
        assert!(save_machine_presets_to_dir(&dir, &presets).is_ok());
        let loaded = load_machine_presets_from_dir(&dir).expect("loaded presets");
        assert_eq!(loaded, presets);
        let _ = std::fs::remove_dir_all(&dir);

        let mut app = NestCraftApp::new();
        app.job.min_z_mm = 5.0;
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        assert!(app.job.is_instance_above_floor(iid));
        app.nudge_selected([0.0, 0.0, -10.0]);
        assert!(app.job.is_instance_above_floor(iid));
        let wb = app.job.instances[0].world_bounds(app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds);
        assert!((wb[0][2] - 5.0).abs() < 1e-3);
    }

    #[test]
    fn move_selection_supports_relative_and_absolute_modes() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");

        // Select both i1 and i2 (with i2 as primary anchor).
        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);
        let p1_before = app.job.instances[0].translation();
        let p2_before = app.job.instances[1].translation();

        // Relative move by (+10, -5, +4)
        app.apply_move_selection(false, [10.0, -5.0, 4.0]);
        let p1_rel = app.job.instances[0].translation();
        let p2_rel = app.job.instances[1].translation();
        assert!((p1_rel[0] - (p1_before[0] + 10.0)).abs() < 1e-4);
        assert!((p2_rel[1] - (p2_before[1] - 5.0)).abs() < 1e-4);
        assert!((p2_rel[2] - (p2_before[2] + 4.0)).abs() < 1e-4);

        // Absolute move places anchor i2 at (50, 60, 10) and preserves relative offset to i1.
        app.apply_move_selection(true, [50.0, 60.0, 10.0]);
        let p1_abs = app.job.instances[0].translation();
        let p2_abs = app.job.instances[1].translation();
        assert!((p2_abs[0] - 50.0).abs() < 1e-4);
        assert!((p2_abs[1] - 60.0).abs() < 1e-4);
        assert!((p2_abs[2] - 10.0).abs() < 1e-4);
        assert!(((p2_abs[0] - p1_abs[0]) - (p2_rel[0] - p1_rel[0])).abs() < 1e-4);
    }

    #[test]
    fn save_bed_and_export_individual_stls_work() {
        let dir = std::env::temp_dir().join(format!("nestcraft_mvp_io_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let mut app = NestCraftApp::new();
        app.add_sample_part();
        app.add_sample_part();
        app.export_stl_prefix = "job_שלום".to_owned();
        app.export_stl_suffix = "_sls!".to_owned();
        app.export_stl_include_id = true;

        let exported = app.export_individual_stls(&dir).expect("exported stls");
        assert_eq!(exported, 2);
        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.is_ascii()
            && !f.contains('#')
            && !f.contains(' ')
            && f.starts_with("job_")
            && f.ends_with("_sls_.stl")));

        // Switching machines does not carry over the active machine's No-Build Zone.
        let mesh = app.job.parts[0].mesh.clone();
        app.job.no_build_zone = Some(NoBuildZone::new("P110 Zone", mesh));
        app.apply_machine_slot(1);
        assert_ne!(
            app.job.no_build_zone.as_ref().map(|z| z.name.as_str()),
            Some("P110 Zone")
        );

        let bed_path = dir.join("test_bed.nestc");
        app.save_bed_file(&bed_path);
        assert!(bed_path.exists());

        let mut app2 = NestCraftApp::new();
        app2.open_bed_file(&bed_path);
        assert_eq!(app2.job.instances.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn async_load_task_processes_stl_files_and_tracks_progress() {
        let dir = std::env::temp_dir().join(format!("nestcraft_async_load_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let ascii_stl = b"solid tri\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 10 0 0\nvertex 0 10 0\nendloop\nendfacet\nendsolid tri\n";
        let stl_a = dir.join("a.stl");
        let stl_b = dir.join("b.stl");
        assert!(std::fs::write(&stl_a, ascii_stl).is_ok());
        assert!(std::fs::write(&stl_b, ascii_stl).is_ok());

        let ctx = egui::Context::default();
        let mut app = NestCraftApp::new();
        app.start_import_files(vec![stl_a, stl_b], &ctx);
        assert!(app.load_task.is_some());

        for _ in 0..100 {
            app.poll_load_task(&ctx);
            if app.load_task.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.load_task.is_none());
        assert_eq!(app.job.instances.len(), 2);
        assert!(app.below_zero_import_dialog.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn importing_stl_preserves_model_location_and_prompts_below_zero() {
        let dir = std::env::temp_dir().join(format!("nestcraft_model_loc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let above_stl = dir.join("above.stl");
        let above_txt = b"solid tri\nfacet normal 0 0 1\nouter loop\nvertex 40 55 12\nvertex 50 55 12\nvertex 40 65 22\nendloop\nendfacet\nendsolid tri\n";
        assert!(std::fs::write(&above_stl, above_txt).is_ok());

        let below_stl = dir.join("below.stl");
        let below_txt = b"solid tri\nfacet normal 0 0 1\nouter loop\nvertex 40 55 -8\nvertex 50 55 -8\nvertex 40 65 2\nendloop\nendfacet\nendsolid tri\n";
        assert!(std::fs::write(&below_stl, below_txt).is_ok());

        let mut app = NestCraftApp::new();
        assert!(app.import_single_file(&above_stl).is_ok());
        assert!(app.below_zero_import_dialog.is_empty());
        let id_above = app.job.instances[0].id;
        assert!((app.job.instance_min_z(id_above).expect("min_z") - 12.0).abs() < 1e-4);

        assert!(app.import_single_file(&below_stl).is_ok());
        assert_eq!(app.below_zero_import_dialog.len(), 1);
        let id_below = app.job.instances[1].id;
        assert!((app.job.instance_min_z(id_below).expect("min_z") + 8.0).abs() < 1e-4);

        app.reset_below_zero_imported_parts();
        assert!(app.below_zero_import_dialog.is_empty());
        assert!(app.job.instance_min_z(id_below).expect("min_z").abs() < 1e-4);
        let proto = app.job.part(app.job.instances[1].part_id).expect("part");
        let wb = app.job.instances[1].world_bounds(proto.mesh.bounds);
        assert!((wb[0][0] - 40.0).abs() < 1e-4);
        assert!((wb[0][1] - 55.0).abs() < 1e-4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_and_redo_restore_unlimited_steps() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        let p0 = app.job.instances[0].translation();

        app.nudge_selected([10.0, 0.0, 0.0]);
        let p1 = app.job.instances[0].translation();
        assert!((p1[0] - (p0[0] + 10.0)).abs() < 1e-4);

        app.toggle_selected_transparent();
        assert!(app.job.instances[0].transparent);

        app.delete_selected();
        assert!(app.job.instances.is_empty());

        // Undo delete -> transparent -> nudge -> add_sample_part
        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 1);
        assert_eq!(app.job.instances[0].id, iid);
        assert!(app.job.instances[0].transparent);

        assert!(app.undo());
        assert!(!app.job.instances[0].transparent);

        assert!(app.undo());
        assert!((app.job.instances[0].translation()[0] - p0[0]).abs() < 1e-4);

        assert!(app.undo());
        assert!(app.job.instances.is_empty());

        // Stack exhausted
        assert!(!app.undo());

        // Redo all the way back
        assert!(app.redo());
        assert_eq!(app.job.instances.len(), 1);
        assert!(app.redo());
        assert!((app.job.instances[0].translation()[0] - (p0[0] + 10.0)).abs() < 1e-4);
        assert!(app.redo());
        assert!(app.job.instances[0].transparent);
        assert!(app.redo());
        assert!(app.job.instances.is_empty());
        assert!(!app.redo());
    }

    #[test]
    fn merge_selected_parts_and_shells_to_parts_roundtrip_with_undo() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        assert_eq!(app.job.instances.len(), 2);

        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);
        app.merge_selected_parts();
        assert_eq!(app.job.instances.len(), 1);
        let merged_id = app.selected.expect("merged");

        app.shells_to_parts_for(vec![merged_id]);
        assert_eq!(app.job.instances.len(), 2);
        assert_eq!(app.selected_set.len(), 2);

        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 1);
        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 2);

        // Merging with keep_sources = true preserves originals (2 + 1 = 3).
        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);
        app.merge_selected_parts_with_keep(&[i1, i2], true);
        assert_eq!(app.job.instances.len(), 3);
        let kept_merged_id = app.selected.expect("kept_merged");

        // Splitting with keep_source = true preserves the merged source (3 + 2 = 5).
        app.shells_to_parts_for_with_keep(vec![kept_merged_id], true);
        assert_eq!(app.job.instances.len(), 5);
        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 3);
    }

    #[test]
    fn faulty_placed_instance_ids_flags_non_watertight_meshes() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        assert!(app.faulty_placed_instance_ids().is_empty());

        // Single open triangle is non-watertight and must be flagged.
        let open_mesh = TriMesh::from_indexed(
            vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]],
            vec![[0, 1, 2]],
        )
        .expect("open triangle mesh");
        let pid = app.job.add_part("OpenTriangle".to_owned(), Arc::new(open_mesh));
        let faulty_id = app.job.add_instance_on_floor_next(pid).expect("instance");
        assert_eq!(app.faulty_placed_instance_ids(), vec![faulty_id]);
    }

    #[test]
    fn rotate_transform_around_pivot_rotates_90_deg_around_z() {
        let orig = [
            [1.0, 0.0, 0.0, 20.0],
            [0.0, 1.0, 0.0, 10.0],
            [0.0, 0.0, 1.0, 5.0],
        ];
        let pivot = [10.0, 10.0, 5.0];
        let rotated = rotate_transform_around_pivot(orig, pivot, [0.0, 0.0, 1.0], std::f32::consts::FRAC_PI_2);
        assert!((rotated[0][3] - 10.0).abs() < 1e-4);
        assert!((rotated[1][3] - 20.0).abs() < 1e-4);
        assert!((rotated[2][3] - 5.0).abs() < 1e-4);
    }

    #[test]
    fn rotating_off_origin_part_keeps_its_own_centroid_fixed() {
        let mut app = NestCraftApp::new();
        // Box with local coordinates far from (0, 0, 0): [100..120, 50..70, 10..30]
        let mesh = TriMesh::from_indexed(
            vec![
                [100.0, 50.0, 10.0],
                [120.0, 50.0, 10.0],
                [120.0, 70.0, 10.0],
                [100.0, 70.0, 10.0],
                [100.0, 50.0, 30.0],
                [120.0, 50.0, 30.0],
                [120.0, 70.0, 30.0],
                [100.0, 70.0, 30.0],
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
        .expect("valid box");
        let pid = app.job.add_part("off_origin", Arc::new(mesh));
        let iid = app.job.add_instance(pid, IDENTITY_TRANSFORM).expect("placed");
        app.select_instance(Some(iid));

        app.rotate_selected_90(2);
        let proto = app.job.part(pid).expect("part");
        let wb = app.job.instances[0].world_bounds(proto.mesh.bounds);
        let cx = (wb[0][0] + wb[1][0]) * 0.5;
        let cy = (wb[0][1] + wb[1][1]) * 0.5;
        let cz = (wb[0][2] + wb[1][2]) * 0.5;
        assert!((cx - 110.0).abs() < 1e-3, "cx={cx}");
        assert!((cy - 60.0).abs() < 1e-3, "cy={cy}");
        assert!((cz - 20.0).abs() < 1e-3, "cz={cz}");

        app.rotate_selected_by(0, 45.0);
        let euler = app.job.instances[0].euler_deg();
        assert!((euler[0] - 45.0).abs() < 1e-3);
        assert!((euler[2] - 90.0).abs() < 1e-3);
    }

    #[test]
    fn render_part_thumbnail_produces_shaded_20x20_image() {
        let mesh = create_sample_bracket_mesh(0).expect("sample mesh");
        let img = render_part_thumbnail(&mesh);
        assert_eq!(img.size, [THUMB_PX, THUMB_PX]);
        let opaque_pixels = img.pixels.iter().filter(|c| c.a() == 255).count();
        assert!(opaque_pixels > 20 && opaque_pixels < THUMB_PX * THUMB_PX);
    }

    #[test]
    fn group_and_ungroup_selected_parts_with_undo() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);

        app.group_selected_parts("My Group".to_owned());
        assert_eq!(app.job.groups.len(), 1);
        assert!(app.job.instances.iter().all(|i| i.group_id.is_some()));

        app.ungroup_selected_parts();
        assert!(app.job.instances.iter().all(|i| i.group_id.is_none()));

        assert!(app.undo());
        assert!(app.job.instances.iter().all(|i| i.group_id.is_some()));
        assert!(app.undo());
        assert!(app.job.groups.is_empty());
        assert!(app.job.instances.iter().all(|i| i.group_id.is_none()));

        app.job.create_group("Empty 1");
        app.job.create_group("Empty 2");
        assert_eq!(app.job.remove_empty_groups(), 2);
        assert!(app.job.groups.is_empty());
    }

    #[test]
    fn align_instance_face_to_neg_z_rotates_top_face_downward() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        let bounds = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
        let top_before = app.job.instances[0].world_bounds(bounds)[1][2];

        // Aligning +Z top face to -Z flips the part upside down by 180 deg.
        assert!(app.job.align_instance_face_to_neg_z(iid, [0.0, 0.0, 1.0]));
        let m = app.job.instances[0].transform;
        let wn_z = m[2][2];
        assert!((wn_z + 1.0).abs() < 1e-4, "expected -1.0, got {wn_z}");
        let top_after = app.job.instances[0].world_bounds(bounds)[1][2];
        assert!((top_before - top_after).abs() < 1e-3);
    }

    #[test]
    fn scaling_part_supports_uniform_per_axis_and_imperial_metric_with_undo() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        let dims0 = {
            let b = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
            app.job.instances[0].world_bounds(b)
        };
        let dx0 = dims0[1][0] - dims0[0][0];
        let dy0 = dims0[1][1] - dims0[0][1];
        let dz0 = dims0[1][2] - dims0[0][2];

        // Per-axis scale (2x X, 1x Y, 0.5x Z)
        app.apply_scale_selection([2.0, 1.0, 0.5]);
        let b1 = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
        let wb1 = app.job.instances[0].world_bounds(b1);
        assert!(((wb1[1][0] - wb1[0][0]) - dx0 * 2.0).abs() < 1e-3);
        assert!(((wb1[1][1] - wb1[0][1]) - dy0).abs() < 1e-3);
        assert!(((wb1[1][2] - wb1[0][2]) - dz0 * 0.5).abs() < 1e-3);

        // Imperial -> Metric (x 25.4)
        app.apply_scale_selection([25.4, 25.4, 25.4]);
        let b2 = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
        let wb2 = app.job.instances[0].world_bounds(b2);
        assert!(((wb2[1][0] - wb2[0][0]) - dx0 * 2.0 * 25.4).abs() < 1e-2);

        // Undo back to original size
        assert!(app.undo());
        assert!(app.undo());
        let b_undo = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
        let wb_undo = app.job.instances[0].world_bounds(b_undo);
        assert!(((wb_undo[1][0] - wb_undo[0][0]) - dx0).abs() < 1e-3);
        let _ = iid;
    }

    #[test]
    fn create_sinterbox_groups_multi_selection_and_undoes_cleanly() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);

        app.open_sinterbox_dialog();
        assert!(app.sinterbox_dialog_open);
        assert_eq!(app.sinterbox_label_text, "Sample Bracket #1");

        app.apply_create_sinterbox();
        assert_eq!(app.job.groups.len(), 1);
        assert_eq!(app.job.instances.len(), 3);
        let gid = app.job.groups[0].id;
        assert!(app.job.instances.iter().all(|i| i.group_id == Some(gid)));

        let cage_inst = app.job.instances.last().expect("cage");
        assert!(cage_inst.transparent);
        let cage_mesh = &app.job.part(cage_inst.part_id).expect("cage part").mesh;
        assert!(manifold::analyze_mesh(cage_mesh).is_watertight_manifold());

        assert!(app.undo());
        assert!(app.job.groups.is_empty());
        assert_eq!(app.job.instances.len(), 2);
    }

    #[test]
    fn create_sinterbox_single_part_with_multiplication_creates_batch_and_group() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        assert_eq!(app.selected_set.len(), 1);

        app.open_sinterbox_dialog();
        app.sinterbox_single_copies = 4;
        app.sinterbox_shape_idx = 1; // Tight Fit
        app.apply_create_sinterbox();

        assert_eq!(app.job.groups.len(), 1);
        assert_eq!(app.job.instances.len(), 5); // 4 parts + 1 cage
        let gid = app.job.groups[0].id;
        assert!(app.job.instances.iter().all(|i| i.group_id == Some(gid)));

        let cage_inst = app.job.instances.last().expect("cage");
        let cage_mesh = &app.job.part(cage_inst.part_id).expect("cage part").mesh;
        assert!(manifold::analyze_mesh(cage_mesh).is_watertight_manifold());

        assert!(app.undo());
        assert!(app.job.groups.is_empty());
        assert_eq!(app.job.instances.len(), 1);
    }
}
