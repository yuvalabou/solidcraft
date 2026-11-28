//! NestCraft application state, I/O, and part-manipulation logic.

use crate::viewport;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use nestcraft_core::io::threemf::ThreeMfLoadedItem;
use nestcraft_core::io::{nestc, stl, threemf};
use nestcraft_core::manifold::{self, ManifoldReport};
use nestcraft_core::model::{
    BedOrigin, BuildJob, GroupId, IDENTITY_TRANSFORM, InstanceId, MachineProfile, NoBuildZone, PartId, TriMesh,
};
use nestcraft_core::sinterbox::{self, CageShape, LabelPanel, SinterboxConfig};
use serde::{Deserialize, Serialize};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{Container, Surface};
use solidcraft_ui_kit::view_compass::Drag as CompassDrag;
use solidcraft_viewport::{Camera, StandardView};
use solidcraft_viewport_gpu::view::{GpuScene, MovingLayer, SceneSlot, SharedTris};

pub(crate) const COLLISION_VOXEL_PITCH_MM: f32 = 3.0;

pub(crate) struct DragState {
    pub(crate) anchor_id: InstanceId,
    pub(crate) instances: Vec<(InstanceId, [f32; 3])>,
    pub(crate) start_hit: [f32; 3],
    pub(crate) plane_z: f32,
    pub(crate) min_allowed_dz: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct RotateDragState {
    pub(crate) pivot: [f32; 3],
    pub(crate) pivot_screen: egui::Pos2,
    pub(crate) axis: usize,
    pub(crate) angle_rad: f32,
    pub(crate) prev_pos: egui::Pos2,
    pub(crate) edge_tangent: egui::Vec2,
    pub(crate) instances: Vec<(InstanceId, [[f32; 4]; 3])>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DragZoneState {
    pub(crate) start_position: [f32; 3],
    pub(crate) start_hit: [f32; 3],
    pub(crate) plane_z: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BoxSelectState {
    pub(crate) start: egui::Pos2,
    pub(crate) current: egui::Pos2,
    pub(crate) additive: bool,
}

pub(crate) enum ImportedKind {
    Stl(Arc<TriMesh>, Arc<SharedTris>),
    ThreeMf(Vec<(ThreeMfLoadedItem, Arc<SharedTris>)>),
}

#[derive(Debug, Clone)]
pub(crate) enum KeepOrDeleteAction {
    SplitShells(Vec<InstanceId>),
    MergeSelected(Vec<InstanceId>),
}

pub(crate) enum BackgroundLoadOutcome {
    Imported {
        name: String,
        kind: ImportedKind,
    },
    OpenedBed {
        job: Box<BuildJob>,
        path: Option<PathBuf>,
        path_display: String,
    },
    Failed(String),
}

pub(crate) struct LoadProgressMsg {
    pub(crate) completed: usize,
    pub(crate) total: usize,
    pub(crate) current_label: String,
    pub(crate) outcome: Option<BackgroundLoadOutcome>,
}

pub(crate) struct ActiveLoadTask {
    pub(crate) rx: Receiver<LoadProgressMsg>,
    pub(crate) completed: usize,
    pub(crate) total: usize,
    pub(crate) current_label: String,
    pub(crate) imported_files: usize,
    pub(crate) imported_items: usize,
    pub(crate) below_zero_instances: Vec<(InstanceId, f32)>,
    pub(crate) pushed_undo: bool,
    pub(crate) errors: Vec<String>,
}

pub(crate) enum CalcOutcome {
    Collision {
        report: nestcraft_core::collision::CollisionReport,
        cache: Box<nestcraft_core::collision::CollisionCache>,
    },
    Nest2d {
        job: Box<BuildJob>,
        result: nestcraft_core::nest2d::Nest2dResult,
    },
    Nest2dFailed(String),
    Thermal(nestcraft_core::thermal::ThermalMapReport),
    Slice(nestcraft_core::slice::SliceDistribution),
}

pub(crate) struct BackgroundCalcTask {
    pub(crate) title: String,
    pub(crate) show_progress: bool,
    pub(crate) completed: Arc<AtomicUsize>,
    pub(crate) total: Arc<AtomicUsize>,
    pub(crate) rx: Receiver<CalcOutcome>,
}

pub(crate) enum Nest3dMsg {
    Progress(nestcraft_core::nest3d::Nest3dProgress),
    Finished {
        job: Box<BuildJob>,
        result: nestcraft_core::nest3d::Nest3dResult,
    },
    SettleFinished {
        job: Box<BuildJob>,
        result: nestcraft_core::compact::GravitySettleResult,
    },
    Cancelled,
    Failed(String),
}

pub(crate) struct ActiveNest3dTask {
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) stop_and_apply: Arc<AtomicBool>,
    pub(crate) pre_nest_snapshot: UndoSnapshot,
    pub(crate) started_at: std::time::Instant,
    pub(crate) latest: nestcraft_core::nest3d::Nest3dProgress,
    pub(crate) rx: Receiver<Nest3dMsg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedBedConfig {
    pub(crate) machine_name: String,
    pub(crate) size_mm_bits: [u32; 3],
    pub(crate) bed_origin: BedOrigin,
    pub(crate) wall_margin_bits: u32,
    pub(crate) min_z_bits: u32,
    pub(crate) clearance_bits: u32,
    pub(crate) material_id: String,
}

impl SavedBedConfig {
    pub(crate) fn from_job(job: &BuildJob) -> Self {
        let norm = |f: f32| if f == 0.0 { 0.0_f32.to_bits() } else { f.to_bits() };
        let sz = job.machine.size_mm();
        Self {
            machine_name: job.machine.name().to_owned(),
            size_mm_bits: [norm(sz[0]), norm(sz[1]), norm(sz[2])],
            bed_origin: job.bed_origin,
            wall_margin_bits: norm(job.wall_margin_mm),
            min_z_bits: norm(job.min_z_mm),
            clearance_bits: norm(job.clearance_mm),
            material_id: job.material_id.clone(),
        }
    }
}

pub(crate) enum SaveOutcome {
    Saved {
        path: PathBuf,
        path_display: String,
        file_name: String,
        saved_config: SavedBedConfig,
    },
    Failed(String),
}

pub(crate) struct ActiveSaveTask {
    pub(crate) file_name: String,
    pub(crate) progress_permille: Arc<AtomicUsize>,
    pub(crate) stage: Arc<Mutex<String>>,
    pub(crate) rx: Receiver<SaveOutcome>,
}

#[derive(Clone)]
pub(crate) struct UndoSnapshot {
    pub(crate) job: BuildJob,
    pub(crate) selected: Option<InstanceId>,
    pub(crate) selected_set: HashSet<InstanceId>,
    pub(crate) selection_anchor: Option<InstanceId>,
}

pub(crate) fn load_single_file_outcome(path: &Path) -> BackgroundLoadOutcome {
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
                job: Box::new(job),
                path: Some(path.to_path_buf()),
                path_display: path.display().to_string(),
            },
            Err(e) => BackgroundLoadOutcome::Failed(format!("Open bed error: {e}")),
        }
    } else if ext == "3mf" {
        match threemf::read_threemf(&bytes) {
            Ok(items) => {
                let mut tris_by_ptr: HashMap<usize, Arc<SharedTris>> = HashMap::new();
                let prepared = items
                    .into_iter()
                    .map(|item| {
                        let key = Arc::as_ptr(&item.mesh) as usize;
                        let tris = tris_by_ptr
                            .entry(key)
                            .or_insert_with(|| viewport::mesh_to_shared_tris(&item.mesh))
                            .clone();
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

pub(crate) struct NestCraftApp {
    pub(crate) job: BuildJob,
    pub(crate) selected: Option<InstanceId>,
    pub(crate) selected_set: HashSet<InstanceId>,
    pub(crate) selection_anchor: Option<InstanceId>,
    pub(crate) camera: Camera,
    pub(crate) scene_cache: viewport::SceneCache,
    pub(crate) scene_slot: SceneSlot,
    pub(crate) moving_slot: SceneSlot,
    pub(crate) hl_slot: SceneSlot,
    pub(crate) empty_slot: SceneSlot,
    pub(crate) scene_key: u64,
    pub(crate) hl_key: u64,
    pub(crate) moving_key: u64,
    pub(crate) moving_target: Option<viewport::DragExclude>,
    pub(crate) align_face_target: Option<InstanceId>,
    pub(crate) hovered_face: Option<(InstanceId, usize)>,
    pub(crate) scene_dirty: bool,
    pub(crate) status: String,
    pub(crate) euler_edit: HashMap<InstanceId, [f32; 3]>,
    pub(crate) dup_dialog_open: bool,
    pub(crate) dup_axes: [bool; 3],
    pub(crate) dup_counts: [usize; 3],
    pub(crate) dup_total: usize,
    pub(crate) dup_exact_total: bool,
    pub(crate) dup_gap_mm: [f32; 3],
    pub(crate) move_dialog_open: bool,
    pub(crate) move_mode_idx: usize,
    pub(crate) move_xyz: [f32; 3],
    pub(crate) scale_dialog_open: bool,
    pub(crate) scale_uniform: bool,
    pub(crate) scale_keep_open: bool,
    pub(crate) scale_mode_idx: usize,
    pub(crate) scale_pct: [f32; 3],
    pub(crate) scale_abs_mm: [f32; 3],
    pub(crate) scale_factor: [f32; 3],
    pub(crate) scale_base_dims: [f32; 3],
    pub(crate) scale_initial_dims: [f32; 3],
    pub(crate) scale_initial_snapshot: Option<UndoSnapshot>,
    pub(crate) scale_applied_in_session: usize,
    pub(crate) sinterbox_dialog_open: bool,
    pub(crate) sinterbox_shape_idx: usize,
    pub(crate) sinterbox_inner_clearance: f32,
    pub(crate) sinterbox_window_mm: f32,
    pub(crate) sinterbox_window_auto: bool,
    pub(crate) sinterbox_bar_mm: f32,
    pub(crate) sinterbox_bar_auto: bool,
    pub(crate) sinterbox_label_enabled: bool,
    pub(crate) sinterbox_label_text: String,
    pub(crate) sinterbox_label_panel_idx: usize,
    pub(crate) sinterbox_label_depth: f32,
    pub(crate) sinterbox_group_name: String,
    pub(crate) sinterbox_group_name_edited: bool,
    pub(crate) sinterbox_transparent: bool,
    pub(crate) sinterbox_single_copies: usize,
    pub(crate) sinterbox_single_gap: f32,
    pub(crate) color_picker_open: bool,
    pub(crate) color_picker_rgb: [u8; 3],
    pub(crate) color_picker_undo_pushed: bool,
    pub(crate) pending_keep_delete: Option<KeepOrDeleteAction>,
    pub(crate) export_stl_dialog_open: bool,
    pub(crate) report_dialog_open: bool,
    pub(crate) report_preview: Option<nestcraft_core::report::BuildReport>,
    pub(crate) failed_print_dialog_open: bool,
    pub(crate) failed_print_z_mm: f32,
    pub(crate) failed_print_filter: usize,
    pub(crate) failed_print_source_label: String,
    pub(crate) failed_print_external_job: Option<BuildJob>,
    pub(crate) nest2d_dialog_open: bool,
    pub(crate) nest2d_config: nestcraft_core::nest2d::Nest2dConfig,
    pub(crate) nest2d_last_result: Option<nestcraft_core::nest2d::Nest2dResult>,
    pub(crate) nest3d_dialog_open: bool,
    pub(crate) nest3d_advanced_open: bool,
    pub(crate) nest3d_config: nestcraft_core::nest3d::Nest3dConfig,
    pub(crate) nest3d_last_result: Option<nestcraft_core::nest3d::Nest3dResult>,
    pub(crate) nest3d_task: Option<ActiveNest3dTask>,
    pub(crate) nest3d_sys_avail_ram_bytes: u64,
    pub(crate) gravity_settle_dialog_open: bool,
    pub(crate) gravity_settle_config: nestcraft_core::compact::GravitySettleConfig,
    pub(crate) gravity_settle_last_result: Option<nestcraft_core::compact::GravitySettleResult>,
    pub(crate) slice_dist_open: bool,
    pub(crate) slice_dist_step_mm: f32,
    pub(crate) slice_dist_bed_scale: bool,
    pub(crate) slice_dist_cache: Option<nestcraft_core::slice::SliceDistribution>,
    pub(crate) slice_dist_dirty: bool,
    pub(crate) thermal_map_open: bool,
    pub(crate) thermal_report: Option<nestcraft_core::thermal::ThermalMapReport>,
    pub(crate) collision_check_active: bool,
    pub(crate) collision_cache: nestcraft_core::collision::CollisionCache,
    pub(crate) collision_overlay_nonempty: bool,
    pub(crate) export_stl_selected_only: bool,
    pub(crate) export_stl_prefix: String,
    pub(crate) export_stl_suffix: String,
    pub(crate) export_stl_include_id: bool,
    pub(crate) export_stl_apply_transform: bool,
    pub(crate) drag_state: Option<DragState>,
    pub(crate) rotate_gizmo_open: bool,
    pub(crate) rotate_drag: Option<RotateDragState>,
    pub(crate) drag_zone: Option<DragZoneState>,
    pub(crate) box_select: Option<BoxSelectState>,
    pub(crate) zoom_rect_mode: bool,
    pub(crate) zoom_rect: Option<(egui::Pos2, egui::Pos2)>,
    pub(crate) viewport_aspect: f64,
    pub(crate) load_task: Option<ActiveLoadTask>,
    pub(crate) save_task: Option<ActiveSaveTask>,
    pub(crate) save_hint: Option<(String, bool, std::time::Instant)>,
    pub(crate) current_bed_path: Option<PathBuf>,
    pub(crate) last_saved_config: Option<SavedBedConfig>,
    pub(crate) calc_task: Option<BackgroundCalcTask>,
    pub(crate) below_zero_import_dialog: Vec<(InstanceId, f32)>,
    pub(crate) undo_stack: Vec<UndoSnapshot>,
    pub(crate) redo_stack: Vec<UndoSnapshot>,
    pub(crate) viewport_ctx_menu: Option<InstanceId>,
    pub(crate) compass_drag: CompassDrag,
    pub(crate) sample_counter: u32,
    pub(crate) manifold_cache: HashMap<PartId, ManifoldReport>,
    pub(crate) part_thumbs: HashMap<PartId, egui::TextureHandle>,
    pub(crate) expanded_parts: HashSet<PartId>,
    pub(crate) active_machine_slot: usize,
    pub(crate) machine_presets: [MachinePresetConfig; MACHINE_SLOT_COUNT],
    pub(crate) custom_platform_open: bool,
    pub(crate) custom_name: String,
    pub(crate) custom_size_mm: [f32; 3],
    pub(crate) custom_wall_margin: f32,
    pub(crate) custom_min_z: f32,
    pub(crate) custom_clearance: f32,
    pub(crate) custom_bed_origin: BedOrigin,
    pub(crate) custom_material_id: String,
    pub(crate) orthographic_view: bool,
    pub(crate) new_group_name: String,
    pub(crate) view_bar_rect: Option<egui::Rect>,
    pub(crate) repair_badge_rect: Option<egui::Rect>,
}

pub(crate) fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NoBuildZoneConfig {
    pub(crate) name: String,
    pub(crate) position: [f32; 3],
    pub(crate) rotation_deg: [f32; 3],
    pub(crate) clearance_mm: f32,
    pub(crate) enabled: bool,
    pub(crate) visible: bool,
    #[serde(default = "default_true")]
    pub(crate) locked: bool,
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
pub(crate) enum BedOriginCfg {
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

pub(crate) const MACHINE_SLOT_COUNT: usize = 6;
pub(crate) const P100_MACHINE_SLOT: usize = 2;
pub(crate) const CUSTOM_MACHINE_SLOT: usize = 5;

pub(crate) fn slot_supports_no_build_zone(slot: usize) -> bool {
    slot != P100_MACHINE_SLOT
}

fn default_preset_material_id() -> String {
    "pa2200".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct MachinePresetConfig {
    pub(crate) name: String,
    pub(crate) size_mm: [f32; 3],
    pub(crate) wall_margin_mm: f32,
    pub(crate) min_z_mm: f32,
    pub(crate) clearance_mm: f32,
    #[serde(default)]
    pub(crate) bed_origin: BedOriginCfg,
    #[serde(default = "default_preset_material_id")]
    pub(crate) material_id: String,
}

pub(crate) fn default_machine_presets() -> [MachinePresetConfig; MACHINE_SLOT_COUNT] {
    [
        MachinePresetConfig {
            name: "EOS Formiga P110".to_owned(),
            size_mm: MachineProfile::FormigaP110.size_mm(),
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::FrontLeft,
            material_id: "pa2200".to_owned(),
        },
        MachinePresetConfig {
            name: "EOS Formiga P110 Velocis".to_owned(),
            size_mm: MachineProfile::FormigaP110.size_mm(),
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::FrontLeft,
            material_id: "pa2200".to_owned(),
        },
        MachinePresetConfig {
            name: "EOS Formiga P100".to_owned(),
            size_mm: MachineProfile::FormigaP110.size_mm(),
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::FrontLeft,
            material_id: "pa2200".to_owned(),
        },
        MachinePresetConfig {
            name: "Farsoon HT403P".to_owned(),
            size_mm: MachineProfile::FarsoonHT403P.size_mm(),
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::Center,
            material_id: "pa2200".to_owned(),
        },
        MachinePresetConfig {
            name: "Farsoon SS403P".to_owned(),
            size_mm: MachineProfile::FarsoonHT403P.size_mm(),
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::Center,
            material_id: "fs3300".to_owned(),
        },
        MachinePresetConfig {
            name: "Custom SLS".to_owned(),
            size_mm: [250.0, 250.0, 300.0],
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            clearance_mm: 2.5,
            bed_origin: BedOriginCfg::FrontLeft,
            material_id: "pa2200".to_owned(),
        },
    ]
}

pub(crate) fn nestcraft_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA"));
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_DATA_HOME").or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share").into_os_string())
    });
    base.map(|b| PathBuf::from(b).join("NestCraft"))
}

pub(crate) fn save_machine_presets_to_dir(
    dir: &Path,
    presets: &[MachinePresetConfig; MACHINE_SLOT_COUNT],
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    let json = serde_json::to_string_pretty(presets).map_err(|e| format!("Serialize error: {e}"))?;
    std::fs::write(dir.join("machines.json"), json)
        .map_err(|e| format!("Failed to write machines.json: {e}"))
}

pub(crate) fn load_machine_presets_from_dir(
    dir: &Path,
) -> Option<[MachinePresetConfig; MACHINE_SLOT_COUNT]> {
    let bytes = std::fs::read(dir.join("machines.json")).ok()?;
    if let Ok(presets) = serde_json::from_slice::<[MachinePresetConfig; MACHINE_SLOT_COUNT]>(&bytes) {
        return Some(presets);
    }
    let legacy = serde_json::from_slice::<[MachinePresetConfig; 3]>(&bytes).ok()?;
    let mut migrated = default_machine_presets();
    let [old_p110, old_ht403, old_custom] = legacy;

    migrated[0] = old_p110.clone();

    migrated[1].size_mm = old_p110.size_mm;
    migrated[1].wall_margin_mm = old_p110.wall_margin_mm;
    migrated[1].min_z_mm = old_p110.min_z_mm;
    migrated[1].clearance_mm = old_p110.clearance_mm;
    migrated[1].bed_origin = old_p110.bed_origin;
    migrated[1].material_id = old_p110.material_id.clone();

    migrated[2].size_mm = old_p110.size_mm;
    migrated[2].wall_margin_mm = old_p110.wall_margin_mm;
    migrated[2].min_z_mm = old_p110.min_z_mm;
    migrated[2].clearance_mm = old_p110.clearance_mm;
    migrated[2].bed_origin = old_p110.bed_origin;
    migrated[2].material_id = old_p110.material_id;

    migrated[3] = old_ht403.clone();

    migrated[4].size_mm = old_ht403.size_mm;
    migrated[4].wall_margin_mm = old_ht403.wall_margin_mm;
    migrated[4].min_z_mm = old_ht403.min_z_mm;
    migrated[4].clearance_mm = old_ht403.clearance_mm;
    migrated[4].bed_origin = old_ht403.bed_origin;

    migrated[5] = old_custom;
    Some(migrated)
}

pub(crate) fn no_build_zone_dir_for_slot(slot: usize) -> Option<PathBuf> {
    let subdir = match slot {
        0 | 1 => "p110",
        2 => return None,
        3 => "ht403p",
        4 => "ss403p",
        _ => "custom",
    };
    nestcraft_data_dir().map(|d| d.join("no_build_zone").join(subdir))
}

pub(crate) fn load_no_build_zone_for_slot(slot: usize) -> Option<NoBuildZone> {
    if !slot_supports_no_build_zone(slot) {
        return None;
    }
    if let Some(dir) = no_build_zone_dir_for_slot(slot)
        && let Some(zone) = load_no_build_zone_from_dir(&dir)
    {
        return Some(zone);
    }
    let legacy = match slot {
        0 | 1 => Some("slot_0"),
        3 => Some("slot_1"),
        5 => Some("slot_2"),
        _ => None,
    };
    legacy
        .and_then(|name| nestcraft_data_dir().map(|d| d.join("no_build_zone").join(name)))
        .and_then(|dir| load_no_build_zone_from_dir(&dir))
}

pub(crate) fn sanitize_ascii_filename_part(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii())
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .collect()
}

pub(crate) fn export_part_menu_label(selected_count: usize) -> &'static str {
    if selected_count > 1 {
        "Export Parts..."
    } else {
        "Export Part..."
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ColorMenuChoice {
    OpenPicker,
    Set(Option<[u8; 3]>),
}

pub(crate) fn show_submenu(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
    let btn = ui.add(egui::Button::new(format!("{label} ▸")));
    Container::new(ui.ctx(), Surface::Popup).submenu().show(ui, &btn, body);
}

pub(crate) fn show_color_submenu(ui: &mut egui::Ui) -> Option<ColorMenuChoice> {
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
    show_submenu(ui, "Color", |ui| {
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

pub(crate) const THUMB_PX: usize = 20;

pub(crate) fn render_part_thumbnail(mesh: &TriMesh) -> egui::ColorImage {
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

pub(crate) fn format_export_stl_stem(
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

pub(crate) fn save_no_build_zone_config_to_dir(dir: &Path, zone: &NoBuildZone) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    let cfg = NoBuildZoneConfig::from(zone);
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| format!("Serialize error: {e}"))?;
    std::fs::write(dir.join("config.json"), json)
        .map_err(|e| format!("Failed to write no-build zone config: {e}"))
}

pub(crate) fn load_no_build_zone_from_dir(dir: &Path) -> Option<NoBuildZone> {
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

pub(crate) const VIEW_CYCLE: [StandardView; 7] = [
    StandardView::Iso,
    StandardView::Top,
    StandardView::Bottom,
    StandardView::Front,
    StandardView::Back,
    StandardView::Right,
    StandardView::Left,
];

pub(crate) fn view_label(view: StandardView) -> &'static str {
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

pub(crate) fn distribute_compact_grid(total: usize, enabled: [bool; 3]) -> [usize; 3] {
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
    pub(crate) fn new() -> Self {
        let machine_presets = nestcraft_data_dir()
            .and_then(|d| load_machine_presets_from_dir(&d))
            .unwrap_or_else(default_machine_presets);
        let p0 = &machine_presets[0];
        let mut job = BuildJob::new(MachineProfile::custom(&p0.name, p0.size_mm));
        job.wall_margin_mm = p0.wall_margin_mm;
        job.min_z_mm = p0.min_z_mm;
        job.clearance_mm = p0.clearance_mm;
        job.bed_origin = p0.bed_origin.into();
        job.material_id = p0.material_id.clone();
        job.no_build_zone = load_no_build_zone_for_slot(0);
        let nest3d_config = nestcraft_core::nest3d::Nest3dConfig::from_job(&job);
        let camera = viewport::default_camera_with_origin(&job.machine, job.bed_origin);
        let custom_name = p0.name.clone();
        let custom_size_mm = job.machine.size_mm();
        let custom_wall_margin = job.wall_margin_mm;
        let custom_min_z = job.min_z_mm;
        let custom_clearance = job.clearance_mm;
        let custom_bed_origin = job.bed_origin;
        let custom_material_id = job.material_id.clone();
        let gravity_settle_config = nestcraft_core::compact::GravitySettleConfig::from_job(&job);
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
            report_dialog_open: false,
            report_preview: None,
            failed_print_dialog_open: false,
            failed_print_z_mm: 0.0,
            failed_print_filter: 0,
            failed_print_source_label: "Current Bed".to_owned(),
            failed_print_external_job: None,
            nest2d_dialog_open: false,
            nest2d_config: nestcraft_core::nest2d::Nest2dConfig::default(),
            nest2d_last_result: None,
            nest3d_dialog_open: false,
            nest3d_advanced_open: false,
            nest3d_config,
            nest3d_last_result: None,
            nest3d_task: None,
            nest3d_sys_avail_ram_bytes: 4 * 1024 * 1024 * 1024,
            gravity_settle_dialog_open: false,
            gravity_settle_config,
            gravity_settle_last_result: None,
            slice_dist_open: false,
            slice_dist_step_mm: 1.0,
            slice_dist_bed_scale: false,
            slice_dist_cache: None,
            slice_dist_dirty: false,
            thermal_map_open: false,
            thermal_report: None,
            collision_check_active: false,
            collision_cache: nestcraft_core::collision::CollisionCache::new(),
            collision_overlay_nonempty: false,
            export_stl_selected_only: false,
            export_stl_prefix: String::new(),
            export_stl_suffix: String::new(),
            export_stl_include_id: true,
            export_stl_apply_transform: true,
            drag_state: None,
            rotate_gizmo_open: false,
            rotate_drag: None,
            drag_zone: None,
            box_select: None,
            zoom_rect_mode: false,
            zoom_rect: None,
            viewport_aspect: 16.0 / 9.0,
            load_task: None,
            save_task: None,
            save_hint: None,
            current_bed_path: None,
            last_saved_config: None,
            calc_task: None,
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
            custom_material_id,
            orthographic_view: false,
            new_group_name: String::new(),
            view_bar_rect: None,
            repair_badge_rect: None,
        }
    }

    pub(crate) fn manifold_report(&mut self, part_id: PartId) -> Option<ManifoldReport> {
        if let Some(report) = self.manifold_cache.get(&part_id) {
            return Some(report.clone());
        }
        let mesh = self.job.part(part_id)?.mesh.clone();
        let report = manifold::analyze_mesh(&mesh);
        self.manifold_cache.insert(part_id, report.clone());
        Some(report)
    }

    pub(crate) fn faulty_placed_instance_ids(&mut self) -> Vec<InstanceId> {
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

    pub(crate) fn apply_machine_slot(&mut self, slot: usize) {
        let idx = slot.min(MACHINE_SLOT_COUNT - 1);
        self.active_machine_slot = idx;
        let p = &self.machine_presets[idx];
        self.job.machine = MachineProfile::custom(&p.name, p.size_mm);
        self.job.wall_margin_mm = p.wall_margin_mm;
        self.job.min_z_mm = p.min_z_mm;
        self.job.clearance_mm = p.clearance_mm;
        self.job.bed_origin = p.bed_origin.into();
        self.job.material_id = p.material_id.clone();
        self.job.no_build_zone = load_no_build_zone_for_slot(idx);
        self.camera = self.default_camera_for_job();
        self.mark_dirty();
        self.status = format!("Active machine: {}", self.job.machine.name());
    }

    pub(crate) fn open_report_dialog(&mut self) {
        self.report_preview = Some(nestcraft_core::report::BuildReport::generate(&self.job));
        self.report_dialog_open = true;
    }

    pub(crate) fn open_failed_print_dialog(&mut self) {
        self.failed_print_external_job = None;
        self.failed_print_source_label = "Current Bed".to_owned();
        if self.failed_print_z_mm <= 0.0 {
            self.failed_print_z_mm = (self.job.max_z_mm() * 0.5).max(0.0);
        }
        self.failed_print_dialog_open = true;
    }

    pub(crate) fn is_nest3d_running(&self) -> bool {
        self.nest3d_task.is_some()
    }

    pub(crate) fn refresh_available_ram(&mut self) {
        if let Ok(mut sys) = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::nothing()
                .with_memory(sysinfo::MemoryRefreshKind::nothing().with_ram()),
        ) {
            sys.refresh_memory();
            let avail = sys.available_memory();
            if avail > 0 {
                self.nest3d_sys_avail_ram_bytes = avail;
            }
        }
    }

    pub(crate) fn sync_nest3d_ceiling_overlay(&mut self) {
        let next = if self.nest3d_dialog_open || self.is_nest3d_running() {
            self.nest3d_config
                .max_z_limit_mm
                .map(|_| self.nest3d_config.effective_max_z(&self.job))
        } else {
            None
        };
        if self.scene_cache.max_z_limit_mm != next {
            self.scene_cache.max_z_limit_mm = next;
            self.scene_dirty = true;
        }
    }

    pub(crate) fn open_nest3d_dialog(&mut self) {
        if self.is_nest3d_running() {
            return;
        }
        self.refresh_available_ram();
        self.nest3d_config.min_distance_mm = self.job.clearance_mm.max(0.0);
        self.nest3d_config.wall_margin_mm = self.job.wall_margin_mm.max(0.0);
        self.nest3d_config.min_z_mm = self.job.min_z_mm.max(0.0);
        if self.selected_set.is_empty() {
            self.nest3d_config.selected_only = false;
        }
        self.nest3d_dialog_open = true;
        self.sync_nest3d_ceiling_overlay();
    }

    pub(crate) fn open_gravity_settle_dialog(&mut self) {
        if self.is_nest3d_running() {
            return;
        }
        self.gravity_settle_config.min_distance_mm = self.job.clearance_mm.max(0.0);
        self.gravity_settle_config.wall_margin_mm = self.job.wall_margin_mm.max(0.0);
        self.gravity_settle_config.min_z_mm = self.job.min_z_mm.max(0.0);
        if self.selected_set.is_empty() {
            self.gravity_settle_config.selected_only = false;
        }
        self.gravity_settle_dialog_open = true;
    }

    pub(crate) fn open_nest2d_dialog(&mut self) {
        self.nest2d_config.min_distance_mm = self.job.clearance_mm;
        self.nest2d_config.wall_margin_mm = self.job.wall_margin_mm;
        self.nest2d_dialog_open = true;
    }

    pub(crate) fn open_custom_platform_dialog(&mut self) {
        let p = &self.machine_presets[self.active_machine_slot.min(MACHINE_SLOT_COUNT - 1)];
        self.custom_name = p.name.clone();
        self.custom_size_mm = self.job.machine.size_mm();
        self.custom_wall_margin = self.job.wall_margin_mm;
        self.custom_min_z = self.job.min_z_mm;
        self.custom_clearance = self.job.clearance_mm;
        self.custom_bed_origin = self.job.bed_origin;
        self.custom_material_id = p.material_id.clone();
        self.custom_platform_open = true;
    }

    pub(crate) fn split_part_shells(&mut self, part_id: PartId) {
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

    pub(crate) fn request_shells_to_parts(&mut self, ids: Vec<InstanceId>) {
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

    pub(crate) fn shells_to_parts_for(&mut self, ids: Vec<InstanceId>) {
        self.shells_to_parts_for_with_keep(ids, false);
    }

    pub(crate) fn shells_to_parts_for_with_keep(&mut self, ids: Vec<InstanceId>, keep_source: bool) {
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

    pub(crate) fn request_merge_selected_parts(&mut self) {
        if self.selected_set.len() < 2 {
            self.status = "Select at least 2 parts to merge.".to_owned();
            return;
        }
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        self.pending_keep_delete = Some(KeepOrDeleteAction::MergeSelected(ids));
    }

    pub(crate) fn merge_selected_parts(&mut self) {
        if self.selected_set.len() < 2 {
            self.status = "Select at least 2 parts to merge.".to_owned();
            return;
        }
        let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        self.merge_selected_parts_with_keep(&ids, false);
    }

    pub(crate) fn merge_selected_parts_with_keep(&mut self, ids: &[InstanceId], keep_sources: bool) {
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

    pub(crate) fn group_selected_parts(&mut self, name: String) {
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

    pub(crate) fn ungroup_selected_parts(&mut self) {
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

    pub(crate) fn select_instance(&mut self, id: Option<InstanceId>) {
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

    pub(crate) fn toggle_select_instance(&mut self, id: InstanceId) {
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

    pub(crate) fn select_tree_range(&mut self, ordered_ids: &[InstanceId], target: InstanceId) {
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

    pub(crate) fn add_sample_part(&mut self) {
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

    pub(crate) fn rotate_selected_90(&mut self, axis: usize) {
        self.rotate_selected_by(axis, 90.0);
    }

    pub(crate) fn rotate_selected_by(&mut self, axis: usize, deg: f32) {
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

    pub(crate) fn open_move_dialog(&mut self) {
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

    pub(crate) fn selected_primary_dims(&self) -> [f32; 3] {
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

    pub(crate) fn open_scale_dialog(&mut self) {
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

    pub(crate) fn reset_scale_in_dialog(&mut self) {
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

    pub(crate) fn apply_scale_selection(&mut self, factors: [f32; 3]) {
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

    pub(crate) fn apply_move_selection(&mut self, absolute: bool, xyz: [f32; 3]) {
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

    pub(crate) fn open_duplicate_dialog(&mut self) {
        if self.selected.is_none() {
            return;
        }
        self.dup_gap_mm = [self.job.clearance_mm; 3];
        self.dup_dialog_open = true;
    }

    pub(crate) fn select_group_instances(&mut self, gid: GroupId) {
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

    pub(crate) fn open_sinterbox_dialog(&mut self) {
        if self.selected_set.is_empty() {
            self.status = "Select part(s) to create a Sinterbox".to_owned();
            return;
        }
        let mut ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
        ids.sort_by_key(|id| id.0);
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

    pub(crate) fn current_sinterbox_config(&self) -> SinterboxConfig {
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

    pub(crate) fn apply_create_sinterbox(&mut self) {
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

    pub(crate) fn snapshot(&self) -> UndoSnapshot {
        UndoSnapshot {
            job: self.job.clone(),
            selected: self.selected,
            selected_set: self.selected_set.clone(),
            selection_anchor: self.selection_anchor,
        }
    }

    pub(crate) fn push_undo(&mut self) {
        self.undo_stack.push(self.snapshot());
        self.redo_stack.clear();
    }

    pub(crate) fn restore_snapshot(&mut self, snap: UndoSnapshot) {
        self.job = snap.job;
        self.selected = snap.selected;
        self.selected_set = snap.selected_set;
        self.selection_anchor = snap.selection_anchor;
        self.euler_edit.clear();
        self.drag_state = None;
        self.rotate_drag = None;
        self.rotate_gizmo_open = false;
        self.drag_zone = None;
        self.box_select = None;
        self.zoom_rect = None;
        self.zoom_rect_mode = false;
        self.moving_target = None;
        self.color_picker_undo_pushed = false;
        self.clear_align_face_mode();
        self.below_zero_import_dialog.clear();
        self.mark_dirty();
    }

    pub(crate) fn undo(&mut self) -> bool {
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

    pub(crate) fn redo(&mut self) -> bool {
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

    pub(crate) fn duplicate_selected(&mut self) {
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

    pub(crate) fn start_align_face_mode(&mut self, target: InstanceId) {
        self.clear_align_face_mode();
        self.align_face_target = Some(target);
        self.status = "Align mode: hover a face to highlight and click to snap it to -Z (Esc to cancel)".to_owned();
    }

    pub(crate) fn clear_align_face_mode(&mut self) {
        self.align_face_target = None;
        self.hovered_face = None;
        self.collision_overlay_nonempty = false;
        if let Ok(mut guard) = self.hl_slot.lock() {
            *guard = Some(GpuScene::default());
        }
        self.hl_key = self.hl_key.wrapping_add(1).max(1);
    }

    pub(crate) fn purge_unused_part_caches(&mut self) {
        let active: HashSet<PartId> = self.job.parts.iter().map(|p| p.id).collect();
        self.scene_cache.parts.retain(|pid, _| active.contains(pid));
        self.manifold_cache.retain(|pid, _| active.contains(pid));
        self.part_thumbs.retain(|pid, _| active.contains(pid));
        self.expanded_parts.retain(|pid| active.contains(pid));
    }

    pub(crate) fn delete_selected(&mut self) {
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
            self.purge_unused_part_caches();
            self.mark_dirty();
            self.status = if removed == 1 {
                "Removed selected instance".to_owned()
            } else {
                format!("Removed {removed} selected instances")
            };
        }
    }

    pub(crate) fn nudge_selected(&mut self, delta: [f32; 3]) {
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

    pub(crate) fn toggle_selected_hidden(&mut self) {
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

    pub(crate) fn toggle_selected_transparent(&mut self) {
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

    pub(crate) fn set_selected_color(&mut self, color: Option<[u8; 3]>) {
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

    pub(crate) fn open_color_picker(&mut self) {
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



    pub(crate) fn toggle_selected_locked(&mut self) {
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

    pub(crate) fn current_standard_view(&self) -> Option<StandardView> {
        const TOL: f64 = 0.05;
        if let Some(v) = self.camera.aligned_standard_view(TOL) {
            return Some(v);
        }
        let (_, iso_pitch) = StandardView::Iso.angles();
        let iso_yaw = std::f64::consts::FRAC_PI_4;
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

    pub(crate) fn is_orthographic(&self) -> bool {
        self.camera.fov <= 0.0
    }

    pub(crate) fn default_camera_for_job(&self) -> Camera {
        let mut cam = viewport::default_camera_with_origin(&self.job.machine, self.job.bed_origin);
        if self.orthographic_view {
            cam.distance *= (cam.fov * 0.5).tan() / 0.4;
            cam.fov = 0.0;
        }
        cam
    }

    pub(crate) fn toggle_projection(&mut self) {
        const PERSP_FOV: f64 = 0.65;
        if self.camera.fov > 0.0 {
            self.camera.distance *= (self.camera.fov * 0.5).tan() / 0.4;
            self.camera.fov = 0.0;
            self.orthographic_view = true;
            self.status = "Projection: Isometric (Orthographic)".to_owned();
        } else {
            self.camera.distance *= 0.4 / (PERSP_FOV * 0.5).tan();
            self.camera.fov = PERSP_FOV;
            self.orthographic_view = false;
            self.status = "Projection: Perspective".to_owned();
        }
    }

    pub(crate) fn set_standard_view(&mut self, view: StandardView) {
        self.camera.set_view(view);
        if view == StandardView::Iso {
            self.camera.yaw = std::f64::consts::FRAC_PI_4;
        }
        self.status = format!("View: {}", view_label(view));
    }

    pub(crate) fn cycle_standard_view(&mut self) {
        let next = match self.current_standard_view().and_then(|cur| VIEW_CYCLE.iter().position(|&v| v == cur)) {
            Some(idx) => VIEW_CYCLE[(idx + 1) % VIEW_CYCLE.len()],
            None => StandardView::Iso,
        };
        self.set_standard_view(next);
    }

    pub(crate) fn arrow_nudge_axes(&self) -> ([f32; 3], [f32; 3]) {
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

    pub(crate) fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input()
            || self.custom_platform_open
            || self.dup_dialog_open
            || self.move_dialog_open
            || self.scale_dialog_open
            || self.sinterbox_dialog_open
            || self.color_picker_open
            || self.export_stl_dialog_open
            || self.report_dialog_open
            || self.failed_print_dialog_open
            || self.nest2d_dialog_open
            || self.nest3d_dialog_open
            || self.gravity_settle_dialog_open
            || !self.below_zero_import_dialog.is_empty()
            || self.pending_keep_delete.is_some()
            || self.load_task.is_some()
            || self.save_task.is_some()
            || self.calc_task.is_some()
        {
            return;
        }
        if self.is_nest3d_running() {
            ctx.input(|i| {
                let cmd = i.modifiers.command;
                let alt = i.modifiers.alt;
                let shift = i.modifiers.shift;
                if !cmd && !alt && i.key_pressed(egui::Key::Escape) {
                    self.request_nest3d_cancel();
                    return;
                }
                if !cmd && !alt && !shift && i.key_pressed(egui::Key::Space) {
                    self.cycle_standard_view();
                }
                if !cmd && !alt && i.key_pressed(egui::Key::F) {
                    self.camera = self.default_camera_for_job();
                }
                if !cmd && !alt {
                    if i.key_pressed(egui::Key::Num1) {
                        self.camera = self.camera.looking_from([0.0, -1.0, 0.0].into());
                    } else if i.key_pressed(egui::Key::Num3) {
                        self.camera = self.camera.looking_from([1.0, 0.0, 0.0].into());
                    } else if i.key_pressed(egui::Key::Num7) {
                        self.camera = self.camera.looking_from([0.0, 0.0, 1.0].into());
                    } else if i.key_pressed(egui::Key::Num0) {
                        self.camera = self.default_camera_for_job();
                    }
                }
            });
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
                self.trigger_save_bed(ctx);
                return;
            }
            if cmd && !shift && i.key_pressed(egui::Key::S) {
                self.open_scale_dialog();
                return;
            }
            if cmd && i.key_pressed(egui::Key::K) {
                self.toggle_collision_check();
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
                self.camera = self.default_camera_for_job();
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
                    self.camera = self.default_camera_for_job();
                }
            }
        });
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.scene_dirty = true;
        if self.slice_dist_open {
            self.slice_dist_dirty = true;
        }
        if self.thermal_map_open {
            self.refresh_thermal_map();
        }
        if self.collision_check_active {
            self.refresh_collision_check();
        }
    }

    pub(crate) fn apply_collision_overlay(&mut self, rep: &nestcraft_core::collision::CollisionReport) {
        let is_empty = rep.colliding_ids.is_empty()
            && rep.overlap_voxels.is_empty()
            && rep.overlap_quads.is_empty()
            && rep.intersection_segments.is_empty()
            && rep.contacts.is_empty();
        if is_empty && !self.collision_overlay_nonempty {
            return;
        }
        let hl = viewport::build_collision_highlight_scene(rep);
        if let Ok(mut guard) = self.hl_slot.lock() {
            *guard = Some(hl);
        }
        self.hl_key = self.hl_key.wrapping_add(1).max(1);
        self.collision_overlay_nonempty = !is_empty;
    }

    pub(crate) fn apply_collision_report(
        &mut self,
        rep: &nestcraft_core::collision::CollisionReport,
        update_status: bool,
    ) {
        if self.scene_cache.collision_ids != rep.colliding_ids {
            self.scene_cache.collision_ids = rep.colliding_ids.clone();
            self.scene_dirty = true;
        }
        self.apply_collision_overlay(rep);
        if update_status {
            self.status = if rep.pairs.is_empty() {
                format!("Collision Check: No collisions ({:.1} mm clearance)", rep.clearance_mm)
            } else {
                format!(
                    "Collision Check: {} colliding pair(s), {} overlap voxel(s)",
                    rep.pairs.len(),
                    rep.overlap_voxels.len()
                )
            };
        }
    }

    pub(crate) fn clear_collision_highlight(&mut self) {
        if !self.scene_cache.collision_ids.is_empty() {
            self.scene_cache.collision_ids.clear();
            self.scene_dirty = true;
        }
        self.collision_overlay_nonempty = false;
        if let Ok(mut guard) = self.hl_slot.lock() {
            *guard = Some(GpuScene::default());
        }
        self.hl_key = self.hl_key.wrapping_add(1).max(1);
    }

    pub(crate) fn refresh_collision_check(&mut self) {
        self.start_collision_check(false);
    }

    pub(crate) fn toggle_collision_check(&mut self) {
        if self.calc_task.as_ref().is_some_and(|t| t.title == "Collision Check") {
            self.calc_task = None;
        }
        self.collision_check_active = !self.collision_check_active;
        if self.collision_check_active {
            self.collision_cache.clear();
            self.start_collision_check(true);
        } else {
            self.collision_cache.clear();
            self.clear_collision_highlight();
            self.status = "Collision Check disabled".to_owned();
            self.scene_dirty = true;
        }
    }

    pub(crate) fn spawn_calc_task(
        &mut self,
        title: &str,
        show_progress: bool,
        work: impl FnOnce(Arc<AtomicUsize>, Arc<AtomicUsize>) -> CalcOutcome + Send + 'static,
    ) {
        let completed = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(1));
        let c = completed.clone();
        let t = total.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let out = work(c, t);
            let _ = tx.send(out);
        });
        self.calc_task = Some(BackgroundCalcTask {
            title: title.to_owned(),
            show_progress,
            completed,
            total,
            rx,
        });
    }

    pub(crate) fn start_collision_check(&mut self, show_progress: bool) {
        use nestcraft_core::collision::CollisionDiff;
        match self.collision_cache.diff(&self.job, COLLISION_VOXEL_PITCH_MM) {
            CollisionDiff::NoChange => return,
            CollisionDiff::RemovalsOnly { .. } => {
                let rep = self.collision_cache.sync(&self.job, COLLISION_VOXEL_PITCH_MM);
                self.apply_collision_report(&rep, true);
                return;
            }
            CollisionDiff::Incremental { ref dirty_ids, .. }
                if self.collision_cache.count_candidate_pairs(&self.job, dirty_ids) == 0 =>
            {
                let rep = self.collision_cache.sync(&self.job, COLLISION_VOXEL_PITCH_MM);
                self.apply_collision_report(&rep, true);
                return;
            }
            CollisionDiff::FullRebuild
                if !show_progress
                    && !self.collision_cache.is_initialized()
                    && self
                        .calc_task
                        .as_ref()
                        .is_some_and(|t| t.title == "Collision Check") =>
            {
                return;
            }
            CollisionDiff::Incremental { .. } | CollisionDiff::FullRebuild => {}
        }

        let job = self.job.clone();
        let mut cache = self.collision_cache.clone();
        self.spawn_calc_task("Collision Check", show_progress, move |c, t| {
            let rep = cache.sync_with_progress(
                &job,
                COLLISION_VOXEL_PITCH_MM,
                Some(&|done, tot| {
                    c.store(done, Ordering::Relaxed);
                    t.store(tot.max(1), Ordering::Relaxed);
                }),
            );
            CalcOutcome::Collision {
                report: rep,
                cache: Box::new(cache),
            }
        });
    }

    pub(crate) fn request_nest3d_stop_and_apply(&mut self) {
        if let Some(task) = &self.nest3d_task {
            task.stop_and_apply.store(true, Ordering::Relaxed);
            self.status = "Stopping 3D Nest and applying best layout...".to_owned();
        }
    }

    pub(crate) fn request_nest3d_cancel(&mut self) {
        if let Some(task) = &self.nest3d_task {
            task.cancel.store(true, Ordering::Relaxed);
            self.status = "Cancelling 3D Nest...".to_owned();
        }
    }

    pub(crate) fn start_nest3d(&mut self, ctx: &egui::Context) {
        if self.is_nest3d_running() || self.calc_task.is_some() || self.job.instances.is_empty() {
            return;
        }
        self.nest3d_dialog_open = false;
        self.drag_state = None;
        self.rotate_drag = None;
        self.drag_zone = None;
        self.moving_target = None;
        self.clear_align_face_mode();

        let pre_nest_snapshot = self.snapshot();
        let mut job = self.job.clone();
        let cfg = self.nest3d_config.clone();
        let sel = self.selected_set.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop_and_apply = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let initial_progress = nestcraft_core::nest3d::Nest3dProgress {
            phase: nestcraft_core::nest3d::Nest3dPhase::Preprocessing,
            label: "Preparing 3D voxel masks...".to_owned(),
            done: 0,
            total: 1,
            best_max_z_mm: self.job.max_z_mm(),
            best_density_pct: self.job.packing_density_pct(),
            best_placed: 0,
            best_overflow: self.job.instances.len(),
            best_thermal_peak_pct: None,
            generation: 0,
            elapsed_ms: 0,
            live_transforms: None,
        };

        let cancel_bg = Arc::clone(&cancel);
        let stop_bg = Arc::clone(&stop_and_apply);
        let ctx_bg = ctx.clone();
        std::thread::spawn(move || {
            let cb = |prog: nestcraft_core::nest3d::Nest3dProgress| {
                let _ = tx.send(Nest3dMsg::Progress(prog));
                ctx_bg.request_repaint();
            };
            let res = nestcraft_core::nest3d::nest_3d_with_callbacks(
                &mut job,
                &cfg,
                Some(&sel),
                Some(cancel_bg.as_ref()),
                Some(stop_bg.as_ref()),
                Some(&cb),
            );
            let msg = match res {
                Ok(result) => {
                    if cancel_bg.load(Ordering::Relaxed) {
                        Nest3dMsg::Cancelled
                    } else {
                        Nest3dMsg::Finished {
                            job: Box::new(job),
                            result,
                        }
                    }
                }
                Err(e) => {
                    if cancel_bg.load(Ordering::Relaxed) {
                        Nest3dMsg::Cancelled
                    } else {
                        Nest3dMsg::Failed(e.to_string())
                    }
                }
            };
            let _ = tx.send(msg);
            ctx_bg.request_repaint();
        });

        self.nest3d_task = Some(ActiveNest3dTask {
            cancel,
            stop_and_apply,
            pre_nest_snapshot,
            started_at: std::time::Instant::now(),
            latest: initial_progress,
            rx,
        });
        self.sync_nest3d_ceiling_overlay();
        self.status = "3D Nesting running (camera orbit/zoom active; Esc to cancel)...".to_owned();
    }

    pub(crate) fn start_gravity_settle(&mut self, ctx: &egui::Context) {
        if self.is_nest3d_running() || self.calc_task.is_some() || self.job.instances.is_empty() {
            return;
        }
        self.gravity_settle_dialog_open = false;
        self.drag_state = None;
        self.rotate_drag = None;
        self.drag_zone = None;
        self.moving_target = None;
        self.clear_align_face_mode();

        let pre_nest_snapshot = self.snapshot();
        let mut job = self.job.clone();
        let cfg = self.gravity_settle_config.clone();
        let sel = self.selected_set.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop_and_apply = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let total_vol: f64 = job
            .instances
            .iter()
            .filter(|i| job.is_instance_visible(i))
            .filter_map(|i| job.part(i.part_id).map(|p| p.mesh.volume_mm3))
            .sum();
        let [w, d, _] = job.machine.size_mm();
        let bed_area = f64::from(w) * f64::from(d);
        let initial_placed = job.instances.iter().filter(|i| job.is_instance_visible(i)).count();

        let initial_progress = nestcraft_core::nest3d::Nest3dProgress {
            phase: nestcraft_core::nest3d::Nest3dPhase::Compacting,
            label: "Building physics collision shapes (V-HACD)...".to_owned(),
            done: 0,
            total: cfg.max_steps.max(1) as usize,
            best_max_z_mm: self.job.max_z_mm(),
            best_density_pct: self.job.packing_density_pct(),
            best_placed: initial_placed,
            best_overflow: 0,
            best_thermal_peak_pct: None,
            generation: 0,
            elapsed_ms: 0,
            live_transforms: None,
        };

        let cancel_bg = Arc::clone(&cancel);
        let stop_bg = Arc::clone(&stop_and_apply);
        let ctx_bg = ctx.clone();
        let started_bg = std::time::Instant::now();
        std::thread::spawn(move || {
            let cb = |sp: nestcraft_core::compact::GravitySettleProgress| {
                let density = if bed_area > 1e-4 && sp.max_z_mm > 1e-4 {
                    (total_vol / (bed_area * f64::from(sp.max_z_mm))) * 100.0
                } else {
                    0.0
                };
                let prog = nestcraft_core::nest3d::Nest3dProgress {
                    phase: nestcraft_core::nest3d::Nest3dPhase::Compacting,
                    label: format!(
                        "Gravity Settle step {}/{} ({}/{} sleeping, -Z {:.1} mm)",
                        sp.step, sp.total_steps, sp.sleeping_count, sp.dynamic_count, sp.z_reduction_mm
                    ),
                    done: sp.step as usize,
                    total: sp.total_steps.max(1) as usize,
                    best_max_z_mm: sp.max_z_mm,
                    best_density_pct: density,
                    best_placed: initial_placed,
                    best_overflow: 0,
                    best_thermal_peak_pct: None,
                    generation: 0,
                    elapsed_ms: started_bg.elapsed().as_millis() as u64,
                    live_transforms: sp.preview_transforms,
                };
                let _ = tx.send(Nest3dMsg::Progress(prog));
                ctx_bg.request_repaint();
            };
            let res = nestcraft_core::compact::gravity_settle_with_callbacks(
                &mut job,
                &cfg,
                Some(&sel),
                None,
                Some(cancel_bg.as_ref()),
                Some(stop_bg.as_ref()),
                Some(&cb),
            );
            let msg = match res {
                Ok(result) => {
                    if cancel_bg.load(Ordering::Relaxed) {
                        Nest3dMsg::Cancelled
                    } else {
                        Nest3dMsg::SettleFinished {
                            job: Box::new(job),
                            result,
                        }
                    }
                }
                Err(e) => {
                    if cancel_bg.load(Ordering::Relaxed) {
                        Nest3dMsg::Cancelled
                    } else {
                        Nest3dMsg::Failed(e.to_string())
                    }
                }
            };
            let _ = tx.send(msg);
            ctx_bg.request_repaint();
        });

        self.nest3d_task = Some(ActiveNest3dTask {
            cancel,
            stop_and_apply,
            pre_nest_snapshot,
            started_at: std::time::Instant::now(),
            latest: initial_progress,
            rx,
        });
        self.status = "Gravity Settle running (camera orbit/zoom active; Esc to cancel)...".to_owned();
    }

    pub(crate) fn poll_nest3d_task(&mut self, ctx: &egui::Context) {
        let Some(mut task) = self.nest3d_task.take() else {
            return;
        };
        let mut finished: Option<Nest3dMsg> = None;
        let mut latest_live: Option<Vec<(InstanceId, nestcraft_core::model::Transform3x4)>> = None;
        loop {
            match task.rx.try_recv() {
                Ok(Nest3dMsg::Progress(mut prog)) => {
                    if let Some(live) = prog.live_transforms.take() {
                        latest_live = Some(live);
                    }
                    task.latest = prog;
                }
                Ok(msg) => {
                    finished = Some(msg);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = Some(Nest3dMsg::Cancelled);
                    break;
                }
            }
        }
        if finished.is_none()
            && let Some(live) = latest_live
        {
            if live.len() > 8 {
                let by_id: HashMap<InstanceId, nestcraft_core::model::Transform3x4> =
                    live.into_iter().collect();
                for inst in &mut self.job.instances {
                    if let Some(&tf) = by_id.get(&inst.id) {
                        inst.transform = tf;
                    }
                }
            } else {
                for (iid, tf) in live {
                    if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == iid) {
                        inst.transform = tf;
                    }
                }
            }
            self.scene_dirty = true;
        }

        if let Some(msg) = finished {
            match msg {
                Nest3dMsg::Finished { job, result } => {
                    self.undo_stack.push(task.pre_nest_snapshot);
                    self.redo_stack.clear();
                    self.job = *job;
                    self.euler_edit.clear();
                    self.mark_dirty();
                    let stopped_note = if result.stopped_early { " (stopped early)" } else { "" };
                    let iter_note = if result.stage2_iterations > 0 {
                        format!(" | {} refine iters", result.stage2_iterations)
                    } else {
                        String::new()
                    };
                    let thermal_note = result
                        .peak_thermal_pct
                        .map_or_else(String::new, |tp| format!(" | Thermal {tp:.1}%"));
                    self.status = format!(
                        "3D Nest{stopped_note}: {} placed, {} overflow | Max Z {:.1} mm | Density {:.2}%{iter_note}{thermal_note} ({} ms)",
                        result.placed_count,
                        result.overflow_count,
                        result.max_z_mm,
                        result.packing_density_pct,
                        result.elapsed_ms,
                    );
                    self.nest3d_last_result = Some(result);
                }
                Nest3dMsg::SettleFinished { job, result } => {
                    self.undo_stack.push(task.pre_nest_snapshot);
                    self.redo_stack.clear();
                    self.job = *job;
                    self.euler_edit.clear();
                    self.mark_dirty();
                    let stopped_note = if result.stopped_early { " (stopped early)" } else { "" };
                    let rb_note = if result.rolled_back_count > 0 {
                        format!(", {} rolled back", result.rolled_back_count)
                    } else {
                        String::new()
                    };
                    self.status = format!(
                        "Gravity Settle{stopped_note}: {} moved{rb_note} | Max Z {:.1} -> {:.1} mm (-{:.1} mm, {} steps, {} ms)",
                        result.moved_count,
                        result.max_z_before_mm,
                        result.max_z_after_mm,
                        result.z_reduction_mm,
                        result.steps_run,
                        result.elapsed_ms,
                    );
                    self.gravity_settle_last_result = Some(result);
                }
                Nest3dMsg::Cancelled => {
                    self.restore_snapshot(task.pre_nest_snapshot);
                    self.status = "3D Nesting cancelled — bed restored".to_owned();
                }
                Nest3dMsg::Failed(err) => {
                    self.restore_snapshot(task.pre_nest_snapshot);
                    self.status = format!("3D Nest error: {err}");
                }
                Nest3dMsg::Progress(_) => {}
            }
            self.sync_nest3d_ceiling_overlay();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            self.nest3d_task = Some(task);
        }
    }

    pub(crate) fn start_nest2d(&mut self) {
        let mut job = self.job.clone();
        let cfg = self.nest2d_config.clone();
        let sel = self.selected_set.clone();
        self.spawn_calc_task("2D Auto-Nest", true, move |c, t| {
            match nestcraft_core::nest2d::nest_2d_with_progress(
                &mut job,
                &cfg,
                Some(&sel),
                Some(&|done, tot| {
                    c.store(done, Ordering::Relaxed);
                    t.store(tot.max(1), Ordering::Relaxed);
                }),
            ) {
                Ok(result) => CalcOutcome::Nest2d {
                    job: Box::new(job),
                    result,
                },
                Err(e) => CalcOutcome::Nest2dFailed(e.to_string()),
            }
        });
    }

    pub(crate) fn start_thermal_map(&mut self) {
        let show_progress = self.thermal_report.is_none();
        let job = self.job.clone();
        self.spawn_calc_task("Thermal Map", show_progress, move |c, t| {
            let rep = nestcraft_core::thermal::ThermalMapReport::compute_with_progress(
                &job,
                Some(&|done, tot| {
                    c.store(done, Ordering::Relaxed);
                    t.store(tot.max(1), Ordering::Relaxed);
                }),
            );
            CalcOutcome::Thermal(rep)
        });
    }

    pub(crate) fn start_slice_distribution(&mut self) {
        let show_progress = self.slice_dist_cache.is_none();
        self.slice_dist_dirty = false;
        let job = self.job.clone();
        let step = self.slice_dist_step_mm;
        self.spawn_calc_task("Slice Distribution", show_progress, move |c, t| {
            let dist = nestcraft_core::slice::SliceDistribution::compute_with_progress(
                &job,
                step,
                Some(&|done, tot| {
                    c.store(done, Ordering::Relaxed);
                    t.store(tot.max(1), Ordering::Relaxed);
                }),
            );
            CalcOutcome::Slice(dist)
        });
    }

    pub(crate) fn poll_calc_task(&mut self, ctx: &egui::Context) {
        let Some(task) = &self.calc_task else {
            return;
        };
        ctx.request_repaint();
        match task.rx.try_recv() {
            Ok(outcome) => {
                self.calc_task = None;
                match outcome {
                    CalcOutcome::Collision { report, cache } => {
                        if self.collision_check_active {
                            self.collision_cache = *cache;
                            self.apply_collision_report(&report, true);
                            if self.collision_cache.diff(&self.job, COLLISION_VOXEL_PITCH_MM)
                                != nestcraft_core::collision::CollisionDiff::NoChange
                            {
                                self.start_collision_check(false);
                            }
                        }
                    }
                    CalcOutcome::Nest2d { job, result } => {
                        self.push_undo();
                        self.job = *job;
                        self.euler_edit.clear();
                        self.mark_dirty();
                        self.status = format!(
                            "2D Nest: {} placed, {} overflow ({:.1}% fill, {} ms)",
                            result.placed_count, result.overflow_count, result.used_area_pct, result.elapsed_ms
                        );
                        self.nest2d_last_result = Some(result);
                    }
                    CalcOutcome::Nest2dFailed(e) => {
                        self.status = format!("2D Nest error: {e}");
                    }
                    CalcOutcome::Thermal(rep) => {
                        let mut colors = HashMap::with_capacity(rep.parts.len());
                        for p in &rep.parts {
                            colors.insert(p.instance_id, nestcraft_core::thermal::thermal_rgba(p.peak_density_pct));
                        }
                        self.scene_cache.thermal_colors = Some(colors);
                        self.thermal_report = Some(rep);
                        self.scene_dirty = true;
                        self.status = "Thermal Map updated (20×20×20 mm local sintered density)".to_owned();
                    }
                    CalcOutcome::Slice(dist) => {
                        self.slice_dist_cache = Some(dist);
                        self.slice_dist_dirty = false;
                        self.status = "Slice Distribution updated".to_owned();
                    }
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.calc_task = None;
            }
        }
    }

    pub(crate) fn update_live_drag_collision(&mut self) {
        if !self.collision_check_active {
            return;
        }
        let mut active = HashSet::new();
        if let Some(ds) = &self.drag_state {
            for (id, _) in &ds.instances {
                active.insert(*id);
            }
        }
        if let Some(rd) = &self.rotate_drag {
            for (id, _) in &rd.instances {
                active.insert(*id);
            }
        }
        if active.len() != 1 {
            // Defer multi-part or empty live collision checks until drag release (`mark_dirty`).
            return;
        }
        if self.collision_cache.is_initialized()
            && !matches!(
                self.collision_cache.diff(&self.job, COLLISION_VOXEL_PITCH_MM),
                nestcraft_core::collision::CollisionDiff::FullRebuild
            )
        {
            let rep = self.collision_cache.sync(&self.job, COLLISION_VOXEL_PITCH_MM);
            self.apply_collision_report(&rep, false);
        }
    }

    pub(crate) fn refresh_thermal_map(&mut self) {
        self.start_thermal_map();
    }

    pub(crate) fn toggle_thermal_map(&mut self) {
        self.thermal_map_open = !self.thermal_map_open;
        if self.thermal_map_open {
            self.start_thermal_map();
        } else {
            self.scene_cache.thermal_colors = None;
            self.thermal_report = None;
            self.status = "Thermal Map disabled".to_owned();
            self.scene_dirty = true;
        }
    }

    pub(crate) fn import_no_build_zone_stl(&mut self, path: &Path) {
        if !slot_supports_no_build_zone(self.active_machine_slot) {
            self.status = "Selected machine does not use a No-Build Zone".to_owned();
            return;
        }
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

    pub(crate) fn persist_no_build_zone(&mut self) {
        if let (Some(dir), Some(zone)) = (no_build_zone_dir_for_slot(self.active_machine_slot), &self.job.no_build_zone)
            && let Err(e) = save_no_build_zone_config_to_dir(&dir, zone)
        {
            self.status = e;
        }
    }

    #[cfg(test)]
    pub(crate) fn save_bed_file(&mut self, path: &Path) {
        let saved_config = SavedBedConfig::from_job(&self.job);
        match nestc::write_nestc(&self.job) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, bytes) {
                    self.status = format!("Save bed failed: {e}");
                } else {
                    self.current_bed_path = Some(path.to_path_buf());
                    self.last_saved_config = Some(saved_config);
                    self.status = format!("Saved bed to {}", path.display());
                }
            }
            Err(e) => {
                self.status = format!("Save bed error: {e}");
            }
        }
    }

    pub(crate) fn quick_save_target(&self) -> Option<PathBuf> {
        let path = self.current_bed_path.as_ref()?;
        let saved = self.last_saved_config.as_ref()?;
        if saved == &SavedBedConfig::from_job(&self.job) {
            Some(path.clone())
        } else {
            None
        }
    }

    pub(crate) fn trigger_save_bed(&mut self, ctx: &egui::Context) {
        if self.save_task.is_some() {
            return;
        }
        if let Some(path) = self.quick_save_target() {
            self.start_save_bed(path, ctx);
            return;
        }
        let mut dialog = rfd::FileDialog::new()
            .add_filter("NestCraft Bed", &["nestc"])
            .set_file_name(self.default_save_filename("nestc"));
        if let Some(parent) = self.current_bed_path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(parent);
        }
        if let Some(path) = dialog.save_file() {
            self.start_save_bed(path, ctx);
        }
    }

    pub(crate) fn start_save_bed(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.save_task.is_some() {
            return;
        }
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("bed.nestc")
            .to_owned();
        let path_display = path.display().to_string();
        let job = self.job.clone();
        let saved_config = SavedBedConfig::from_job(&job);
        let progress_permille = Arc::new(AtomicUsize::new(0));
        let stage = Arc::new(Mutex::new("Preparing parts...".to_owned()));
        let (tx, rx) = mpsc::channel();

        let p_bg = Arc::clone(&progress_permille);
        let s_bg = Arc::clone(&stage);
        let ctx_bg = ctx.clone();
        let fname_bg = file_name.clone();
        std::thread::spawn(move || {
            let res = nestc::write_nestc_with_progress(
                &job,
                Some(&|done, total, label| {
                    let permille = ((done.saturating_mul(950)) / total.max(1)).min(950);
                    p_bg.store(permille, Ordering::Relaxed);
                    if let Ok(mut guard) = s_bg.lock() {
                        *guard = label.to_owned();
                    }
                    ctx_bg.request_repaint();
                }),
            );
            let outcome = match res {
                Ok(bytes) => {
                    if let Ok(mut guard) = s_bg.lock() {
                        *guard = "Writing file...".to_owned();
                    }
                    p_bg.store(960, Ordering::Relaxed);
                    ctx_bg.request_repaint();
                    match std::fs::write(&path, bytes) {
                        Ok(()) => {
                            p_bg.store(1000, Ordering::Relaxed);
                            SaveOutcome::Saved {
                                path,
                                path_display,
                                file_name: fname_bg,
                                saved_config,
                            }
                        }
                        Err(e) => SaveOutcome::Failed(format!("Save bed failed: {e}")),
                    }
                }
                Err(e) => SaveOutcome::Failed(format!("Save bed error: {e}")),
            };
            let _ = tx.send(outcome);
            ctx_bg.request_repaint();
        });

        self.save_hint = None;
        self.status = format!("Saving {file_name}...");
        self.save_task = Some(ActiveSaveTask {
            file_name,
            progress_permille,
            stage,
            rx,
        });
    }

    pub(crate) fn poll_save_task(&mut self, ctx: &egui::Context) {
        if let Some((_, _, at)) = &self.save_hint
            && at.elapsed() >= std::time::Duration::from_millis(2500)
        {
            self.save_hint = None;
        }
        let Some(task) = &self.save_task else {
            return;
        };
        ctx.request_repaint();
        match task.rx.try_recv() {
            Ok(SaveOutcome::Saved {
                path,
                path_display,
                file_name,
                saved_config,
            }) => {
                self.save_task = None;
                self.current_bed_path = Some(path);
                self.last_saved_config = Some(saved_config);
                self.status = format!("Saved bed to {path_display}");
                self.save_hint = Some((format!("Saved {file_name}"), false, std::time::Instant::now()));
            }
            Ok(SaveOutcome::Failed(msg)) => {
                self.save_task = None;
                self.status = msg.clone();
                self.save_hint = Some((msg, true, std::time::Instant::now()));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.save_task = None;
            }
        }
    }

    pub(crate) fn default_save_filename(&self, ext: &str) -> String {
        format_job_save_filename(
            &today_yyyymmdd(),
            self.job.machine.name(),
            &self.job.material_id,
            ext,
        )
    }

    pub(crate) fn apply_loaded_bed(&mut self, job: BuildJob, path_display: &str, source_path: Option<PathBuf>) {
        self.job = job;
        match source_path {
            Some(path) => {
                self.last_saved_config = Some(SavedBedConfig::from_job(&self.job));
                self.current_bed_path = Some(path);
            }
            None => {
                self.current_bed_path = None;
                self.last_saved_config = None;
            }
        }
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
        self.collision_cache.clear();
        self.slice_dist_cache = None;
        self.slice_dist_dirty = false;
        self.thermal_report = None;
        self.camera = self.default_camera_for_job();
        self.mark_dirty();
        self.status = format!("Opened bed {path_display}");
    }

    #[cfg(test)]
    pub(crate) fn open_bed_file(&mut self, path: &Path) {
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
                self.apply_loaded_bed(job, &display, Some(path.to_path_buf()));
            }
            Err(e) => {
                self.status = format!("Open bed error: {e}");
            }
        }
    }

    pub(crate) fn start_import_files(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
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

    pub(crate) fn start_open_bed(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.start_import_files(vec![path], ctx);
    }

    pub(crate) fn poll_load_task(&mut self, ctx: &egui::Context) {
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
                                        let (pid, created) = self.job.add_or_reuse_part(name, mesh);
                                        if created || !self.scene_cache.parts.contains_key(&pid) {
                                            self.scene_cache.parts.insert(pid, shared_tris);
                                        }
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
                                            let (pid, created) = self.job.add_or_reuse_part(item.name, item.mesh);
                                            if created || !self.scene_cache.parts.contains_key(&pid) {
                                                self.scene_cache.parts.insert(pid, shared_tris);
                                            }
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
                            BackgroundLoadOutcome::OpenedBed {
                                job,
                                path,
                                path_display,
                            } => {
                                self.apply_loaded_bed(*job, &path_display, path);
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

    pub(crate) fn export_individual_stls(&self, dir: &Path) -> Result<usize, String> {
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

    pub(crate) fn clear_no_build_zone(&mut self) {
        self.job.no_build_zone = None;
        if let Some(dir) = no_build_zone_dir_for_slot(self.active_machine_slot) {
            let _ = std::fs::remove_file(dir.join("zone.stl"));
            let _ = std::fs::remove_file(dir.join("config.json"));
        }
        self.mark_dirty();
        self.status = "Cleared No-Build Zone".to_owned();
    }

    #[cfg(test)]
    pub(crate) fn import_single_file(&mut self, path: &Path) -> Result<(usize, String), String> {
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
                let (pid, _) = self.job.add_or_reuse_part(item.name, item.mesh);
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
            let (pid, _) = self.job.add_or_reuse_part(name.clone(), Arc::new(mesh));
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
    pub(crate) fn import_files<P: AsRef<Path>>(&mut self, paths: &[P]) {
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

    pub(crate) fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        if dropped.is_empty() || self.is_nest3d_running() {
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


    pub(crate) fn export_threemf_file(&mut self, path: &Path) {
        match threemf::write_threemf(&self.job) {
            Ok(bytes) => match std::fs::write(path, bytes) {
                Ok(()) => self.status = format!("Exported 3MF to {}", path.display()),
                Err(e) => self.status = format!("Write error: {e}"),
            },
            Err(e) => self.status = format!("3MF export error: {e}"),
        }
    }

    pub(crate) fn start_moving_layer(&mut self, target: viewport::DragExclude) {
        let tokens = Tokens::get();
        let scene = viewport::build_moving_scene(&self.job, &self.selected_set, &tokens, &mut self.scene_cache, &target);
        if let Ok(mut guard) = self.moving_slot.lock() {
            *guard = Some(scene);
        }
        self.moving_key = self.moving_key.wrapping_add(1).max(1);
        self.moving_target = Some(target);
        self.scene_dirty = true;
    }

    pub(crate) fn current_moving_layer(&self) -> Option<MovingLayer> {
        let target = self.moving_target.as_ref()?;
        if matches!(target, viewport::DragExclude::Instances(_))
            && let Some(rd) = &self.rotate_drag
        {
            return Some(MovingLayer {
                key: self.moving_key,
                slot: self.moving_slot.clone(),
                transform: rotation_matrix_4x4_col_major(rd.pivot, world_axis(rd.axis), rd.angle_rad),
            });
        }
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

    pub(crate) fn sync_scene_if_dirty(&mut self) {
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


    pub(crate) fn reset_below_zero_imported_parts(&mut self) {
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

}

pub(crate) fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    (year, m as u32, d as u32)
}

pub(crate) fn today_yyyymmdd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}")
}

fn sanitize_slug(s: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_underscore = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !out.is_empty() && !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        fallback.to_owned()
    } else {
        out
    }
}

fn short_bed_slug(machine_name: &str) -> String {
    let lower = machine_name.to_ascii_lowercase();
    if lower.contains("ht403") {
        "ht403".to_owned()
    } else if lower.contains("ss403") {
        "ss403".to_owned()
    } else if lower.contains("velocis") {
        "p110v".to_owned()
    } else if lower.contains("p100") {
        "p100".to_owned()
    } else if lower.contains("p110") || lower.contains("formiga") {
        "p110".to_owned()
    } else {
        sanitize_slug(machine_name, "bed")
    }
}

fn short_material_slug(material_id: &str) -> String {
    match material_id.trim().to_ascii_lowercase().as_str() {
        "pa2200" => "2200".to_owned(),
        "fs3300" => "3300".to_owned(),
        "pa2210fr" => "2210fr".to_owned(),
        "fs3401gb" => "3401gb".to_owned(),
        "pbt01" => "pbt01".to_owned(),
        "pa3200gb" => "3200gb".to_owned(),
        _ => sanitize_slug(material_id, "mat"),
    }
}

pub(crate) fn format_job_save_filename(
    date_yyyymmdd: &str,
    machine_name: &str,
    material_id: &str,
    ext: &str,
) -> String {
    let bed = short_bed_slug(machine_name);
    let mat = short_material_slug(material_id);
    format!("{date_yyyymmdd}_{bed}_{mat}.{ext}")
}

pub(crate) fn world_axis(axis: usize) -> [f32; 3] {
    match axis {
        0 => [1.0, 0.0, 0.0],
        1 => [0.0, 1.0, 0.0],
        _ => [0.0, 0.0, 1.0],
    }
}

pub(crate) fn rotation_matrix_4x4_col_major(pivot: [f32; 3], axis: [f32; 3], angle_rad: f32) -> [[f32; 4]; 4] {
    let len = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
    if len < 1e-6 || angle_rad.abs() < 1e-6 {
        return [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
    }
    let (ux, uy, uz) = (axis[0] / len, axis[1] / len, axis[2] / len);
    let (s, c) = angle_rad.sin_cos();
    let t = 1.0 - c;
    let r = [
        [t * ux * ux + c, t * ux * uy - s * uz, t * ux * uz + s * uy],
        [t * ux * uy + s * uz, t * uy * uy + c, t * uy * uz - s * ux],
        [t * ux * uz - s * uy, t * uy * uz + s * ux, t * uz * uz + c],
    ];
    let tx = pivot[0] - (r[0][0] * pivot[0] + r[0][1] * pivot[1] + r[0][2] * pivot[2]);
    let ty = pivot[1] - (r[1][0] * pivot[0] + r[1][1] * pivot[1] + r[1][2] * pivot[2]);
    let tz = pivot[2] - (r[2][0] * pivot[0] + r[2][1] * pivot[1] + r[2][2] * pivot[2]);
    [
        [r[0][0], r[1][0], r[2][0], 0.0],
        [r[0][1], r[1][1], r[2][1], 0.0],
        [r[0][2], r[1][2], r[2][2], 0.0],
        [tx, ty, tz, 1.0],
    ]
}

pub(crate) fn rotate_transform_around_pivot(
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

pub(crate) fn create_sample_bracket_mesh(variant: u32) -> nestcraft_core::Result<TriMesh> {
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

