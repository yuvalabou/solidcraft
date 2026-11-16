//! SolidCraft SLS 3D nesting desktop application.

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

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use solidcraft_sls_core::io::{stl, threemf};
use solidcraft_sls_core::model::{
    BuildJob, InstanceId, MachineProfile, PartId, RotationConstraint, TriMesh,
    transform_from_translation_euler_deg,
};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Edge, Surface};
use solidcraft_ui_kit::view_compass::{self, Compass, Drag as CompassDrag, ViewAction, ViewAxes};
use solidcraft_ui_kit::{
    Button, Segments, StageBar, StatusTone, button, catalog_footer_with_tip, checkbox_row,
    icon_label_button, row, scope, section, section_count, segmented, status_line, value_row,
};
use solidcraft_viewport::Camera;
use solidcraft_viewport::vec::Vec3Ops as _;
use solidcraft_viewport_egui::{ViewportCallback, paint_background};
use solidcraft_viewport_gpu::view::{Frame, GpuTarget, SceneSlot, SharedTris};

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SolidCraft SLS")
            .with_inner_size([1280.0, 820.0]),
        renderer: eframe::Renderer::Wgpu,
        depth_buffer: 0,
        stencil_buffer: 0,
        multisampling: 1,
        ..Default::default()
    };

    eframe::run_native(
        "SolidCraft SLS",
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
                };
                let scene = GpuTarget {
                    depth: Some(solidcraft_viewport_gpu::depth_format(&rs.device, true)),
                    ..window
                };
                let _ = solidcraft_viewport_egui::install(rs, scene, window, false, false, |err| {
                    eprintln!("solidcraft-sls wgpu error: {err}");
                });
            }
            Ok(Box::new(SlsApp::new()))
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

struct SlsApp {
    job: BuildJob,
    selected: Option<InstanceId>,
    camera: Camera,
    shared_cache: HashMap<PartId, Arc<SharedTris>>,
    scene_slot: SceneSlot,
    hl_slot: SceneSlot,
    empty_slot: SceneSlot,
    scene_key: u64,
    scene_dirty: bool,
    status: String,
    euler_edit: HashMap<InstanceId, [f32; 3]>,
    dup_count: usize,
    drag_state: Option<DragState>,
    compass_drag: CompassDrag,
    sample_counter: u32,
}

impl SlsApp {
    fn new() -> Self {
        let job = BuildJob::new(MachineProfile::FormigaP110);
        let camera = viewport::default_camera(&job.machine);
        Self {
            job,
            selected: None,
            camera,
            shared_cache: HashMap::new(),
            scene_slot: Arc::new(Mutex::new(None)),
            hl_slot: Arc::new(Mutex::new(None)),
            empty_slot: Arc::new(Mutex::new(None)),
            scene_key: 1,
            scene_dirty: true,
            status: "Ready — import an STL/3MF or add a sample part.".to_owned(),
            euler_edit: HashMap::new(),
            dup_count: 1,
            drag_state: None,
            compass_drag: CompassDrag::default(),
            sample_counter: 0,
        }
    }

    fn select_instance(&mut self, id: Option<InstanceId>) {
        if self.selected != id {
            self.selected = id;
            if let Some(iid) = id
                && let Some(inst) = self.job.instances.iter().find(|i| i.id == iid)
            {
                self.euler_edit.insert(iid, inst.euler_deg());
            }
            self.mark_dirty();
        }
    }

    fn add_sample_part(&mut self) {
        self.sample_counter += 1;
        let idx = self.sample_counter;
        if let Ok(mesh) = create_sample_bracket_mesh(idx) {
            let name = format!("Sample Bracket #{idx}");
            let pid = self.job.add_part(name.clone(), Arc::new(mesh));
            if let Ok(iid) = self.job.add_instance_centered_on_floor(pid) {
                self.select_instance(Some(iid));
                self.status = format!("Added {name}");
            }
        }
    }

