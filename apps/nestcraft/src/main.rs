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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use nestcraft_core::io::threemf::ThreeMfLoadedItem;
use nestcraft_core::io::{nestc, stl, threemf};
use nestcraft_core::manifold::{self, ManifoldReport};
use nestcraft_core::model::{
    BedOrigin, BuildJob, GroupId, InstanceId, MachineProfile, NoBuildZone, PartId, RotationConstraint, TriMesh,
    transform_from_translation_euler_deg,
};
use serde::{Deserialize, Serialize};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Edge, Surface};
use solidcraft_ui_kit::view_compass::{self, Compass, Drag as CompassDrag, ViewAction, ViewAxes};
use solidcraft_ui_kit::{
    Button, Segments, StageBar, StatusTone, button, catalog_footer_with_tip, checkbox_row,
    icon_label_button, row, scope, section, section_count, segmented, status_line, value_row,
};
use solidcraft_viewport::vec::Vec3Ops as _;
use solidcraft_viewport::{Camera, StandardView};
use solidcraft_viewport_egui::{ViewportCallback, paint_background};
use solidcraft_viewport_gpu::view::{Frame, GpuTarget, SceneSlot, SharedTris};

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

#[derive(Debug, Clone, Copy)]
struct DragState {
    instance_id: InstanceId,
    start_translation: [f32; 3],
    start_hit: [f32; 3],
    plane_z: f32,
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
    Stl(TriMesh),
    ThreeMf(Vec<ThreeMfLoadedItem>),
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
    errors: Vec<String>,
}

