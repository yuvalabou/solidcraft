//! Right sidebar: Build Summary, Build Chamber, and No-Build Zone.

use crate::app::*;

use nestcraft_core::model::BedOrigin;
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Edge, Surface};
use solidcraft_ui_kit::{
    Button, Segments, StatusTone, button, checkbox_row, row, scope, section, segmented, status_line, value_row,
};


impl NestCraftApp {
    pub(crate) fn draw_inspector(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
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
                                self.camera = self.default_camera_for_job();
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
                        row(ui, "Material", |ui| {
                            let cur = nestcraft_core::report::material_by_id(&self.job.material_id);
                            egui::ComboBox::from_id_salt("sls_material_combo")
                                .selected_text(cur.name)
                                .width(150.0)
                                .show_ui(ui, |ui| {
                                    for mat in nestcraft_core::report::SLS_MATERIALS {
                                        if ui
                                            .selectable_label(self.job.material_id == mat.id, mat.name)
                                            .clicked()
                                        {
                                            self.job.material_id = mat.id.to_owned();
                                            if self.report_dialog_open {
                                                self.report_preview =
                                                    Some(nestcraft_core::report::BuildReport::generate(&self.job));
                                            }
                                            self.status = format!("Material: {}", mat.name);
                                        }
                                    }
                                });
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

}