    fn rotate_selected_90(&mut self, axis: usize) {
        let Some(sel_id) = self.selected else {
            return;
        };
        if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == sel_id) {
            let pos = inst.translation();
            let euler = self.euler_edit.entry(sel_id).or_insert_with(|| inst.euler_deg());
            euler[axis] = (euler[axis] + 90.0).rem_euclid(360.0);
            inst.transform = transform_from_translation_euler_deg(pos, *euler);
            self.mark_dirty();
        }
    }

    fn mark_dirty(&mut self) {
        self.scene_dirty = true;
    }

    fn import_file(&mut self, path: &Path) {
        let Ok(bytes) = std::fs::read(path) else {
            self.status = format!("Failed to read {}", path.display());
            return;
        };
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();

        if ext == "3mf" {
            match threemf::read_threemf(&bytes) {
                Ok(items) => {
                    let count = items.len();
                    for item in items {
                        let pid = self.job.add_part(item.name, item.mesh);
                        if let Ok(iid) = self.job.add_instance(pid, item.transform) {
                            self.select_instance(Some(iid));
                        }
                    }
                    self.status = format!("Imported {count} build item(s) from {}", path.display());
                    self.mark_dirty();
                }
                Err(e) => self.status = format!("3MF import error: {e}"),
            }
        } else {
            match stl::parse_stl(&bytes) {
                Ok(mesh) => {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("part")
                        .to_owned();
                    let pid = self.job.add_part(name.clone(), Arc::new(mesh));
                    if let Ok(iid) = self.job.add_instance_centered_on_floor(pid) {
                        self.select_instance(Some(iid));
                    }
                    self.status = format!("Imported '{name}'");
                    self.mark_dirty();
                }
                Err(e) => self.status = format!("STL import error: {e}"),
            }
        }
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
        let scene = viewport::build_gpu_scene(&self.job, self.selected, &tokens, &mut self.shared_cache);
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
                            ui.label(egui::RichText::new("SolidCraft SLS").strong().color(t.text));
                            StageBar::separator(ui);

                            let mut machine_idx = match self.job.machine {
                                MachineProfile::FormigaP110 => 0,
                                MachineProfile::FarsoonHT403P => 1,
                                MachineProfile::Custom { .. } => 0,
                            };
                            let machine_changed = ui
                                .push_id("machine_seg", |ui| {
                                    segmented(
                                        ui,
                                        &mut machine_idx,
                                        &["Formiga P110", "Farsoon HT403P"],
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
                                self.job.machine = if machine_idx == 1 {
                                    MachineProfile::FarsoonHT403P
                                } else {
                                    MachineProfile::FormigaP110
                                };
                                self.camera = viewport::default_camera(&self.job.machine);
                                self.mark_dirty();
                            }

                            StageBar::separator(ui);
                            if icon_label_button(ui, "Import", "sls_import_mesh", true, false)
                                .backing_on_hover_text("Import STL or 3MF mesh into the build chamber")
                                .clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .add_filter("3D Mesh", &["stl", "STL", "3mf", "3MF"])
                                    .pick_file()
                            {
                                self.import_file(&path);
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

                            StageBar::separator(ui);
                            ui.add(egui::DragValue::new(&mut self.dup_count).range(1..=200).prefix("×"))
                                .backing_on_hover_text("Number of duplicate copies to place");
                            if icon_label_button(ui, "Duplicate", "sls_duplicate", self.selected.is_some(), false)
                                .backing_on_hover_text("Duplicate selected instance N times")
                                .clicked()
                                && let Some(sel) = self.selected
                            {
                                let count = self.dup_count.max(1);
                                if let Ok(ids) = self.job.duplicate_instance(sel, count) {
                                    let last = ids.last().copied();
                                    self.select_instance(last);
                                    self.status = format!("Duplicated {count} instance(s)");
                                }
                            }
                            if icon_label_button(ui, "Drop Z=0", "sls_orient_lock", self.selected.is_some(), false)
                                .backing_on_hover_text("Drop selected part to the build floor (Z = 0)")
                                .clicked()
                                && let Some(sel) = self.selected
                                && self.job.drop_instance_to_floor(sel)
                            {
                                self.mark_dirty();
                                self.status = "Dropped part to Z = 0".to_owned();
                            }
                            if icon_label_button(ui, "Delete", "delete", self.selected.is_some(), true)
                                .backing_on_hover_text("Remove selected instance from build")
                                .clicked()
                                && let Some(sel) = self.selected
                                && self.job.remove_instance(sel)
                            {
                                self.euler_edit.remove(&sel);
                                self.selected = None;
                                self.mark_dirty();
                                self.status = "Removed selected instance".to_owned();
                            }

                            StageBar::separator(ui);
                            icon_label_button(ui, "Auto-Nest", "sls_auto_nest", false, false)
                                .backing_on_hover_text("3D EDT Auto-Nest (coming in Stage 2)");
                            if icon_label_button(ui, "Fit View", "fit", true, false)
                                .backing_on_hover_text("Reset camera to default isometric build view")
                                .clicked()
                            {
                                self.camera = viewport::default_camera(&self.job.machine);
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
                    ui.label(egui::RichText::new(&self.status).size(t.font_small).color(t.text_dim));
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
                        ui.label(egui::RichText::new(metrics).size(t.font_small).color(t.text));
                    });
                });
            });
        });
    }

    fn draw_parts_browser(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let side_c = Container::new(ctx, Surface::Panel);
        let left_panel = side_c.edge("sls_parts_tree", Edge::Left).default_size(265.0);
        side_c.show_edge(ui, left_panel, side_c.edge_frame(t.panel), |ui| {
            scope(ui, |ui| {
                ui.label(egui::RichText::new("Build Parts Browser").strong().size(t.font_heading).color(t.form_ink));
                ui.separator();
                let mut picked_id: Option<InstanceId> = None;
                let footer_h = solidcraft_ui_kit::catalog_footer_height(ui) + t.space_8;
                let scroll_h = (ui.available_height() - footer_h).max(60.0);
                egui::ScrollArea::vertical()
                    .max_height(scroll_h)
                    .show(ui, |ui| {
                        if self.job.parts.is_empty() {
                            status_line(ui, "No parts loaded yet. Import an STL/3MF or add a Sample Part.", StatusTone::Dim);
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
                                for inst in self.job.instances.iter().filter(|i| i.part_id == proto.id) {
                                    let is_sel = self.selected == Some(inst.id);
                                    let in_bounds = self.job.is_instance_in_bounds(inst.id);
                                    let pos = inst.translation();
                                    let mut tag = String::new();
                                    if inst.locked {
                                        tag.push_str(" [LOCK]");
                                    }
                                    if !in_bounds {
                                        tag.push_str(" [OOB]");
                                    }
                                    let label = format!(
                                        "#{:<3} ({:.1}, {:.1}, {:.1}){tag}",
                                        inst.id.0, pos[0], pos[1], pos[2]
                                    );
                                    let rich = if !in_bounds {
                                        egui::RichText::new(label).color(t.form_error)
                                    } else {
                                        egui::RichText::new(label)
                                    };
                                    if ui.selectable_label(is_sel, rich).clicked() {
                                        picked_id = Some(inst.id);
                                    }
                                }
                            });
                        }
                    });
                if let Some(id) = picked_id {
                    self.select_instance(Some(id));
                }
                catalog_footer_with_tip(
                    ui,
                    &format!("{} instance(s) in chamber", self.job.instances.len()),
                    "Click an instance row or mesh in the 3D viewport to inspect or drag (Shift+drag for Z).",
                );
            });
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

                    section(ui, "chamber_sec", "Build Chamber", true, |ui| {
                        value_row(ui, "Machine", self.job.machine.name());
                        value_row(ui, "Envelope", &format!("{wx:.0} × {wy:.0} × {wz:.0} mm"));
                        row(ui, "Wall Margin", |ui| {
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.job.wall_margin_mm)
                                        .range(0.0..=25.0)
                                        .speed(0.5)
                                        .suffix(" mm"),
                                )
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
                            );
                        });
                        value_row(ui, "Build Height", &format!("{max_z:.2} mm ({layers} layers)"));
                        value_row(ui, "Density", &format!("{:.2}%", self.job.packing_density_pct()));
                    });

                    ui.add_space(t.space_6);
                    section(ui, "inst_sec", "Selected Instance", true, |ui| {
                        let Some(sel_id) = self.selected else {
                            status_line(ui, "Select a part in the 3D view or Parts Browser.", StatusTone::Dim);
                            return;
                        };

                        let in_bounds = self.job.is_instance_in_bounds(sel_id);
                        let current_euler = self
                            .job
                            .instances
                            .iter()
                            .find(|i| i.id == sel_id)
                            .map(|i| i.euler_deg());
                        let part_area = self
                            .job
                            .instances
                            .iter()
                            .find(|i| i.id == sel_id)
                            .and_then(|i| self.job.part(i.part_id))
                            .map(|p| p.mesh.surface_area_mm2);

                        if let Some(area) = part_area {
                            value_row(ui, "Surface Area", &format!("{:.2} cm²", area / 100.0));
                        }

                        let mut drop_floor = false;
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
                                if in_bounds {
                                    "Within chamber & wall margin"
                                } else {
                                    "Exceeds chamber or wall margin!"
                                },
                                if in_bounds { StatusTone::Dim } else { StatusTone::Error },
                            );

                            if changed {
                                self.mark_dirty();
                            }
                        }

                        if drop_floor && self.job.drop_instance_to_floor(sel_id) {
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

            if resp.dragged_by(egui::PointerButton::Secondary) || resp.dragged_by(egui::PointerButton::Middle) {
                let delta = resp.drag_delta();
                if ui.input(|i| i.modifiers.shift) {
                    self.camera.pan(f64::from(delta.x), f64::from(delta.y), f64::from(rect.height().max(1.0)));
                } else {
                    self.camera.orbit(f64::from(-delta.x) * 0.006, f64::from(delta.y) * 0.006);
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
                && self.compass_drag.at.is_none()
                && resp.drag_started_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                if let Some(iid) = hit {
                    self.select_instance(Some(iid));
                    if let Some(inst) = self.job.instances.iter().find(|i| i.id == iid)
                        && !inst.locked
                    {
                        let tr = inst.translation();
                        let plane_z = tr[2];
                        if let Some(hit_pt) = viewport::intersect_horizontal_plane(&self.camera, rect, pos, plane_z) {
                            self.drag_state = Some(DragState {
                                instance_id: iid,
                                start_translation: tr,
                                start_hit: hit_pt,
                                plane_z,
                            });
                        }
                    }
                }
            }

            if resp.dragged_by(egui::PointerButton::Primary)
                && let (Some(ds), Some(pos)) = (self.drag_state, resp.interact_pointer_pos())
            {
                let shift = ui.input(|i| i.modifiers.shift);
                if shift {
                    if let Some(z_now) = viewport::intersect_vertical_plane(&self.camera, rect, pos, ds.start_hit)
                        && let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == ds.instance_id)
                    {
                        let mut tr = inst.translation();
                        tr[2] = ds.start_translation[2] + (z_now - ds.plane_z);
                        inst.set_translation(tr);
                        self.mark_dirty();
                    }
                } else if let Some(hit_now) = viewport::intersect_horizontal_plane(&self.camera, rect, pos, ds.plane_z)
                    && let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == ds.instance_id)
                {
                    let mut tr = inst.translation();
                    tr[0] = ds.start_translation[0] + (hit_now[0] - ds.start_hit[0]);
                    tr[1] = ds.start_translation[1] + (hit_now[1] - ds.start_hit[1]);
                    inst.set_translation(tr);
                    self.mark_dirty();
                }
            }

            if resp.drag_stopped() {
                self.drag_state = None;
            }

            if !over_compass
                && self.compass_drag.at.is_none()
                && resp.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                self.select_instance(hit);
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
                clip: [0.0; 4],
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
                ssaa: 1.0,
                hover_tint: [0.08, 0.08, 0.08],
                selection_tint: [0.93, 0.70, 0.22],
                hatch: [[0.5, 0.5, 0.5]; solidcraft_viewport::hatch::HATCH_COLOURS],
                sky: [0.0, 0.0, 1.0, 1.0],
                moving: None,
            });
            painter.add(egui_wgpu::Callback::new_paint_callback(rect, cb));

            if rect.width() >= 140.0 && rect.height() >= 140.0 {
                let nav = view_compass::orbit(ui, &mut self.compass_drag, ui.rect_contains_pointer(rect), |p| {
                    p.distance(compass_centre) <= 49.5
                });
                if let Some(step) = nav.step {
                    self.camera.orbit(-step.dx * 0.008, step.dy * 0.008);
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
                        self.camera = viewport::default_camera(&self.job.machine);
                    }
                    None => {}
                }
            }
        });
    }
}

impl eframe::App for SlsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        solidcraft_theme::apply(&ctx);
        let t = Tokens::get();

        self.draw_top_bar(ui, &ctx, &t);
        self.draw_bottom_bar(ui, &ctx, &t);
        self.draw_parts_browser(ui, &ctx, &t);
        self.draw_inspector(ui, &ctx, &t);
        self.draw_viewport(ui, &ctx, &t);
    }
}

fn create_sample_bracket_mesh(variant: u32) -> solidcraft_sls_core::Result<TriMesh> {
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
        let mut app = SlsApp::new();
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert!(app.scene_slot.lock().is_ok_and(|g| g.is_some()));
    }

    #[test]
    fn sample_part_and_90_deg_rotation_update_scene() {
        let mut app = SlsApp::new();
        app.add_sample_part();
        assert_eq!(app.job.instances.len(), 1);
        let iid = app.selected.expect("selected");
        app.rotate_selected_90(0);
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert!(app.job.drop_instance_to_floor(iid));
        assert!(app.job.is_instance_in_bounds(iid));
    }
}
