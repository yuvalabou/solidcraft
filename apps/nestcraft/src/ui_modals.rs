//! Draggable modal dialogs and drag-and-drop overlay.

use crate::app::*;

use nestcraft_core::model::{
    BedOrigin, InstanceId, MachineProfile,
};
use nestcraft_core::sinterbox::{self};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::BackingTooltipExt as _;
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

}
