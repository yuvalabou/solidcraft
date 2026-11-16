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
    BuildJob, InstanceId, MachineProfile, PartId, RotationConstraint,
    transform_from_translation_euler_deg,
};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Edge, Surface};
use solidcraft_ui_kit::icon_label_button;
use solidcraft_viewport::Camera;
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
            status: "Ready — import an STL or 3MF to begin.".to_owned(),
            euler_edit: HashMap::new(),
            dup_count: 1,
            drag_state: None,
        }
    }

    fn select_instance(&mut self, id: Option<InstanceId>) {
        if self.selected != id {
            self.selected = id;
            if let Some(iid) = id
                && let Some(inst) = self.job.instances.iter().find(|i| i.id == iid) {
                    self.euler_edit.insert(iid, inst.euler_deg());
                }
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
                            self.selected = Some(iid);
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
                        self.selected = Some(iid);
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
}

impl eframe::App for SlsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        solidcraft_theme::apply(&ctx);
        let t = Tokens::get();
        let top_c = Container::new(&ctx, Surface::Chrome);
        let side_c = Container::new(&ctx, Surface::Panel);

        let top_panel = top_c.edge("sls_top_bar", Edge::Top);
        top_c.show_edge(ui, top_panel, top_c.edge_frame(t.panel), |ui| {
            ui.horizontal(|ui| {
                ui.strong("SolidCraft SLS");
                ui.separator();

                let machine_name = self.job.machine.name().to_owned();
                if ui
                    .selectable_label(matches!(self.job.machine, MachineProfile::FormigaP110), "Formiga P110")
                    .clicked()
                    && machine_name != MachineProfile::FormigaP110.name()
                {
                    self.job.machine = MachineProfile::FormigaP110;
                    self.camera = viewport::default_camera(&self.job.machine);
                    self.mark_dirty();
                }
                if ui
                    .selectable_label(matches!(self.job.machine, MachineProfile::FarsoonHT403P), "Farsoon HT403P")
                    .clicked()
                    && machine_name != MachineProfile::FarsoonHT403P.name()
                {
                    self.job.machine = MachineProfile::FarsoonHT403P;
                    self.camera = viewport::default_camera(&self.job.machine);
                    self.mark_dirty();
                }

                ui.separator();
                if icon_label_button(ui, "Import", "sls_import_mesh", true, false)
                    .backing_on_hover_text("Import STL or 3MF mesh into the build chamber")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("3D Mesh", &["stl", "STL", "3mf", "3MF"])
                        .pick_file()
                    {
                        self.import_file(&path);
                    }
                if icon_label_button(ui, "Export 3MF", "export", !self.job.instances.is_empty(), false)
                    .backing_on_hover_text("Export build job to 3MF")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("3MF", &["3mf"])
                        .set_file_name("build.3mf")
                        .save_file()
                    {
                        self.export_threemf_file(&path);
                    }
                ui.separator();
                ui.add(egui::DragValue::new(&mut self.dup_count).range(1..=200).prefix("×"));
                if icon_label_button(ui, "Duplicate", "sls_duplicate", self.selected.is_some(), false)
                    .backing_on_hover_text("Duplicate selected instance N times")
                    .clicked()
                    && let Some(sel) = self.selected {
                        let count = self.dup_count.max(1);
                        if let Ok(ids) = self.job.duplicate_instance(sel, count) {
                            let last = ids.last().copied();
                            self.select_instance(last);
                            self.status = format!("Duplicated {count} instance(s)");
                        }
                    }
                if icon_label_button(ui, "Delete", "delete", self.selected.is_some(), true)
                    .backing_on_hover_text("Remove selected instance from build")
                    .clicked()
                    && let Some(sel) = self.selected
                        && self.job.remove_instance(sel) {
                            self.euler_edit.remove(&sel);
                            self.selected = None;
                            self.mark_dirty();
                            self.status = "Removed selected instance".to_owned();
                        }
                if icon_label_button(ui, "Reset View", "fit", true, false)
                    .backing_on_hover_text("Reset camera to default isometric build view")
                    .clicked()
                {
                    self.camera = viewport::default_camera(&self.job.machine);
                }
                icon_label_button(ui, "Auto-Nest", "sls_auto_nest", false, false)
                    .backing_on_hover_text("3D EDT Auto-Nest (coming in Stage 2)");
            });
        });

        let left_panel = side_c.edge("sls_parts_tree", Edge::Left).default_size(250.0);
        side_c.show_edge(ui, left_panel, side_c.edge_frame(t.panel), |ui| {
            ui.heading("Build Parts");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut picked_id: Option<InstanceId> = None;
                for proto in &self.job.parts {
                    let count = self.job.instance_count_for_part(proto.id);
                    let [dx, dy, dz] = proto.mesh.dimensions();
                    let tris = proto.mesh.indices.len();
                    let header = format!("{} (×{count})", proto.name);
                    egui::CollapsingHeader::new(header)
                        .id_salt(("proto", proto.id.0))
                        .default_open(true)
                        .show(ui, |ui| {
                            ui.small(format!("{tris} tris · {dx:.1} × {dy:.1} × {dz:.1} mm"));
                            for inst in self.job.instances.iter().filter(|i| i.part_id == proto.id) {
                                let is_sel = self.selected == Some(inst.id);
                                let pos = inst.translation();
                                let label = format!("#{}  ({:.1}, {:.1}, {:.1})", inst.id.0, pos[0], pos[1], pos[2]);
                                if ui.selectable_label(is_sel, label).clicked() {
                                    picked_id = Some(inst.id);
                                }
                            }
                        });
                }
                if let Some(id) = picked_id {
                    self.select_instance(Some(id));
                }
            });
        });

        let right_panel = side_c.edge("sls_inspector", Edge::Right).default_size(270.0);
        side_c.show_edge(ui, right_panel, side_c.edge_frame(t.panel), |ui| {
            ui.heading("Build Metrics");
            ui.separator();
            let [wx, wy, wz] = self.job.machine.size_mm();
            ui.label(format!("Machine: {}", self.job.machine.name()));
            ui.label(format!("Envelope: {wx:.0} × {wy:.0} × {wz:.0} mm"));
            ui.label(format!("Parts / Instances: {} / {}", self.job.parts.len(), self.job.instances.len()));
            ui.label(format!("Part Volume: {:.2} cm³", self.job.total_part_volume_mm3() / 1000.0));
            ui.label(format!("Max Z: {:.2} mm", self.job.max_z_mm()));
            ui.label(format!("Density: {:.2}%", self.job.packing_density_pct()));
            ui.horizontal(|ui| {
                ui.label("Wall Margin:");
                if ui
                    .add(egui::DragValue::new(&mut self.job.wall_margin_mm).range(0.0..=25.0).speed(0.5).suffix(" mm"))
                    .changed()
                {
                    self.mark_dirty();
                }
            });

            ui.add_space(12.0);
            ui.heading("Selected Instance");
            ui.separator();
            if let Some(sel_id) = self.selected {
                let in_bounds = self.job.is_instance_in_bounds(sel_id);
                let mut current_euler = None;
                let mut part_area = None;
                if let Some(inst) = self.job.instances.iter().find(|i| i.id == sel_id) {
                    current_euler = Some(inst.euler_deg());
                    part_area = self.job.part(inst.part_id).map(|p| p.mesh.surface_area_mm2);
                }
                if let Some(area) = part_area {
                    ui.small(format!("Surface Area: {:.2} cm²", area / 100.0));
                }
                if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == sel_id) {
                    let mut pos = inst.translation();
                    let mut changed = false;
                    ui.horizontal(|ui| {
                        ui.label("X");
                        changed |= ui.add(egui::DragValue::new(&mut pos[0]).speed(0.5).suffix(" mm")).changed();
                        ui.label("Y");
                        changed |= ui.add(egui::DragValue::new(&mut pos[1]).speed(0.5).suffix(" mm")).changed();
                        ui.label("Z");
                        changed |= ui.add(egui::DragValue::new(&mut pos[2]).speed(0.5).suffix(" mm")).changed();
                    });

                    let euler = self
                        .euler_edit
                        .entry(sel_id)
                        .or_insert_with(|| current_euler.unwrap_or([0.0, 0.0, 0.0]));
                    let mut rot_changed = false;
                    ui.horizontal(|ui| {
                        ui.label("Rx");
                        rot_changed |= ui.add(egui::DragValue::new(&mut euler[0]).speed(1.0).suffix("°")).changed();
                        ui.label("Ry");
                        rot_changed |= ui.add(egui::DragValue::new(&mut euler[1]).speed(1.0).suffix("°")).changed();
                        ui.label("Rz");
                        rot_changed |= ui.add(egui::DragValue::new(&mut euler[2]).speed(1.0).suffix("°")).changed();
                    });

                    if rot_changed {
                        inst.transform = transform_from_translation_euler_deg(pos, *euler);
                        changed = true;
                    } else if changed {
                        inst.set_translation(pos);
                    }

                    ui.horizontal(|ui| {
                        ui.label("Orient:");
                        ui.selectable_value(&mut inst.constraint, RotationConstraint::Free, "Free");
                        ui.selectable_value(&mut inst.constraint, RotationConstraint::ZOnly, "ZOnly");
                        ui.selectable_value(&mut inst.constraint, RotationConstraint::Fixed, "Fixed");
                    });
                    ui.checkbox(&mut inst.locked, "Lock placement");
                    ui.label(if in_bounds { "Bounds: OK" } else { "Bounds: OUT OF ENVELOPE" });

                    if changed {
                        self.mark_dirty();
                    }
                }
            } else {
                ui.label("Click a part in the viewport or list.");
            }

            ui.add_space(12.0);
            ui.separator();
            ui.small(&self.status);
        });

        let central = Container::new(&ctx, Surface::Viewport).central();
        central.show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());

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

            if resp.drag_started_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos() {
                    let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                    if let Some(iid) = hit {
                        self.select_instance(Some(iid));
                        if let Some(inst) = self.job.instances.iter().find(|i| i.id == iid)
                            && !inst.locked {
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
                && let (Some(ds), Some(pos)) = (self.drag_state, resp.interact_pointer_pos()) {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if shift {
                        if let Some(z_now) = viewport::intersect_vertical_plane(&self.camera, rect, pos, ds.start_hit)
                            && let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == ds.instance_id) {
                                let mut tr = inst.translation();
                                tr[2] = ds.start_translation[2] + (z_now - ds.plane_z);
                                inst.set_translation(tr);
                                self.mark_dirty();
                            }
                    } else if let Some(hit_now) = viewport::intersect_horizontal_plane(&self.camera, rect, pos, ds.plane_z)
                        && let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == ds.instance_id) {
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

            if resp.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos() {
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
        });
    }
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
}
