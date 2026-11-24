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

    pub(crate) fn draw_slice_distribution_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        if !self.slice_dist_open {
            return;
        }
        if self.slice_dist_cache.is_none() {
            self.slice_dist_cache = Some(nestcraft_core::slice::SliceDistribution::compute(
                &self.job,
                self.slice_dist_step_mm,
            ));
        }

        let side_c = Container::new(ctx, Surface::Panel);
        let panel = side_c
            .edge("sls_slice_dist_panel", Edge::Right)
            .default_size(220.0)
            .min_size(180.0)
            .max_size(560.0)
            .resizable(true);
        let frame = side_c
            .edge_frame(t.panel)
            .inner_margin(egui::Margin::symmetric(t.space_10 as i8, t.space_10 as i8));

        let mut close_now = false;
        let mut recompute = false;
        side_c.show_edge(ui, panel, frame, |ui| {
            scope(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(t.space_6, t.space_6);

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Slice Distribution").strong().color(t.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, "×", Button::Small)
                            .backing_on_hover_text("Collapse Slice Distribution sidebar")
                            .clicked()
                        {
                            close_now = true;
                        }
                    });
                });

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Step").small().color(t.text_dim));
                    egui::ComboBox::from_id_salt("sls_slice_step_combo")
                        .selected_text(format!("{:.1} mm", self.slice_dist_step_mm))
                        .width(72.0)
                        .show_ui(ui, |ui| {
                            for step in [0.1_f32, 0.5, 1.0, 2.0, 5.0] {
                                if ui
                                    .selectable_label(
                                        (self.slice_dist_step_mm - step).abs() < 0.01,
                                        format!("{step:.1} mm"),
                                    )
                                    .clicked()
                                {
                                    self.slice_dist_step_mm = step;
                                    recompute = true;
                                }
                            }
                        });
                    if button(ui, "Refresh", Button::Small).clicked() {
                        recompute = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Scale").small().color(t.text_dim));
                    let mut scale_idx = usize::from(self.slice_dist_bed_scale);
                    if ui
                        .push_id("sls_slice_scale_seg", |ui| {
                            segmented(
                                ui,
                                &mut scale_idx,
                                &["Peak", "Bed %"],
                                Segments {
                                    dense: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .inner
                    {
                        self.slice_dist_bed_scale = scale_idx == 1;
                    }
                });

                if let Some(dist) = &self.slice_dist_cache {
                    if dist.excluded_oob_count > 0 {
                        status_line(
                            ui,
                            &format!("{} out-of-bounds part(s) excluded", dist.excluded_oob_count),
                            StatusTone::Warning,
                        );
                    }
                    value_row(
                        ui,
                        "Peak Area",
                        &format!("{:.1} mm² @ Z={:.1}", dist.peak_area_mm2, dist.peak_z_mm),
                    );
                    value_row(ui, "Mean Area", &format!("{:.1} mm²", dist.mean_area_mm2));
                    value_row(
                        ui,
                        "Z Span",
                        &format!("{:.1}..{:.1} mm ({} slices)", dist.z_min_mm, dist.z_max_mm, dist.layers.len()),
                    );

                    ui.separator();
                    if dist.layers.is_empty() {
                        status_line(ui, "No parts inside machine to slice.", StatusTone::Dim);
                    } else {
                        let avail = ui.available_size();
                        let chart_h = (avail.y - 36.0).max(160.0);
                        let chart_w = ui.available_width().max(120.0);
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(chart_w, chart_h),
                            egui::Sense::hover(),
                        );
                        let p = ui.painter_at(rect);
                        p.rect_filled(rect, 4.0, t.panel.gamma_multiply(0.6));
                        p.rect_stroke(
                            rect,
                            4.0,
                            egui::Stroke::new(1.0, t.border),
                            egui::StrokeKind::Inside,
                        );

                        let pad = 6.0_f32;
                        let inner = rect.shrink(pad);
                        for tick in [0.25_f32, 0.5, 0.75, 1.0] {
                            let gx = inner.left() + inner.width() * tick;
                            p.line_segment(
                                [egui::pos2(gx, inner.top()), egui::pos2(gx, inner.bottom())],
                                egui::Stroke::new(1.0, t.border.gamma_multiply(0.45)),
                            );
                        }
                        let n = dist.layers.len().max(1);
                        let row_h = (inner.height() / n as f32).max(1.0);
                        let max_ref = if self.slice_dist_bed_scale {
                            dist.bed_area_mm2.max(1e-6)
                        } else {
                            dist.peak_area_mm2.max(1e-6)
                        };

                        let hovered_idx = resp.hover_pos().and_then(|hp| {
                            if !inner.contains(hp) {
                                return None;
                            }
                            let frac_from_top = ((hp.y - inner.top()) / inner.height()).clamp(0.0, 0.9999);
                            let rev = (frac_from_top * n as f32).floor() as usize;
                            Some(n.saturating_sub(1).saturating_sub(rev))
                        });

                        for (idx, layer) in dist.layers.iter().enumerate() {
                            // Draw bottom (z_min) at bottom of chart, top (z_max) at top.
                            let rev = n - 1 - idx;
                            let y0 = inner.top() + rev as f32 * row_h;
                            let y1 = (y0 + row_h - 0.5).max(y0 + 1.0);
                            let frac = (layer.area_mm2 / max_ref).clamp(0.0, 1.0) as f32;
                            let bar_w = (inner.width() * frac).max(if layer.area_mm2 > 0.0 { 2.0 } else { 0.0 });
                            let bar_rect = egui::Rect::from_min_max(
                                egui::pos2(inner.left(), y0),
                                egui::pos2(inner.left() + bar_w, y1),
                            );
                            let is_hov = hovered_idx == Some(idx);
                            let col = if is_hov { t.accent } else { t.accent.gamma_multiply(0.75) };
                            p.rect_filled(bar_rect, 1.0, col);
                        }

                        if let Some(idx) = hovered_idx
                            && let Some(l) = dist.layers.get(idx)
                        {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Z={:.1} mm | {:.1} mm² ({:.1}%) | {} part(s)",
                                    l.z_mm, l.area_mm2, l.fill_pct, l.part_count
                                ))
                                .small()
                                .strong()
                                .color(t.accent),
                            );
                        } else {
                            status_line(ui, "Hover chart to inspect Z layer; drag left edge to widen.", StatusTone::Dim);
                        }
                    }
                }
            });
        });

        if close_now {
            self.slice_dist_open = false;
        } else if recompute {
            self.slice_dist_cache = Some(nestcraft_core::slice::SliceDistribution::compute(
                &self.job,
                self.slice_dist_step_mm,
            ));
        }
    }

    pub(crate) fn draw_thermal_map_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        if !self.thermal_map_open {
            return;
        }
        if self.thermal_report.is_none() {
            self.refresh_thermal_map();
        }

        let side_c = Container::new(ctx, Surface::Panel);
        let panel = side_c
            .edge("sls_thermal_map_panel", Edge::Right)
            .default_size(230.0)
            .min_size(190.0)
            .max_size(520.0)
            .resizable(true);
        let frame = side_c
            .edge_frame(t.panel)
            .inner_margin(egui::Margin::symmetric(t.space_10 as i8, t.space_10 as i8));

        let mut close_now = false;
        let mut recompute = false;
        let mut select_part: Option<nestcraft_core::model::InstanceId> = None;

        side_c.show_edge(ui, panel, frame, |ui| {
            scope(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(t.space_6, t.space_6);

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Thermal Map").strong().color(t.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, "×", Button::Small)
                            .backing_on_hover_text("Close Thermal Map")
                            .clicked()
                        {
                            close_now = true;
                        }
                        if button(ui, "Refresh", Button::Small).clicked() {
                            recompute = true;
                        }
                    });
                });

                if let Some(rep) = &self.thermal_report {
                    if rep.excluded_oob_count > 0 {
                        status_line(
                            ui,
                            &format!("{} out-of-bounds part(s) excluded", rep.excluded_oob_count),
                            StatusTone::Warning,
                        );
                    }
                    value_row(ui, "Kernel", &format!("{:.0}×{:.0}×{:.0} mm ({:.1} mm pitch)", rep.kernel_mm, rep.kernel_mm, rep.kernel_mm, rep.pitch_mm));
                    value_row(ui, "Peak Density", &format!("{:.1}%", rep.peak_density_pct));
                    value_row(
                        ui,
                        "Hotspot XYZ",
                        &format!(
                            "({:.0}, {:.0}, {:.0}) mm",
                            rep.hotspot_xyz[0], rep.hotspot_xyz[1], rep.hotspot_xyz[2]
                        ),
                    );
                    if rep.hotspot_part_count > 0 {
                        status_line(
                            ui,
                            &format!("{} part(s) >= 30% local density (curl / cake risk)", rep.hotspot_part_count),
                            StatusTone::Warning,
                        );
                    }

                    // Compact 4-stop color legend
                    ui.horizontal(|ui| {
                        for (label, pct) in [("Cool", 5.0_f32), ("15%", 15.0), ("25%", 25.0), ("35%+", 35.0)] {
                            let [r, g, b, _] = nestcraft_core::thermal::thermal_rgba(pct);
                            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                            ui.painter().rect_filled(dot, 2.0, egui::Color32::from_rgb(r, g, b));
                            ui.label(egui::RichText::new(label).small().color(t.text_dim));
                        }
                    });

                    ui.separator();
                    if rep.parts.is_empty() {
                        status_line(ui, "No parts inside machine to analyze.", StatusTone::Dim);
                    } else {
                        egui::ScrollArea::vertical()
                            .id_salt("sls_thermal_parts_scroll")
                            .show(ui, |ui| {
                                for p in &rep.parts {
                                    let [r, g, b, _] = nestcraft_core::thermal::thermal_rgba(p.peak_density_pct);
                                    let resp = ui.horizontal(|ui| {
                                        let (dot, _) =
                                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                                        ui.painter().rect_filled(dot, 2.0, egui::Color32::from_rgb(r, g, b));
                                        let is_sel = self.selected_set.contains(&p.instance_id);
                                        let lbl = format!("{} — {:.1}% (avg {:.1}%)", p.name, p.peak_density_pct, p.mean_density_pct);
                                        ui.selectable_label(is_sel, lbl)
                                    });
                                    if resp.inner.clicked() {
                                        select_part = Some(p.instance_id);
                                    }
                                }
                            });
                    }
                }
            });
        });

        if close_now {
            self.toggle_thermal_map();
        } else if recompute {
            self.refresh_thermal_map();
            self.scene_dirty = true;
        } else if let Some(iid) = select_part {
            self.selected_set.clear();
            self.selected_set.insert(iid);
            self.selected = Some(iid);
            self.selection_anchor = Some(iid);
            self.scene_dirty = true;
        }
    }
}