struct NestCraftApp {
    job: BuildJob,
    selected: Option<InstanceId>,
    selected_set: HashSet<InstanceId>,
    selection_anchor: Option<InstanceId>,
    camera: Camera,
    shared_cache: HashMap<PartId, Arc<SharedTris>>,
    scene_slot: SceneSlot,
    hl_slot: SceneSlot,
    empty_slot: SceneSlot,
    scene_key: u64,
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
    export_stl_dialog_open: bool,
    export_stl_selected_only: bool,
    export_stl_prefix: String,
    export_stl_suffix: String,
    export_stl_include_id: bool,
    export_stl_apply_transform: bool,
    drag_state: Option<DragState>,
    drag_zone: Option<DragZoneState>,
    box_select: Option<BoxSelectState>,
    load_task: Option<ActiveLoadTask>,
    viewport_ctx_menu: Option<InstanceId>,
    compass_drag: CompassDrag,
    sample_counter: u32,
    manifold_cache: HashMap<PartId, ManifoldReport>,
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
            shared_cache: HashMap::new(),
            scene_slot: Arc::new(Mutex::new(None)),
            hl_slot: Arc::new(Mutex::new(None)),
            empty_slot: Arc::new(Mutex::new(None)),
            scene_key: 1,
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
            export_stl_dialog_open: false,
            export_stl_selected_only: false,
            export_stl_prefix: String::new(),
            export_stl_suffix: String::new(),
            export_stl_include_id: true,
            export_stl_apply_transform: true,
            drag_state: None,
            drag_zone: None,
            box_select: None,
            load_task: None,
            viewport_ctx_menu: None,
            compass_drag: CompassDrag::default(),
            sample_counter: 0,
            manifold_cache: HashMap::new(),
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
            let name = format!("Sample Bracket #{idx}");
            let pid = self.job.add_part(name.clone(), Arc::new(mesh));
            if let Ok(iid) = self.job.add_instance_on_floor_next(pid) {
                self.select_instance(Some(iid));
                self.status = format!("Added {name}");
            }
        }
    }

    fn rotate_selected_90(&mut self, axis: usize) {
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        for sel_id in ids {
            if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == sel_id) {
                let pos = inst.translation();
                let euler = self.euler_edit.entry(sel_id).or_insert_with(|| inst.euler_deg());
                euler[axis] = (euler[axis] + 90.0).rem_euclid(360.0);
                inst.transform = transform_from_translation_euler_deg(pos, *euler);
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

    fn duplicate_selected(&mut self) {
        let Some(sel) = self.selected else {
            return;
        };
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

    fn delete_selected(&mut self) {
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
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

    fn toggle_selected_locked(&mut self) {
        let Some(primary) = self.selected else {
            return;
        };
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
            || self.export_stl_dialog_open
        {
            return;
        }
        ctx.input(|i| {
            let cmd = i.modifiers.command;
            let shift = i.modifiers.shift;
            let alt = i.modifiers.alt;

            if cmd && i.key_pressed(egui::Key::S) {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("NestCraft Bed", &["nestc"])
                    .set_file_name("bed.nestc")
                    .save_file()
                {
                    self.save_bed_file(&path);
                }
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
            if !cmd && !alt && (i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
                self.delete_selected();
            }
            if !cmd && !alt && i.key_pressed(egui::Key::Z)
                && let Some(sel) = self.selected
                && self.job.drop_instance_to_floor(sel)
            {
                self.mark_dirty();
                self.status = "Dropped part to Z = 0".to_owned();
            }
            if !cmd && !alt && i.key_pressed(egui::Key::C)
                && let Some(sel) = self.selected
                && self.job.center_instance_xy(sel)
            {
                self.mark_dirty();
                self.status = "Centered part in XY".to_owned();
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
        self.shared_cache.clear();
        self.manifold_cache.clear();
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
            errors: Vec::new(),
        });

        std::thread::spawn(move || {
            for (idx, path) in paths.into_iter().enumerate() {
                let file_name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file")
                    .to_owned();
                let _ = tx.send(LoadProgressMsg {
                    completed: idx,
                    total,
                    current_label: format!("Loading {file_name}..."),
                    outcome: None,
                });
                ctx_bg.request_repaint();

                let outcome = match std::fs::read(&path) {
                    Err(e) => BackgroundLoadOutcome::Failed(format!("Failed to read {}: {e}", path.display())),
                    Ok(bytes) => {
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
                                Ok(items) => BackgroundLoadOutcome::Imported {
                                    name: file_name.clone(),
                                    kind: ImportedKind::ThreeMf(items),
                                },
                                Err(e) => BackgroundLoadOutcome::Failed(format!("3MF import error: {e}")),
                            }
                        } else if ext == "stl" {
                            match stl::parse_stl(&bytes) {
                                Ok(mesh) => {
                                    let stem = path
                                        .file_stem()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("part")
                                        .to_owned();
                                    BackgroundLoadOutcome::Imported {
                                        name: stem,
                                        kind: ImportedKind::Stl(mesh),
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
                };

                let _ = tx.send(LoadProgressMsg {
                    completed: idx + 1,
                    total,
                    current_label: format!("Loaded {file_name}"),
                    outcome: Some(outcome),
                });
                ctx_bg.request_repaint();
            }
        });
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
        loop {
            match task.rx.try_recv() {
                Ok(msg) => {
                    task.completed = msg.completed;
                    task.total = msg.total;
                    task.current_label = msg.current_label;
                    if let Some(outcome) = msg.outcome {
                        match outcome {
                            BackgroundLoadOutcome::Imported { name, kind } => {
                                task.imported_files += 1;
                                match kind {
                                    ImportedKind::Stl(mesh) => {
                                        let pid = self.job.add_part(name, Arc::new(mesh));
                                        if let Ok(iid) = self.job.add_instance_on_floor_next(pid) {
                                            self.select_instance(Some(iid));
                                            task.imported_items += 1;
                                        }
                                    }
                                    ImportedKind::ThreeMf(items) => {
                                        for item in items {
                                            let pid = self.job.add_part(item.name, item.mesh);
                                            if let Ok(iid) = self.job.add_instance(pid, item.transform) {
                                                self.job.clamp_instance_above_floor(iid);
                                                self.select_instance(Some(iid));
                                                task.imported_items += 1;
                                            }
                                        }
                                    }
                                }
                                self.mark_dirty();
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

        if disconnected || task.completed >= task.total {
            if !opened_bed {
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
                    self.job.clamp_instance_above_floor(iid);
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
            if let Ok(iid) = self.job.add_instance_on_floor_next(pid) {
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

    fn sync_scene_if_dirty(&mut self) {
        if !self.scene_dirty {
            return;
        }
        let tokens = Tokens::get();
        let scene = viewport::build_gpu_scene(&self.job, &self.selected_set, &tokens, &mut self.shared_cache);
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
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
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
                                .backing_on_hover_text("Save current bed, parts, and settings as .nestc (Ctrl+S)")
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
                            if icon_label_button(ui, "Move", "move", !self.selected_set.is_empty(), false)
                                .backing_on_hover_text("Move selected part(s) by relative offset or absolute position (M)")
                                .clicked()
                            {
                                self.open_move_dialog();
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
                                 Ctrl+D: Duplicate   |   Del: Delete\n\
                                 R / Shift+R / Alt+R: Rotate +90° Z / X / Y\n\
                                 Z: Drop Z=0   |   C: Center XY   |   H: Hide   |   L: Lock\n\
                                 Arrows / PgUp/PgDn: Nudge 1 mm (Shift: 10 mm, Ctrl: 0.1 mm)\n\
                                 1 / 3 / 7 / 0 / F: Front / Right / Top / Iso / Fit View",
                            );
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

    fn draw_parts_browser(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        enum TreeAction {
            Select(InstanceId),
            ToggleSelect(InstanceId),
            SelectRange(InstanceId),
            ToggleInstanceHidden(InstanceId, bool),
            ToggleInstanceLock(InstanceId, bool),
            Duplicate(InstanceId),
            DropToFloor(InstanceId),
            CenterXy(InstanceId),
            DeleteInstance(InstanceId),
            SetInstanceGroup(InstanceId, Option<GroupId>),
            ToggleGroupHidden(GroupId, bool),
            DeleteGroup(GroupId),
            SplitShells(PartId),
        }

        // Pre-populate manifold reports so the immutable render pass can read them cleanly.
        let part_ids: Vec<PartId> = self.job.parts.iter().map(|p| p.id).collect();
        for pid in part_ids {
            let _ = self.manifold_report(pid);
        }

        let side_c = Container::new(ctx, Surface::Panel);
        let left_panel = side_c.edge("sls_parts_tree", Edge::Left).default_size(275.0);
        side_c.show_edge(ui, left_panel, side_c.edge_frame(t.panel), |ui| {
            scope(ui, |ui| {
                ui.label(egui::RichText::new("Build Parts Browser").strong().size(t.font_heading).color(t.form_ink));
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
                        let gid = self.job.create_group(name);
                        let sel_ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                        for sel in sel_ids {
                            let _ = self.job.set_instance_group(sel, Some(gid));
                        }
                        self.status = "Created project group".to_owned();
                    }
                });
                ui.separator();

                let mut actions: Vec<TreeAction> = Vec::new();
                let mut tree_order: Vec<InstanceId> = Vec::new();
                let footer_h = solidcraft_ui_kit::catalog_footer_height(ui) + t.space_8;
                let scroll_h = (ui.available_height() - footer_h).max(60.0);
                egui::ScrollArea::vertical()
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
                                    if ui.button(if group.hidden { "Show Group" } else { "Hide Group" }).clicked() {
                                        actions.push(TreeAction::ToggleGroupHidden(group.id, !group.hidden));
                                        ui.close();
                                    }
                                    if ui.button("Ungroup & Delete").clicked() {
                                        actions.push(TreeAction::DeleteGroup(group.id));
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
                                            if ui.button(if inst.locked { "Unlock" } else { "Lock" }).clicked() {
                                                actions.push(TreeAction::ToggleInstanceLock(inst.id, !inst.locked));
                                                ui.close();
                                            }
                                            if ui.button("Remove from Group").clicked() {
                                                actions.push(TreeAction::SetInstanceGroup(inst.id, None));
                                                ui.close();
                                            }
                                            ui.separator();
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

                        for proto in &self.job.parts {
                            let count = self.job.instance_count_for_part(proto.id);
                            let [dx, dy, dz] = proto.mesh.dimensions();
                            let tris = proto.mesh.indices.len();
                            let vol_cm3 = proto.mesh.volume_mm3 / 1000.0;
                            section_count(ui, ("proto", proto.id.0), &proto.name, true, Some(count), |ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{tris} tris · {dx:.1}×{dy:.1}×{dz:.1} mm · {vol_cm3:.2} cm³"
                                    ))
                                    .size(t.font_small)
                                    .color(t.form_dim),
                                );

                                if let Some(report) = self.manifold_cache.get(&proto.id) {
                                    let is_ok = report.is_watertight_manifold();
                                    let summary = if is_ok {
                                        format!("Analysis: Watertight ({} shell)", report.shell_count)
                                    } else {
                                        format!(
                                            "Analysis: {} open · {} flipped · {} shells",
                                            report.boundary_edge_count,
                                            report.flipped_edge_count,
                                            report.shell_count
                                        )
                                    };
                                    ui.push_id(("analysis", proto.id.0), |ui| {
                                        ui.collapsing(
                                            egui::RichText::new(summary)
                                                .size(t.font_small)
                                                .color(if is_ok { t.form_dim } else { t.form_error }),
                                            |ui| {
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "Shells: {} | Boundary: {} | Non-manifold: {} | Flipped: {} | Degenerate: {}",
                                                        report.shell_count,
                                                        report.boundary_edge_count,
                                                        report.non_manifold_edge_count,
                                                        report.flipped_edge_count,
                                                        report.degenerate_triangle_count,
                                                    ))
                                                    .size(t.font_small)
                                                    .color(t.form_dim),
                                                );
                                                if report.shell_count > 1
                                                    && button(ui, "Split Shells to Parts", Button::Small).clicked()
                                                {
                                                    actions.push(TreeAction::SplitShells(proto.id));
                                                }
                                            },
                                        );
                                    });
                                }

                                for inst in self.job.instances.iter().filter(|i| i.part_id == proto.id) {
                                    tree_order.push(inst.id);
                                    let is_sel = self.selected_set.contains(&inst.id);
                                    let above_floor = self.job.is_instance_above_floor(inst.id);
                                    let in_nbz = self.job.is_instance_in_no_build_zone(inst.id);
                                    let visible = self.job.is_instance_visible(inst);
                                    let pos = inst.translation();
                                    let mut tag = String::new();
                                    if inst.hidden {
                                        tag.push_str(" [HIDDEN]");
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
                                    if let Some(gid) = inst.group_id
                                        && let Some(g) = self.job.groups.iter().find(|g| g.id == gid)
                                    {
                                        tag.push_str(&format!(" [{}]", g.name));
                                    }
                                    let label = format!(
                                        "#{:<3} ({:.1}, {:.1}, {:.1}){tag}",
                                        inst.id.0, pos[0], pos[1], pos[2]
                                    );
                                    let rich = if !above_floor || in_nbz {
                                        egui::RichText::new(label).color(t.form_error)
                                    } else if !visible {
                                        egui::RichText::new(label).color(t.form_dim)
                                    } else {
                                        egui::RichText::new(label)
                                    };
                                    let row_resp = ui.horizontal(|ui| {
                                        if ui
                                            .small_button(if inst.hidden { "○" } else { "●" })
                                            .on_hover_text(if inst.hidden { "Show instance" } else { "Hide instance" })
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
                                        if ui.button(if inst.locked { "Unlock" } else { "Lock" }).clicked() {
                                            actions.push(TreeAction::ToggleInstanceLock(inst.id, !inst.locked));
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
                                                if inst.group_id.is_some() && ui.button("None (Ungroup)").clicked() {
                                                    actions.push(TreeAction::SetInstanceGroup(inst.id, None));
                                                    ui.close();
                                                }
                                            });
                                        }
                                        ui.separator();
                                        if ui.button("Delete").clicked() {
                                            actions.push(TreeAction::DeleteInstance(inst.id));
                                            ui.close();
                                        }
                                    });
                                }
                            });
                        }
                    });

                for action in actions {
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
                        TreeAction::ToggleInstanceLock(id, locked) => {
                            if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == id) {
                                inst.locked = locked;
                            }
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
                        TreeAction::SplitShells(pid) => {
                            self.split_part_shells(pid);
                        }
                    }
                }

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
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
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
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
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
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
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
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
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
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
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

    fn draw_inspector(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let side_c = Container::new(ctx, Surface::Panel);
        let right_panel = side_c.edge("sls_inspector", Edge::Right).default_size(295.0);
        side_c.show_edge(ui, right_panel, side_c.edge_frame(t.panel), |ui| {
            scope(ui, |ui| {
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
                    });

                    ui.add_space(t.space_6);
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

                    ui.add_space(t.space_6);
                    section(ui, "inst_sec", "Selected Instance", true, |ui| {
                        let Some(sel_id) = self.selected else {
                            status_line(ui, "Select a part in the 3D view or Parts Browser.", StatusTone::Dim);
                            return;
                        };

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
                        }

                        let mut drop_floor = false;
                        let mut clamp_floor = false;
                        let mut center_xy = false;
                        let mut rot_step: Option<usize> = None;

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
                                inst.transform = transform_from_translation_euler_deg(pos, *euler);
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
                });
            });
        });
    }

    fn draw_viewport(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let central = Container::new(ctx, Surface::Viewport).central();
        central.show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());

            let compass_centre = egui::pos2(rect.right() - 62.0, rect.top() + 62.0);
            let home_centre = egui::pos2(compass_centre.x - 37.0, compass_centre.y - 37.0);
            let over_compass = resp
                .hover_pos()
                .is_some_and(|p| p.distance(compass_centre) <= 52.0 || p.distance(home_centre) <= 14.0);
            let over_view_bar = resp
                .hover_pos()
                .is_some_and(|p| self.view_bar_rect.is_some_and(|r| r.contains(p)));

            if resp.dragged_by(egui::PointerButton::Middle) {
                let delta = resp.drag_delta();
                self.camera.pan(f64::from(delta.x), f64::from(delta.y), f64::from(rect.height().max(1.0)));
            } else if resp.dragged_by(egui::PointerButton::Secondary) {
                self.viewport_ctx_menu = None;
                let delta = resp.drag_delta();
                if ui.input(|i| i.modifiers.shift) {
                    self.camera.pan(f64::from(delta.x), f64::from(delta.y), f64::from(rect.height().max(1.0)));
                } else {
                    self.camera.orbit_unlimited_through_roll(f64::from(-delta.x) * 0.75, f64::from(delta.y) * 0.75);
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
                let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                if let Some(iid) = hit {
                    if !self.selected_set.contains(&iid) {
                        self.select_instance(Some(iid));
                    }
                    if let Some(inst) = self.job.instances.iter().find(|i| i.id == iid)
                        && !inst.locked
                    {
                        let tr = inst.translation();
                        let plane_z = tr[2];
                        if let Some(hit_pt) = viewport::intersect_camera_plane(&self.camera, rect, pos, tr) {
                            self.drag_state = Some(DragState {
                                instance_id: iid,
                                start_translation: tr,
                                start_hit: hit_pt,
                                plane_z,
                            });
                        }
                    }
                } else if viewport::pick_no_build_zone(&self.job, &self.camera, rect, pos) {
                    if let Some(zone) = &self.job.no_build_zone
                        && !zone.locked
                    {
                        let plane_z = zone.position[2];
                        if let Some(hit_pt) = viewport::intersect_camera_plane(&self.camera, rect, pos, zone.position) {
                            self.drag_zone = Some(DragZoneState {
                                start_position: zone.position,
                                start_hit: hit_pt,
                                plane_z,
                            });
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

            if resp.dragged_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                if let Some(bs) = &mut self.box_select {
                    bs.current = pos;
                } else if let Some(dz) = self.drag_zone {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if shift {
                        if let Some(z_now) = viewport::intersect_vertical_plane(&self.camera, rect, pos, dz.start_hit)
                            && let Some(zone) = &mut self.job.no_build_zone
                        {
                            zone.position[2] = dz.start_position[2] + (z_now - dz.plane_z);
                            self.mark_dirty();
                        }
                    } else if let Some(hit_now) = viewport::intersect_camera_plane(&self.camera, rect, pos, dz.start_hit)
                        && let Some(zone) = &mut self.job.no_build_zone
                    {
                        zone.position[0] = dz.start_position[0] + (hit_now[0] - dz.start_hit[0]);
                        zone.position[1] = dz.start_position[1] + (hit_now[1] - dz.start_hit[1]);
                        zone.position[2] = dz.start_position[2] + (hit_now[2] - dz.start_hit[2]);
                        self.mark_dirty();
                    }
                } else if let Some(ds) = self.drag_state {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if shift {
                        if let Some(z_now) = viewport::intersect_vertical_plane(&self.camera, rect, pos, ds.start_hit)
                            && let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == ds.instance_id)
                        {
                            let mut tr = inst.translation();
                            tr[2] = ds.start_translation[2] + (z_now - ds.plane_z);
                            inst.set_translation(tr);
                            self.job.clamp_instance_above_floor(ds.instance_id);
                            self.mark_dirty();
                        }
                    } else if let Some(hit_now) = viewport::intersect_camera_plane(&self.camera, rect, pos, ds.start_hit)
                        && let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == ds.instance_id)
                    {
                        let tr = [
                            ds.start_translation[0] + (hit_now[0] - ds.start_hit[0]),
                            ds.start_translation[1] + (hit_now[1] - ds.start_hit[1]),
                            ds.start_translation[2] + (hit_now[2] - ds.start_hit[2]),
                        ];
                        inst.set_translation(tr);
                        self.job.clamp_instance_above_floor(ds.instance_id);
                        self.mark_dirty();
                    }
                }
            }

            if resp.drag_stopped() {
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
                if self.drag_zone.take().is_some() {
                    self.persist_no_build_zone();
                }
                self.drag_state = None;
            }

            if !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && resp.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                self.viewport_ctx_menu = None;
                let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                if ui.input(|i| i.modifiers.command) {
                    if let Some(iid) = hit {
                        self.toggle_select_instance(iid);
                    }
                } else {
                    self.select_instance(hit);
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
                let mut open_dup = false;
                let mut drop_floor = false;
                let mut center_xy = false;
                let mut rot_z = false;
                let mut toggle_hide = false;
                let mut toggle_lock = false;
                let mut delete_sel = false;

                let is_hidden = self
                    .selected
                    .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
                    .is_some_and(|i| i.hidden);
                let is_locked = self
                    .selected
                    .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
                    .is_some_and(|i| i.locked);

                resp.context_menu(|ui| {
                    if ui.button("Move... (M)").clicked() {
                        open_move = true;
                        ui.close();
                    }
                    if ui.button("Duplicate... (Ctrl+D)").clicked() {
                        open_dup = true;
                        ui.close();
                    }
                    if ui.button("Drop to Min Z (Z)").clicked() {
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
                    if ui.button(if is_locked { "Unlock (L)" } else { "Lock (L)" }).clicked() {
                        toggle_lock = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Delete (Del)").clicked() {
                        delete_sel = true;
                        ui.close();
                    }
                });

                if open_move {
                    self.viewport_ctx_menu = None;
                    self.open_move_dialog();
                } else if open_dup {
                    self.viewport_ctx_menu = None;
                    self.open_duplicate_dialog();
                } else if drop_floor {
                    self.viewport_ctx_menu = None;
                    let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                    for id in ids {
                        self.job.drop_instance_to_floor(id);
                    }
                    self.mark_dirty();
                    self.status = "Dropped selection to Min Z".to_owned();
                } else if center_xy {
                    self.viewport_ctx_menu = None;
                    if let Some(sel) = self.selected
                        && self.job.center_instance_xy(sel)
                    {
                        self.mark_dirty();
                        self.status = "Centered part in XY".to_owned();
                    }
                } else if rot_z {
                    self.viewport_ctx_menu = None;
                    self.rotate_selected_90(2);
                } else if toggle_hide {
                    self.viewport_ctx_menu = None;
                    self.toggle_selected_hidden();
                } else if toggle_lock {
                    self.viewport_ctx_menu = None;
                    self.toggle_selected_locked();
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
                hl_key: 0,
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
                moving: None,
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
        self.draw_bottom_bar(ui, &ctx, &t);
        self.draw_parts_browser(ui, &ctx, &t);
        self.draw_inspector(ui, &ctx, &t);
        self.draw_viewport(ui, &ctx, &t);
        self.draw_custom_platform_modal(&ctx);
        self.draw_duplicate_modal(&ctx);
        self.draw_move_modal(&ctx);
        self.draw_export_stl_modal(&ctx);
        self.draw_load_progress_modal(&ctx);
        self.draw_drop_overlay(&ctx, window_rect, &t);
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
    fn app_initializes_and_syncs_scene() {
        let mut app = NestCraftApp::new();
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert!(app.scene_slot.lock().is_ok_and(|g| g.is_some()));
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
