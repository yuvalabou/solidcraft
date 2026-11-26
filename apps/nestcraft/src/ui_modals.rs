//! Draggable modal dialogs and drag-and-drop overlay.

use crate::app::*;

use nestcraft_core::model::{
    BedOrigin, InstanceId, MachineProfile,
};
use nestcraft_core::sinterbox::{self};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Surface};
use solidcraft_ui_kit::{
    Button, Segments, StatusTone, button, checkbox_row, row, segmented, status_line,
};


impl NestCraftApp {
    pub(crate) fn draw_color_picker_modal(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn draw_drop_overlay(&self, ctx: &egui::Context, rect: egui::Rect, t: &Tokens) {
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


    pub(crate) fn draw_custom_platform_modal(&mut self, ctx: &egui::Context) {
        if !self.custom_platform_open {
            return;
        }
        let mut open = self.custom_platform_open;
        let mut apply = false;
        let mut reset_default = false;
        let mut cancel = false;
        let slot = self.active_machine_slot.min(MACHINE_SLOT_COUNT - 1);
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
                row(ui, "Default Material", |ui| {
                    let cur = nestcraft_core::report::material_by_id(&self.custom_material_id);
                    egui::ComboBox::from_id_salt("cfg_material_combo")
                        .selected_text(cur.name)
                        .width(180.0)
                        .show_ui(ui, |ui| {
                            for mat in nestcraft_core::report::SLS_MATERIALS {
                                if ui
                                    .selectable_label(self.custom_material_id == mat.id, mat.name)
                                    .clicked()
                                {
                                    self.custom_material_id = mat.id.to_owned();
                                }
                            }
                        });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if button(ui, "Apply", Button::Primary).clicked() {
                        apply = true;
                    }
                    if slot < CUSTOM_MACHINE_SLOT && button(ui, "Reset Default", Button::Small).clicked() {
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
            self.custom_material_id = d.material_id.clone();
        } else if apply {
            let clean_machine = MachineProfile::custom(&self.custom_name, self.custom_size_mm);
            self.machine_presets[slot] = MachinePresetConfig {
                name: clean_machine.name().to_owned(),
                size_mm: clean_machine.size_mm(),
                wall_margin_mm: self.custom_wall_margin,
                min_z_mm: self.custom_min_z,
                clearance_mm: self.custom_clearance,
                bed_origin: self.custom_bed_origin.into(),
                material_id: self.custom_material_id.clone(),
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

    pub(crate) fn draw_duplicate_modal(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn draw_move_modal(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn draw_scale_modal(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn draw_sinterbox_modal(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn draw_export_stl_modal(&mut self, ctx: &egui::Context) {
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


    pub(crate) fn draw_below_zero_import_modal(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn draw_load_progress_modal(&self, ctx: &egui::Context) {
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

    pub(crate) fn draw_save_progress_modal(&self, ctx: &egui::Context, t: &Tokens) {
        if let Some(task) = &self.save_task {
            let permille = task
                .progress_permille
                .load(std::sync::atomic::Ordering::Relaxed)
                .min(1000);
            let frac = (permille as f32 / 1000.0).clamp(0.0, 1.0);
            let stage = task
                .stage
                .lock()
                .ok()
                .map(|s| s.clone())
                .unwrap_or_else(|| "Saving...".to_owned());
            egui::Window::new("Saving Job")
                .collapsible(false)
                .resizable(false)
                .title_bar(true)
                .movable(true)
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(ctx.content_rect().center())
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.label(&stage);
                    ui.add_space(6.0);
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .show_percentage()
                            .animate(true)
                            .desired_width(320.0),
                    );
                    ui.add_space(4.0);
                    status_line(ui, &task.file_name, StatusTone::Dim);
                });
        } else if let Some((msg, is_error, at)) = &self.save_hint {
            let ttl = std::time::Duration::from_millis(2500);
            let elapsed = at.elapsed();
            if elapsed < ttl {
                ctx.request_repaint_after(ttl - elapsed);
                let stroke_color = if *is_error { t.form_error } else { t.accent };
                egui::Area::new(egui::Id::unique("sls_save_hint"))
                    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -40.0))
                    .interactable(false)
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| {
                        egui::Frame::NONE
                            .fill(t.panel)
                            .stroke(egui::Stroke::new(1.0, stroke_color))
                            .corner_radius(t.radius_md)
                            .inner_margin(egui::Margin::symmetric(14, 8))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new(msg).color(t.text).strong());
                            });
                    });
            }
        }
    }

    pub(crate) fn draw_keep_delete_modal(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn draw_report_modal(&mut self, ctx: &egui::Context) {
        if !self.report_dialog_open {
            return;
        }
        if self.report_preview.is_none() {
            self.report_preview = Some(nestcraft_core::report::BuildReport::generate(&self.job));
        }
        let mut open = self.report_dialog_open;
        let mut refresh = false;
        let mut open_html_now = false;
        let mut export_html = false;
        let mut export_csv = false;
        let mut close_now = false;
        let t = Tokens::get();

        if let Some(rep) = &self.report_preview {
            egui::Window::new("Build Report Generator (Mockup)")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .movable(true)
                .default_width(640.0)
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(ctx.content_rect().center())
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Machine:").strong());
                        ui.label(format!(
                            "{} ({:.0}×{:.0}×{:.0} mm)",
                            rep.machine_name,
                            rep.machine_size_mm[0],
                            rep.machine_size_mm[1],
                            rep.machine_size_mm[2],
                        ));
                        ui.separator();
                        ui.label(egui::RichText::new("Material:").strong());
                        ui.label(rep.material.name);
                    });

                    if rep.excluded_oob_count > 0 || rep.hidden_excluded_count > 0 {
                        status_line(
                            ui,
                            &format!(
                                "Excluded {} out-of-bounds part(s) outside machine & {} hidden part(s).",
                                rep.excluded_oob_count, rep.hidden_excluded_count
                            ),
                            StatusTone::Warning,
                        );
                    }

                    ui.add_space(6.0);
                    egui::Grid::new("report_kpi_grid")
                        .num_columns(4)
                        .spacing([14.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Parts Inside Machine").small().color(t.text_dim));
                            ui.label(egui::RichText::new("Total Part Volume").small().color(t.text_dim));
                            ui.label(egui::RichText::new("Build Height (+Pillow)").small().color(t.text_dim));
                            ui.label(egui::RichText::new("Packing Density").small().color(t.text_dim));
                            ui.end_row();

                            ui.label(egui::RichText::new(format!("{}", rep.included_parts.len())).strong());
                            ui.label(egui::RichText::new(format!("{:.2} cm³", rep.total_part_volume_cm3)).strong());
                            ui.label(
                                egui::RichText::new(format!(
                                    "{:.1} mm (+{:.0} mm)",
                                    rep.total_build_height_mm, rep.spec.pillow_z_mm
                                ))
                                .strong(),
                            );
                            ui.label(egui::RichText::new(format!("{:.2}%", rep.packing_density_pct)).strong());
                            ui.end_row();

                            ui.label(egui::RichText::new("Fresh Powder").small().color(t.text_dim));
                            ui.label(egui::RichText::new("Est. Build Time").small().color(t.text_dim));
                            ui.label(egui::RichText::new("Material + Machine").small().color(t.text_dim));
                            ui.label(egui::RichText::new(format!("Quoted ({:.2}×)", rep.spec.margin)).small().color(t.text_dim));
                            ui.end_row();

                            ui.label(egui::RichText::new(format!("{:.2} kg", rep.fresh_powder_kg)).strong());
                            ui.label(egui::RichText::new(format!("{:.1} h", rep.est_build_hours)).strong());
                            ui.label(
                                egui::RichText::new(format!("${:.2} + ${:.2}", rep.material_cost, rep.machine_cost))
                                    .strong(),
                            );
                            ui.label(
                                egui::RichText::new(format!("${:.2}", rep.quoted_price))
                                    .strong()
                                    .color(t.accent),
                            );
                            ui.end_row();
                        });

                    ui.add_space(8.0);
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("report_parts_table_scroll")
                        .max_height(220.0)
                        .show(ui, |ui| {
                            egui::Grid::new("report_parts_table")
                                .striped(true)
                                .num_columns(6)
                                .spacing([12.0, 4.0])
                                .show(ui, |ui| {
                                    ui.label(egui::RichText::new("ID").strong());
                                    ui.label(egui::RichText::new("Part").strong());
                                    ui.label(egui::RichText::new("Size (mm)").strong());
                                    ui.label(egui::RichText::new("Vol (cm³)").strong());
                                    ui.label(egui::RichText::new("Area (cm²)").strong());
                                    ui.label(egui::RichText::new("Mesh").strong());
                                    ui.end_row();

                                    for p in &rep.included_parts {
                                        ui.label(format!("#{}", p.instance_id));
                                        ui.label(&p.name);
                                        ui.label(format!("{:.1}×{:.1}×{:.1}", p.size_mm[0], p.size_mm[1], p.size_mm[2]));
                                        ui.label(format!("{:.2}", p.volume_cm3));
                                        ui.label(format!("{:.2}", p.area_cm2));
                                        if p.is_manifold {
                                            ui.label("OK");
                                        } else {
                                            ui.label(egui::RichText::new("Error").color(t.error));
                                        }
                                        ui.end_row();
                                    }
                                });
                        });

                    ui.separator();
                    ui.horizontal(|ui| {
                        if button(ui, "Open HTML Report", Button::Primary).clicked() {
                            open_html_now = true;
                        }
                        if button(ui, "Export HTML...", Button::Small).clicked() {
                            export_html = true;
                        }
                        if button(ui, "Export CSV...", Button::Small).clicked() {
                            export_csv = true;
                        }
                        if button(ui, "Refresh", Button::Small).clicked() {
                            refresh = true;
                        }
                        if button(ui, "Close", Button::Small).clicked() {
                            close_now = true;
                        }
                    });
                });
        }

        if refresh {
            self.report_preview = Some(nestcraft_core::report::BuildReport::generate(&self.job));
        } else if open_html_now
            && let Some(rep) = &self.report_preview
        {
            let path = std::env::temp_dir().join("nestcraft_build_report.html");
            match std::fs::write(&path, rep.to_html()).and_then(|()| open_file_in_default_app(&path)) {
                Ok(()) => self.status = format!("Opened HTML report {}", path.display()),
                Err(e) => self.status = format!("Failed to open HTML report: {e}"),
            }
        } else if export_html
            && let Some(rep) = &self.report_preview
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("HTML Report", &["html"])
                .set_file_name("build_report.html")
                .save_file()
        {
            match std::fs::write(&path, rep.to_html()).and_then(|()| open_file_in_default_app(&path)) {
                Ok(()) => self.status = format!("Exported and opened HTML report {}", path.display()),
                Err(e) => self.status = format!("HTML report export failed: {e}"),
            }
        } else if export_csv
            && let Some(rep) = &self.report_preview
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("CSV Report", &["csv"])
                .set_file_name("build_report.csv")
                .save_file()
        {
            match std::fs::write(&path, rep.to_csv()).and_then(|()| open_file_in_default_app(&path)) {
                Ok(()) => self.status = format!("Exported and opened CSV report {}", path.display()),
                Err(e) => self.status = format!("CSV report export failed: {e}"),
            }
        }
        if close_now || !open {
            self.report_dialog_open = false;
        }
    }

    pub(crate) fn draw_failed_print_modal(&mut self, ctx: &egui::Context) {
        if !self.failed_print_dialog_open {
            return;
        }
        let mut open = self.failed_print_dialog_open;
        let mut use_current = false;
        let mut browse_job = false;
        let mut select_reprint = false;
        let mut load_reprint_job = false;
        let mut export_csv = false;
        let mut close_now = false;
        let t = Tokens::get();

        let inspecting_current = self.failed_print_external_job.is_none();
        let report = {
            let target_job = self.failed_print_external_job.as_ref().unwrap_or(&self.job);
            nestcraft_core::recovery::FailedPrintReport::classify(target_job, self.failed_print_z_mm)
        };
        let max_slider_z = report.job_max_z_mm.max(1.0);
        let reprint_total = report.partial_count + report.not_started_count;

        egui::Window::new("Failed Print Recovery")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .movable(true)
            .default_width(660.0)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                row(ui, "Select Job", |ui| {
                    ui.horizontal(|ui| {
                        if button(ui, "Use Current Bed", Button::Small).clicked() {
                            use_current = true;
                        }
                        if button(ui, "Browse .nestc...", Button::Small).clicked() {
                            browse_job = true;
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "{} ({} parts inside machine)",
                                self.failed_print_source_label,
                                report.rows.len()
                            ))
                            .small()
                            .color(t.text_dim),
                        );
                    });
                });

                row(ui, "Aborted Z (mm)", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.failed_print_z_mm)
                                .range(0.0..=max_slider_z)
                                .speed(0.5)
                                .suffix(" mm"),
                        );
                        ui.add(egui::Slider::new(&mut self.failed_print_z_mm, 0.0..=max_slider_z).show_value(false));
                        let layer_num = (self.failed_print_z_mm / 0.1).round() as u32;
                        ui.label(egui::RichText::new(format!("Layer ~{layer_num}")).small().color(t.text_dim));
                    });
                });

                ui.add_space(4.0);
                egui::Grid::new("failed_print_kpi_grid")
                    .num_columns(4)
                    .spacing([14.0, 4.0])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Completed (OK)").small().color(t.text_dim));
                        ui.label(egui::RichText::new("Partial / Ruined").small().color(t.text_dim));
                        ui.label(egui::RichText::new("Not Started").small().color(t.text_dim));
                        ui.label(egui::RichText::new("Needs Reprint").small().color(t.text_dim));
                        ui.end_row();

                        ui.label(
                            egui::RichText::new(format!("{}", report.completed_count))
                                .strong()
                                .color(egui::Color32::from_rgb(0x22, 0xC5, 0x5E)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{}", report.partial_count))
                                .strong()
                                .color(t.error),
                        );
                        ui.label(
                            egui::RichText::new(format!("{}", report.not_started_count))
                                .strong()
                                .color(egui::Color32::from_rgb(0xF5, 0x9E, 0x0B)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{reprint_total} ({:.2} cm³)", report.reprint_volume_cm3))
                                .strong()
                                .color(t.accent),
                        );
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.push_id("failed_print_filter_seg", |ui| {
                    segmented(
                        ui,
                        &mut self.failed_print_filter,
                        &["All", "Needs Reprint", "Completed"],
                        Segments {
                            dense: true,
                            ..Default::default()
                        },
                    );
                });

                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("failed_print_table_scroll")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        egui::Grid::new("failed_print_table")
                            .striped(true)
                            .num_columns(5)
                            .spacing([14.0, 4.0])
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("ID").strong());
                                ui.label(egui::RichText::new("Part").strong());
                                ui.label(egui::RichText::new("Z Range (mm)").strong());
                                ui.label(egui::RichText::new("Printed").strong());
                                ui.label(egui::RichText::new("Status").strong());
                                ui.end_row();

                                for r in &report.rows {
                                    let show = match self.failed_print_filter {
                                        1 => r.needs_reprint(),
                                        2 => !r.needs_reprint(),
                                        _ => true,
                                    };
                                    if !show {
                                        continue;
                                    }
                                    ui.label(format!("#{}", r.instance_id.0));
                                    ui.label(&r.name);
                                    ui.label(format!("{:.1} .. {:.1}", r.min_z_mm, r.max_z_mm));
                                    match r.status {
                                        nestcraft_core::recovery::FailedPrintStatus::Completed => {
                                            ui.label("100.0%");
                                            ui.label(
                                                egui::RichText::new("Completed")
                                                    .color(egui::Color32::from_rgb(0x22, 0xC5, 0x5E)),
                                            );
                                        }
                                        nestcraft_core::recovery::FailedPrintStatus::Partial { pct_printed } => {
                                            ui.label(format!("{pct_printed:.1}%"));
                                            ui.label(egui::RichText::new("Partial (Reprint)").color(t.error));
                                        }
                                        nestcraft_core::recovery::FailedPrintStatus::NotStarted => {
                                            ui.label("0.0%");
                                            ui.label(
                                                egui::RichText::new("Not Started (Reprint)")
                                                    .color(egui::Color32::from_rgb(0xF5, 0x9E, 0x0B)),
                                            );
                                        }
                                    }
                                    ui.end_row();
                                }
                            });
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    if inspecting_current
                        && reprint_total > 0
                        && button(ui, "Select Reprint in Viewport", Button::Small).clicked()
                    {
                        select_reprint = true;
                    }
                    if reprint_total > 0 && button(ui, "Load Reprint Job on Bed", Button::Primary).clicked() {
                        load_reprint_job = true;
                    }
                    if reprint_total > 0 && button(ui, "Export Reprint CSV...", Button::Small).clicked() {
                        export_csv = true;
                    }
                    if button(ui, "Close", Button::Small).clicked() {
                        close_now = true;
                    }
                });
            });

        if use_current {
            self.failed_print_external_job = None;
            self.failed_print_source_label = "Current Bed".to_owned();
            self.failed_print_z_mm = (self.job.max_z_mm() * 0.5).max(0.0);
        } else if browse_job
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("NestCraft Bed", &["nestc"])
                .pick_file()
        {
            match std::fs::read(&path).and_then(|b| {
                nestcraft_core::io::nestc::read_nestc(&b).map_err(std::io::Error::other)
            }) {
                Ok(loaded) => {
                    self.failed_print_z_mm = (loaded.max_z_mm() * 0.5).max(0.0);
                    self.failed_print_source_label = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    self.failed_print_external_job = Some(loaded);
                }
                Err(e) => {
                    self.status = format!("Failed to load job: {e}");
                }
            }
        } else if select_reprint {
            let ids = report.reprint_instance_ids();
            self.selected_set.clear();
            for &id in &ids {
                self.selected_set.insert(id);
            }
            self.selected = ids.last().copied();
            self.selection_anchor = self.selected;
            self.mark_dirty();
            self.status = format!("Selected {} part(s) needing reprint", ids.len());
        } else if load_reprint_job {
            let reprint_job = {
                let target_job = self.failed_print_external_job.as_ref().unwrap_or(&self.job);
                nestcraft_core::recovery::build_reprint_job(target_job, self.failed_print_z_mm)
            };
            let count = reprint_job.instances.len();
            self.apply_loaded_bed(reprint_job, "Reprint Job", None);
            self.failed_print_dialog_open = false;
            self.status = format!("Loaded reprint job with {count} part(s)");
            return;
        } else if export_csv
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("CSV Reprint List", &["csv"])
                .set_file_name("reprint_parts.csv")
                .save_file()
        {
            match std::fs::write(&path, report.to_reprint_csv()).and_then(|()| open_file_in_default_app(&path)) {
                Ok(()) => self.status = format!("Exported and opened reprint CSV {}", path.display()),
                Err(e) => self.status = format!("Reprint CSV export failed: {e}"),
            }
        }

        if close_now || !open {
            self.failed_print_dialog_open = false;
        }
    }

    pub(crate) fn draw_nest2d_modal(&mut self, ctx: &egui::Context) {
        if !self.nest2d_dialog_open {
            return;
        }
        let mut open = self.nest2d_dialog_open;
        let mut run_now = false;
        let mut close_now = false;
        let t = Tokens::get();

        egui::Window::new("2D Auto-Nest Configuration")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                row(ui, "Target", |ui| {
                    let mut tgt_idx = match self.nest2d_config.target {
                        nestcraft_core::nest2d::Nest2dTarget::ToPlatform => 0,
                        nestcraft_core::nest2d::Nest2dTarget::InPlaceOutsidePlatform => 1,
                    };
                    if ui
                        .push_id("nest2d_target_seg", |ui| {
                            segmented(
                                ui,
                                &mut tgt_idx,
                                &["To Platform", "In Place (Outside)"],
                                Segments {
                                    dense: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .inner
                    {
                        self.nest2d_config.target = if tgt_idx == 1 {
                            nestcraft_core::nest2d::Nest2dTarget::InPlaceOutsidePlatform
                        } else {
                            nestcraft_core::nest2d::Nest2dTarget::ToPlatform
                        };
                    }
                });

                row(ui, "Scope", |ui| {
                    let mut scope_idx = usize::from(self.nest2d_config.selected_only && !self.selected_set.is_empty());
                    let labels = if self.selected_set.is_empty() {
                        ["All Visible", "Selected (None)"]
                    } else {
                        ["All Visible", "Selected Only"]
                    };
                    if ui
                        .push_id("nest2d_scope_seg", |ui| {
                            segmented(
                                ui,
                                &mut scope_idx,
                                &labels,
                                Segments {
                                    dense: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .inner
                    {
                        self.nest2d_config.selected_only = scope_idx == 1 && !self.selected_set.is_empty();
                    }
                });

                row(ui, "Min Distance", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.nest2d_config.min_distance_mm)
                            .range(0.0..=50.0)
                            .speed(0.25)
                            .suffix(" mm"),
                    )
                    .backing_on_hover_text("Minimum XY clearance between nested parts");
                });

                row(ui, "Resolution", |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Accurate").small().color(t.text_dim));
                        ui.add(
                            egui::Slider::new(&mut self.nest2d_config.resolution_mm, 0.5..=3.0)
                                .step_by(0.25)
                                .show_value(false),
                        )
                        .backing_on_hover_text("Drag left for higher accuracy, right for faster nesting");
                        ui.label(egui::RichText::new("Fast").small().color(t.text_dim));
                    });
                });

                if self.nest2d_config.target == nestcraft_core::nest2d::Nest2dTarget::ToPlatform {
                    row(ui, "Wall Margin", |ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.nest2d_config.wall_margin_mm)
                                .range(0.0..=50.0)
                                .speed(0.5)
                                .suffix(" mm"),
                        )
                        .backing_on_hover_text("Keep-in margin from platform X/Y edges");
                    });
                }

                row(ui, "Z Rotation", |ui| {
                    let mut rot_idx = match self.nest2d_config.rotation_step_deg {
                        0 => 0,
                        90 => 1,
                        45 => 2,
                        _ => 3,
                    };
                    if ui
                        .push_id("nest2d_rot_seg", |ui| {
                            segmented(
                                ui,
                                &mut rot_idx,
                                &["0°", "90°", "45°", "15°"],
                                Segments {
                                    dense: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .inner
                    {
                        self.nest2d_config.rotation_step_deg = match rot_idx {
                            0 => 0,
                            1 => 90,
                            2 => 45,
                            _ => 15,
                        };
                    }
                });

                if checkbox_row(
                    ui,
                    "nest2d_use_min_bbox",
                    "Use Minimum X/Y Bounding Box (no interlocking)",
                    self.nest2d_config.use_min_bbox,
                ) {
                    self.nest2d_config.use_min_bbox = !self.nest2d_config.use_min_bbox;
                }
                if checkbox_row(
                    ui,
                    "nest2d_drop_min_z",
                    "Drop nested parts to Min Z floor",
                    self.nest2d_config.drop_to_min_z,
                ) {
                    self.nest2d_config.drop_to_min_z = !self.nest2d_config.drop_to_min_z;
                }
                if self.nest2d_config.target == nestcraft_core::nest2d::Nest2dTarget::ToPlatform
                    && checkbox_row(
                        ui,
                        "nest2d_avoid_nbz",
                        "Avoid enabled No-Build Zone",
                        self.nest2d_config.avoid_no_build_zone,
                    )
                {
                    self.nest2d_config.avoid_no_build_zone = !self.nest2d_config.avoid_no_build_zone;
                }

                if let Some(res) = &self.nest2d_last_result {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Last run: {} placed, {} overflow, {:.1}% bed fill ({} ms)",
                            res.placed_count, res.overflow_count, res.used_area_pct, res.elapsed_ms
                        ))
                        .small()
                        .color(if res.overflow_count > 0 { t.error } else { t.accent }),
                    );
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if self.calc_task.is_none() && button(ui, "Run 2D Nest", Button::Primary).clicked() {
                        run_now = true;
                    }
                    if button(ui, "Close", Button::Small).clicked() {
                        close_now = true;
                    }
                });
            });

        if run_now {
            self.start_nest2d();
        }
        if close_now || !open {
            self.nest2d_dialog_open = false;
        }
    }

    pub(crate) fn draw_nest3d_modal(&mut self, ctx: &egui::Context) {
        if !self.nest3d_dialog_open {
            return;
        }
        use nestcraft_core::nest3d::{
            ClearanceMode, Nest3dOptimizerMode, Nest3dPreset, OrientationFreedom,
            auto_fit_nest3d_resolution, estimate_nest3d_memory,
        };
        use nestcraft_core::voxel::{CpuBudget, FreeAngleSet, ZOnlyStep};

        let mut open = self.nest3d_dialog_open;
        let mut run_now = false;
        let mut close_now = false;
        let t = Tokens::get();

        let [_, _, machine_z] = self.job.machine.size_mm();
        let min_z_floor = self.nest3d_config.min_z_mm.max(0.0);
        let max_z_ceiling = (machine_z - self.nest3d_config.wall_margin_mm.max(0.0)).max(min_z_floor + 1.0);
        let has_selection = !self.selected_set.is_empty();

        Container::new(ctx, Surface::Window)
            .window("3D Auto-Nest Configuration")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                // Primary controls (compact tier)
                row(ui, "Preset", |ui| {
                    let mut p_idx = match self.nest3d_config.preset {
                        Nest3dPreset::Fast => 0,
                        Nest3dPreset::Balanced => 1,
                        Nest3dPreset::Refined => 2,
                        Nest3dPreset::Custom => 3,
                    };
                    if ui
                        .push_id("nest3d_preset_seg", |ui| {
                            segmented(
                                ui,
                                &mut p_idx,
                                &["Fast", "Balanced", "Refined", "Custom"],
                                Segments {
                                    dense: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .inner
                    {
                        let preset = match p_idx {
                            0 => Nest3dPreset::Fast,
                            1 => Nest3dPreset::Balanced,
                            2 => Nest3dPreset::Refined,
                            _ => Nest3dPreset::Custom,
                        };
                        self.nest3d_config.apply_preset(preset);
                    }
                });

                row(ui, "Scope", |ui| {
                    ui.vertical(|ui| {
                        ui.add_enabled_ui(has_selection, |ui| {
                            if ui
                                .checkbox(
                                    &mut self.nest3d_config.selected_only,
                                    if has_selection {
                                        "Pack Selected Only"
                                    } else {
                                        "Pack Selected Only (no selection)"
                                    },
                                )
                                .changed()
                                && !has_selection
                            {
                                self.nest3d_config.selected_only = false;
                            }
                        });
                        ui.checkbox(
                            &mut self.nest3d_config.keep_in_bounds_fixed,
                            "Keep Parts Inside Build Volume Fixed",
                        )
                        .backing_on_hover_text(
                            "Never move, rotate, or re-pack parts already inside the raw machine build envelope",
                        );
                    });
                });

                row(ui, "Min Distance", |ui| {
                    ui.horizontal(|ui| {
                        let min_drag = ui.add(
                            egui::DragValue::new(&mut self.nest3d_config.min_distance_mm)
                                .range(0.0..=25.0)
                                .speed(0.25)
                                .suffix(" mm"),
                        );
                        if min_drag.changed() {
                            self.nest3d_config.preset = Nest3dPreset::Custom;
                            if self.nest3d_config.adaptive_max_distance_mm < self.nest3d_config.min_distance_mm {
                                self.nest3d_config.adaptive_max_distance_mm = self.nest3d_config.min_distance_mm;
                            }
                        }
                        if self.nest3d_config.clearance_mode == ClearanceMode::AdaptiveByPartMass {
                            ui.label("↔ Max");
                            let min_d = self.nest3d_config.min_distance_mm;
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.nest3d_config.adaptive_max_distance_mm)
                                        .range(min_d..=25.0)
                                        .speed(0.25)
                                        .suffix(" mm"),
                                )
                                .changed()
                            {
                                self.nest3d_config.preset = Nest3dPreset::Custom;
                            }
                        }
                    });
                });

                row(ui, "Resolution", |ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Fine").small().color(t.text_dim));
                            if ui
                                .add(
                                    egui::Slider::new(&mut self.nest3d_config.resolution_mm, 0.5..=2.0)
                                        .step_by(0.1)
                                        .suffix(" mm"),
                                )
                                .changed()
                            {
                                self.nest3d_config.preset = Nest3dPreset::Custom;
                            }
                            ui.label(egui::RichText::new("Fast").small().color(t.text_dim));
                        });
                        ui.horizontal(|ui| {
                            for (lbl, val) in [("Fine 0.5", 0.5_f32), ("Balanced 1.0", 1.0), ("Fast 2.0", 2.0)] {
                                if ui
                                    .selectable_label((self.nest3d_config.resolution_mm - val).abs() < 0.05, lbl)
                                    .clicked()
                                {
                                    self.nest3d_config.resolution_mm = val;
                                    self.nest3d_config.preset = Nest3dPreset::Custom;
                                }
                            }
                        });
                    });
                });

                row(ui, "Z Ceiling", |ui| {
                    ui.horizontal(|ui| {
                        let mut has_cap = self.nest3d_config.max_z_limit_mm.is_some();
                        if ui.checkbox(&mut has_cap, "Limit Max Z").changed() {
                            self.nest3d_config.max_z_limit_mm = if has_cap {
                                Some(max_z_ceiling)
                            } else {
                                None
                            };
                            self.sync_nest3d_ceiling_overlay();
                        }
                        if let Some(ref mut cap) = self.nest3d_config.max_z_limit_mm
                            && ui
                                .add(
                                    egui::DragValue::new(cap)
                                        .range((min_z_floor + 1.0)..=max_z_ceiling)
                                        .speed(1.0)
                                        .suffix(" mm"),
                                )
                                .changed()
                        {
                            self.sync_nest3d_ceiling_overlay();
                        }
                    });
                });

                if checkbox_row(
                    ui,
                    "nest3d_prevent_interlock",
                    "Prevent Interlocking (allow open cavities, verify escape path)",
                    self.nest3d_config.prevent_interlocking,
                ) {
                    self.nest3d_config.prevent_interlocking = !self.nest3d_config.prevent_interlocking;
                }

                // Live memory estimate + sysinfo guardrail
                let est = estimate_nest3d_memory(&self.job, &self.nest3d_config, Some(&self.selected_set));
                let half_avail = (self.nest3d_sys_avail_ram_bytes / 2).max(256 * 1024 * 1024) as usize;
                let soft_cap = self
                    .nest3d_config
                    .max_memory_bytes
                    .map_or(half_avail, |u| u.min(half_avail));
                let est_mb = est.total_bytes as f64 / (1024.0 * 1024.0);
                let cap_mb = soft_cap as f64 / (1024.0 * 1024.0);
                let ram_warn = est.total_bytes > soft_cap;
                row(ui, "Est. RAM", |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("~{est_mb:.1} MB (budget {cap_mb:.0} MB)"))
                                .small()
                                .color(if ram_warn { t.error } else { t.text_dim }),
                        );
                        if ram_warn
                            && button(ui, "Auto-Fit Resolution", Button::Small)
                                .backing_on_hover_text("Coarsen voxel resolution to stay within safe RAM budget")
                                .clicked()
                        {
                            self.nest3d_config.resolution_mm = auto_fit_nest3d_resolution(
                                &self.job,
                                &self.nest3d_config,
                                Some(&self.selected_set),
                                soft_cap,
                            );
                            self.nest3d_config.preset = Nest3dPreset::Custom;
                        }
                    });
                });

                ui.add_space(4.0);
                egui::CollapsingHeader::new("Advanced Options")
                    .default_open(self.nest3d_advanced_open)
                    .show(ui, |ui| {
                        self.nest3d_advanced_open = true;
                        egui::ScrollArea::vertical()
                            .id_salt("nest3d_adv_scroll")
                            .max_height(260.0)
                            .show(ui, |ui| {
                                row(ui, "Optimizer", |ui| {
                                    let mut opt_idx = match self.nest3d_config.optimizer_mode {
                                        Nest3dOptimizerMode::ConstructiveOnly => 0,
                                        Nest3dOptimizerMode::RuinAndRecreate => 1,
                                        Nest3dOptimizerMode::Genetic => 2,
                                        Nest3dOptimizerMode::HybridGaRuin => 3,
                                    };
                                    if ui
                                        .push_id("nest3d_opt_seg", |ui| {
                                            segmented(
                                                ui,
                                                &mut opt_idx,
                                                &["BLBF Only", "Ruin", "GA", "Hybrid"],
                                                Segments {
                                                    dense: true,
                                                    ..Default::default()
                                                },
                                            )
                                        })
                                        .inner
                                    {
                                        self.nest3d_config.optimizer_mode = match opt_idx {
                                            0 => Nest3dOptimizerMode::ConstructiveOnly,
                                            1 => Nest3dOptimizerMode::RuinAndRecreate,
                                            2 => Nest3dOptimizerMode::Genetic,
                                            _ => Nest3dOptimizerMode::HybridGaRuin,
                                        };
                                        self.nest3d_config.preset = Nest3dPreset::Custom;
                                    }
                                });

                                row(ui, "Clearance Mode", |ui| {
                                    let mut c_idx = match self.nest3d_config.clearance_mode {
                                        ClearanceMode::Fixed => 0,
                                        ClearanceMode::AdaptiveByPartMass => 1,
                                    };
                                    if ui
                                        .push_id("nest3d_cl_mode", |ui| {
                                            segmented(
                                                ui,
                                                &mut c_idx,
                                                &["Fixed", "Adaptive (Mass)"],
                                                Segments {
                                                    dense: true,
                                                    ..Default::default()
                                                },
                                            )
                                        })
                                        .inner
                                    {
                                        self.nest3d_config.clearance_mode = if c_idx == 1 {
                                            ClearanceMode::AdaptiveByPartMass
                                        } else {
                                            ClearanceMode::Fixed
                                        };
                                    }
                                });

                                row(ui, "Separate Z Gap", |ui| {
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut self.nest3d_config.separate_z_distance, "Enable");
                                        if self.nest3d_config.separate_z_distance {
                                            ui.add(
                                                egui::DragValue::new(&mut self.nest3d_config.min_z_distance_mm)
                                                    .range(0.0..=25.0)
                                                    .speed(0.25)
                                                    .suffix(" mm"),
                                            );
                                        }
                                    });
                                });

                                row(ui, "Margins", |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label("Wall");
                                        ui.add(
                                            egui::DragValue::new(&mut self.nest3d_config.wall_margin_mm)
                                                .range(0.0..=50.0)
                                                .speed(0.5)
                                                .suffix(" mm"),
                                        );
                                        ui.label("Min Z");
                                        ui.add(
                                            egui::DragValue::new(&mut self.nest3d_config.min_z_mm)
                                                .range(0.0..=100.0)
                                                .speed(0.5)
                                                .suffix(" mm"),
                                        );
                                    });
                                });

                                row(ui, "Orientation", |ui| {
                                    let mut f_idx = match self.nest3d_config.orientation_freedom {
                                        OrientationFreedom::RespectPartConstraints => 0,
                                        OrientationFreedom::ForceFixed => 1,
                                        OrientationFreedom::ForceZOnly => 2,
                                        OrientationFreedom::ForceFree => 3,
                                    };
                                    if ui
                                        .push_id("nest3d_orient_free", |ui| {
                                            segmented(
                                                ui,
                                                &mut f_idx,
                                                &["Per-Part", "Fixed", "Z-Only", "Free"],
                                                Segments {
                                                    dense: true,
                                                    ..Default::default()
                                                },
                                            )
                                        })
                                        .inner
                                    {
                                        self.nest3d_config.orientation_freedom = match f_idx {
                                            1 => OrientationFreedom::ForceFixed,
                                            2 => OrientationFreedom::ForceZOnly,
                                            3 => OrientationFreedom::ForceFree,
                                            _ => OrientationFreedom::RespectPartConstraints,
                                        };
                                    }
                                });

                                row(ui, "Free Angle Set", |ui| {
                                    ui.horizontal(|ui| {
                                        let mut a_idx = match self.nest3d_config.free_angle_set {
                                            FreeAngleSet::Octahedral24 => 0,
                                            FreeAngleSet::Tilted48 => 1,
                                            FreeAngleSet::Dense64 => 2,
                                        };
                                        if ui
                                            .push_id("nest3d_angles_seg", |ui| {
                                                segmented(
                                                    ui,
                                                    &mut a_idx,
                                                    &["24 (90°)", "48 (45°)", "64"],
                                                    Segments {
                                                        dense: true,
                                                        ..Default::default()
                                                    },
                                                )
                                            })
                                            .inner
                                        {
                                            self.nest3d_config.free_angle_set = match a_idx {
                                                1 => FreeAngleSet::Tilted48,
                                                2 => FreeAngleSet::Dense64,
                                                _ => FreeAngleSet::Octahedral24,
                                            };
                                        }
                                        ui.checkbox(&mut self.nest3d_config.use_pca_prealign, "PCA Align");
                                    });
                                });

                                row(ui, "Z-Only Step", |ui| {
                                    let mut z_idx = match self.nest3d_config.z_only_step {
                                        ZOnlyStep::Deg90 => 0,
                                        ZOnlyStep::Deg45 => 1,
                                        ZOnlyStep::Deg30 => 2,
                                    };
                                    if ui
                                        .push_id("nest3d_zstep_seg", |ui| {
                                            segmented(
                                                ui,
                                                &mut z_idx,
                                                &["90° (4)", "45° (8)", "30° (12)"],
                                                Segments {
                                                    dense: true,
                                                    ..Default::default()
                                                },
                                            )
                                        })
                                        .inner
                                    {
                                        self.nest3d_config.z_only_step = match z_idx {
                                            0 => ZOnlyStep::Deg90,
                                            2 => ZOnlyStep::Deg30,
                                            _ => ZOnlyStep::Deg45,
                                        };
                                    }
                                });

                                if checkbox_row(
                                    ui,
                                    "nest3d_open_cavities",
                                    "Allow Open-Cavity Nesting",
                                    self.nest3d_config.allow_open_cavities,
                                ) {
                                    self.nest3d_config.allow_open_cavities = !self.nest3d_config.allow_open_cavities;
                                }
                                if checkbox_row(
                                    ui,
                                    "nest3d_avoid_nbz",
                                    "Avoid enabled No-Build Zone",
                                    self.nest3d_config.avoid_no_build_zone,
                                ) {
                                    self.nest3d_config.avoid_no_build_zone = !self.nest3d_config.avoid_no_build_zone;
                                }

                                row(ui, "Thermal / Slice", |ui| {
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::Slider::new(&mut self.nest3d_config.thermal_weight, 0.0..=1.0)
                                                .text("Thermal"),
                                        );
                                        ui.add(
                                            egui::Slider::new(
                                                &mut self.nest3d_config.slice_smoothness_weight,
                                                0.0..=1.0,
                                            )
                                            .text("Slice"),
                                        );
                                    })
                                    .response
                                })
                                .backing_on_hover_text("Applied in Stage 2 refined optimization");

                                row(ui, "Physics Polish", |ui| {
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut self.nest3d_config.gravity_settle_after, "Gravity Settle");
                                        ui.checkbox(&mut self.nest3d_config.allow_micro_rotations, "Micro-Tilt");
                                        if self.nest3d_config.allow_micro_rotations {
                                            ui.add(
                                                egui::DragValue::new(&mut self.nest3d_config.max_tilt_deg)
                                                    .range(0.0..=15.0)
                                                    .speed(0.5)
                                                    .suffix("°"),
                                            );
                                        }
                                    });
                                });

                                row(ui, "CPU Threads", |ui| {
                                    let mut cpu_idx = match self.nest3d_config.cpu_budget {
                                        CpuBudget::Quiet => 0,
                                        CpuBudget::Balanced => 1,
                                        CpuBudget::Max => 2,
                                        CpuBudget::Custom(_) => 3,
                                    };
                                    if ui
                                        .push_id("nest3d_cpu_seg", |ui| {
                                            segmented(
                                                ui,
                                                &mut cpu_idx,
                                                &["Quiet 50%", "Balanced 75%", "Max All-1"],
                                                Segments {
                                                    dense: true,
                                                    ..Default::default()
                                                },
                                            )
                                        })
                                        .inner
                                    {
                                        self.nest3d_config.cpu_budget = match cpu_idx {
                                            0 => CpuBudget::Quiet,
                                            2 => CpuBudget::Max,
                                            _ => CpuBudget::Balanced,
                                        };
                                    }
                                });
                            });
                    });

                if let Some(res) = &self.nest3d_last_result {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Last run: {} placed, {} overflow | Max Z {:.1} mm | {:.2}% density ({} ms)",
                            res.placed_count,
                            res.overflow_count,
                            res.max_z_mm,
                            res.packing_density_pct,
                            res.elapsed_ms
                        ))
                        .small()
                        .color(if res.overflow_count > 0 { t.error } else { t.accent }),
                    );
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if !self.is_nest3d_running()
                        && self.calc_task.is_none()
                        && button(ui, "Run 3D Nest", Button::Primary).clicked()
                    {
                        run_now = true;
                    }
                    if button(ui, "Close", Button::Small).clicked() {
                        close_now = true;
                    }
                });
            });

        if run_now {
            self.start_nest3d(ctx);
        } else if close_now || !open {
            self.nest3d_dialog_open = false;
            self.sync_nest3d_ceiling_overlay();
        }
    }

    pub(crate) fn draw_gravity_settle_modal(&mut self, ctx: &egui::Context) {
        if !self.gravity_settle_dialog_open {
            return;
        }
        use nestcraft_core::compact::SettleDirection;
        use nestcraft_core::nest3d::ClearanceMode;

        let mut open = self.gravity_settle_dialog_open;
        let mut run_now = false;
        let mut close_now = false;
        let t = Tokens::get();

        let [_, _, machine_z] = self.job.machine.size_mm();
        let min_z_floor = self.gravity_settle_config.min_z_mm.max(0.0);
        let max_z_ceiling = (machine_z - self.gravity_settle_config.wall_margin_mm.max(0.0)).max(min_z_floor + 1.0);
        let has_selection = !self.selected_set.is_empty();

        Container::new(ctx, Surface::Window)
            .window("Gravity Settle / Compact")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.set_min_width(400.0);

                row(ui, "Direction", |ui| {
                    let mut dir_idx = match self.gravity_settle_config.direction {
                        SettleDirection::DownZ => 0,
                        SettleDirection::DownZPlusInwardXY => 1,
                    };
                    if ui
                        .push_id("gravity_settle_dir_seg", |ui| {
                            segmented(
                                ui,
                                &mut dir_idx,
                                &["Down -Z", "Down -Z + Inward XY"],
                                Segments {
                                    dense: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .inner
                    {
                        self.gravity_settle_config.direction = if dir_idx == 1 {
                            SettleDirection::DownZPlusInwardXY
                        } else {
                            SettleDirection::DownZ
                        };
                    }
                });

                row(ui, "Scope", |ui| {
                    ui.vertical(|ui| {
                        ui.add_enabled_ui(has_selection, |ui| {
                            if ui
                                .checkbox(
                                    &mut self.gravity_settle_config.selected_only,
                                    if has_selection {
                                        "Compact Selected Only"
                                    } else {
                                        "Compact Selected Only (no selection)"
                                    },
                                )
                                .changed()
                                && !has_selection
                            {
                                self.gravity_settle_config.selected_only = false;
                            }
                        });
                        ui.checkbox(
                            &mut self.gravity_settle_config.keep_in_bounds_fixed,
                            "Keep Parts Inside Build Volume Fixed",
                        )
                        .backing_on_hover_text(
                            "Never move or rotate parts already inside the raw machine build envelope",
                        );
                    });
                });

                row(ui, "Clearance Mode", |ui| {
                    let mut cm_idx = match self.gravity_settle_config.clearance_mode {
                        ClearanceMode::Fixed => 0,
                        ClearanceMode::AdaptiveByPartMass => 1,
                    };
                    if ui
                        .push_id("gravity_settle_cm_seg", |ui| {
                            segmented(
                                ui,
                                &mut cm_idx,
                                &["Fixed", "Adaptive by Part Mass"],
                                Segments {
                                    dense: true,
                                    ..Default::default()
                                },
                            )
                        })
                        .inner
                    {
                        self.gravity_settle_config.clearance_mode = if cm_idx == 1 {
                            ClearanceMode::AdaptiveByPartMass
                        } else {
                            ClearanceMode::Fixed
                        };
                    }
                });

                row(ui, "Min Distance", |ui| {
                    ui.horizontal(|ui| {
                        let min_drag = ui.add(
                            egui::DragValue::new(&mut self.gravity_settle_config.min_distance_mm)
                                .range(0.0..=25.0)
                                .speed(0.25)
                                .suffix(" mm"),
                        );
                        if min_drag.changed()
                            && self.gravity_settle_config.adaptive_max_distance_mm
                                < self.gravity_settle_config.min_distance_mm
                        {
                            self.gravity_settle_config.adaptive_max_distance_mm =
                                self.gravity_settle_config.min_distance_mm;
                        }
                        if self.gravity_settle_config.clearance_mode == ClearanceMode::AdaptiveByPartMass {
                            ui.label("↔ Max");
                            let min_d = self.gravity_settle_config.min_distance_mm;
                            ui.add(
                                egui::DragValue::new(&mut self.gravity_settle_config.adaptive_max_distance_mm)
                                    .range(min_d..=25.0)
                                    .speed(0.25)
                                    .suffix(" mm"),
                            );
                        }
                    });
                });

                row(ui, "Z Distance", |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.gravity_settle_config.separate_z_distance, "Separate Z");
                        if self.gravity_settle_config.separate_z_distance {
                            ui.add(
                                egui::DragValue::new(&mut self.gravity_settle_config.min_z_distance_mm)
                                    .range(0.0..=25.0)
                                    .speed(0.25)
                                    .suffix(" mm"),
                            );
                        }
                    });
                });

                row(ui, "Margins", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Wall");
                        ui.add(
                            egui::DragValue::new(&mut self.gravity_settle_config.wall_margin_mm)
                                .range(0.0..=50.0)
                                .speed(0.5)
                                .suffix(" mm"),
                        );
                        ui.label("Min Z");
                        ui.add(
                            egui::DragValue::new(&mut self.gravity_settle_config.min_z_mm)
                                .range(0.0..=max_z_ceiling)
                                .speed(0.5)
                                .suffix(" mm"),
                        );
                    });
                });

                row(ui, "Z Ceiling", |ui| {
                    ui.horizontal(|ui| {
                        let mut has_cap = self.gravity_settle_config.max_z_limit_mm.is_some();
                        if ui.checkbox(&mut has_cap, "Limit Max Z").changed() {
                            self.gravity_settle_config.max_z_limit_mm = if has_cap {
                                Some(max_z_ceiling)
                            } else {
                                None
                            };
                        }
                        if let Some(ref mut cap) = self.gravity_settle_config.max_z_limit_mm {
                            ui.add(
                                egui::DragValue::new(cap)
                                    .range((min_z_floor + 1.0)..=max_z_ceiling)
                                    .speed(1.0)
                                    .suffix(" mm"),
                            );
                        }
                    });
                });

                row(ui, "Micro-Tilt", |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.gravity_settle_config.allow_micro_rotations, "Allow Micro-Rotations");
                        if self.gravity_settle_config.allow_micro_rotations {
                            ui.add(
                                egui::DragValue::new(&mut self.gravity_settle_config.max_tilt_deg)
                                    .range(0.5..=25.0)
                                    .speed(0.5)
                                    .suffix("°"),
                            );
                        }
                    });
                });

                if checkbox_row(
                    ui,
                    "gravity_settle_prevent_interlock",
                    "Prevent Interlocking (verify escape path after settle)",
                    self.gravity_settle_config.prevent_interlocking,
                ) {
                    self.gravity_settle_config.prevent_interlocking =
                        !self.gravity_settle_config.prevent_interlocking;
                }
                if checkbox_row(
                    ui,
                    "gravity_settle_avoid_nbz",
                    "Avoid enabled No-Build Zone",
                    self.gravity_settle_config.avoid_no_build_zone,
                ) {
                    self.gravity_settle_config.avoid_no_build_zone =
                        !self.gravity_settle_config.avoid_no_build_zone;
                }

                row(ui, "Budget", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Steps");
                        ui.add(egui::DragValue::new(&mut self.gravity_settle_config.max_steps).range(30..=1000).speed(10.0));
                        ui.label("Time");
                        ui.add(
                            egui::DragValue::new(&mut self.gravity_settle_config.time_limit_secs)
                                .range(0.5..=60.0)
                                .speed(0.5)
                                .suffix(" s"),
                        );
                    });
                });

                if let Some(res) = &self.gravity_settle_last_result {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Last settle: {} moved, {} rolled back | Max Z {:.1} -> {:.1} mm (-{:.1} mm, {} ms)",
                            res.moved_count,
                            res.rolled_back_count,
                            res.max_z_before_mm,
                            res.max_z_after_mm,
                            res.z_reduction_mm,
                            res.elapsed_ms
                        ))
                        .small()
                        .color(if res.rolled_back_count > 0 { t.text_dim } else { t.accent }),
                    );
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if !self.is_nest3d_running()
                        && self.calc_task.is_none()
                        && button(ui, "Run Gravity Settle", Button::Primary).clicked()
                    {
                        run_now = true;
                    }
                    if button(ui, "Close", Button::Small).clicked() {
                        close_now = true;
                    }
                });
            });

        if run_now {
            self.start_gravity_settle(ctx);
        } else if close_now || !open {
            self.gravity_settle_dialog_open = false;
        }
    }

    pub(crate) fn draw_calc_progress_modal(&self, ctx: &egui::Context) {
        let Some(task) = &self.calc_task else {
            return;
        };
        if !task.show_progress {
            return;
        }
        let total = task.total.load(std::sync::atomic::Ordering::Relaxed).max(1);
        let done = task.completed.load(std::sync::atomic::Ordering::Relaxed).min(total);
        let frac = (done as f32 / total as f32).clamp(0.0, 1.0);
        egui::Window::new(&task.title)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.label(format!("Calculating... {done}/{total} ({:.0}%)", frac * 100.0));
                ui.add(egui::ProgressBar::new(frac).animate(true).desired_width(280.0));
            });
    }
}

fn open_file_in_default_app(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn()?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        let _ = path;
        Ok(())
    }
}
