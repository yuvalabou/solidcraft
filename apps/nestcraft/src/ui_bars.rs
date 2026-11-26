//! Top, Edit, Analyze, View, and Status bars.

use crate::app::*;

use nestcraft_core::model::InstanceId;
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Edge, Surface};
use solidcraft_ui_kit::{
    Button, Segments, StageBar, button,
    icon_label_button, scope, segmented,
};
use solidcraft_viewport::vec::Bounds;


impl NestCraftApp {
    pub(crate) fn draw_top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let chrome = Container::new(ctx, Surface::Chrome);
        let top_panel = chrome.edge("sls_top_bar", Edge::Top).exact_size(44.0).resizable(false);
        chrome.show_edge(ui, top_panel, chrome.edge_frame(t.toolbar), |ui| {
            scope(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("sls_top_scroll")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        let running_3d = self.is_nest3d_running();
                        ui.horizontal_centered(|ui| {
                            ui.label(egui::RichText::new("NestCraft").strong().color(t.text));
                            StageBar::separator(ui);

                            let mut machine_idx = self.active_machine_slot.min(MACHINE_SLOT_COUNT - 1);
                            let machine_changed = ui
                                .add_enabled_ui(!running_3d, |ui| {
                                    ui.push_id("machine_seg", |ui| {
                                        segmented(
                                            ui,
                                            &mut machine_idx,
                                            &["P110", "P110 Velocis", "P100", "HT403P", "SS403P", "Custom"],
                                            Segments {
                                                dense: true,
                                                intrinsic: true,
                                                workspace_accent: true,
                                                ..Default::default()
                                            },
                                        )
                                    })
                                    .inner
                                })
                                .inner;
                            if machine_changed {
                                self.apply_machine_slot(machine_idx);
                                if machine_idx == CUSTOM_MACHINE_SLOT {
                                    self.open_custom_platform_dialog();
                                }
                            }
                            if ui
                                .add_enabled_ui(!running_3d, |ui| {
                                    button(ui, "Configure...", Button::Small).backing_on_hover_text(
                                        "Configure dimensions, wall distance, Min Z, and clearance for the active machine",
                                    )
                                })
                                .inner
                                .clicked()
                            {
                                self.open_custom_platform_dialog();
                            }

                            StageBar::separator(ui);
                            if icon_label_button(ui, "Open Bed", "open", self.load_task.is_none() && !running_3d, false)
                                .backing_on_hover_text("Open a saved .nestc bed project")
                                .clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .add_filter("NestCraft Bed", &["nestc"])
                                    .pick_file()
                            {
                                self.start_open_bed(path, ctx);
                            }
                            if icon_label_button(ui, "Save Bed", "save", self.save_task.is_none() && !running_3d, false)
                                .backing_on_hover_text(
                                    "Save current bed, parts, and settings as .nestc (Ctrl+Shift+S). Quick-saves directly if machine and chamber settings are unchanged.",
                                )
                                .clicked()
                            {
                                self.trigger_save_bed(ctx);
                            }
                            if icon_label_button(ui, "Generate Report", "export", true, self.report_dialog_open)
                                .backing_on_hover_text(
                                    "Generate build report mockup for parts inside the selected machine & material",
                                )
                                .clicked()
                            {
                                self.open_report_dialog();
                            }
                            if icon_label_button(ui, "Import", "sls_import_mesh", self.load_task.is_none() && !running_3d, false)
                                .backing_on_hover_text("Import STL or 3MF meshes into the build chamber")
                                .clicked()
                                && let Some(paths) = rfd::FileDialog::new()
                                    .add_filter("3D Mesh", &["stl", "STL", "3mf", "3MF", "nestc"])
                                    .pick_files()
                            {
                                self.start_import_files(paths, ctx);
                            }
                            if icon_label_button(
                                ui,
                                "Failed Print?",
                                "sls_slice_z",
                                !running_3d,
                                self.failed_print_dialog_open,
                            )
                            .backing_on_hover_text(
                                "Inspect a failed/aborted build by Z height to see completed vs reprint parts",
                            )
                            .clicked()
                            {
                                self.open_failed_print_dialog();
                            }
                            if icon_label_button(ui, "Sample Part", "mesh_body", !running_3d, false)
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
                                    .set_file_name(self.default_save_filename("3mf"))
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
                            if icon_label_button(
                                ui,
                                "Auto-Nest",
                                "sls_auto_nest",
                                !self.job.instances.is_empty() && !running_3d && self.calc_task.is_none(),
                                self.nest3d_dialog_open || running_3d,
                            )
                            .backing_on_hover_text("Configure and run 3D true-shape volumetric auto-nesting")
                            .clicked()
                            {
                                self.open_nest3d_dialog();
                            }
                            if icon_label_button(ui, "Fit View", "fit", true, false)
                                .backing_on_hover_text("Reset camera to default isometric build view")
                                .clicked()
                            {
                                self.camera = self.default_camera_for_job();
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

    pub(crate) fn add_aabb_to_bounds(bounds: &mut Bounds, aabb: [[f32; 3]; 2]) {
        bounds.add([f64::from(aabb[0][0]), f64::from(aabb[0][1]), f64::from(aabb[0][2])].into());
        bounds.add([f64::from(aabb[1][0]), f64::from(aabb[1][1]), f64::from(aabb[1][2])].into());
    }

    pub(crate) fn platform_bounds(&self) -> Bounds {
        let ([x0, y0], [x1, y1]) = self.job.bed_xy_bounds();
        let [_, _, h] = self.job.machine.size_mm();
        let mut bounds = Bounds::EMPTY;
        Self::add_aabb_to_bounds(&mut bounds, [[x0, y0, 0.0], [x1, y1, h.max(1.0)]]);
        bounds
    }

    pub(crate) fn all_parts_bounds(&self) -> Bounds {
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

    pub(crate) fn selected_parts_bounds(&self) -> Bounds {
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

    pub(crate) fn zoom_to_all_parts(&mut self) {
        let bounds = self.all_parts_bounds();
        if self.camera.fit_aspect(&bounds, self.viewport_aspect) {
            self.status = "Zoomed to all parts".to_owned();
        }
    }

    pub(crate) fn zoom_to_selected(&mut self) {
        let bounds = self.selected_parts_bounds();
        if self.camera.fit_aspect(&bounds, self.viewport_aspect) {
            self.status = "Zoomed to selected part(s)".to_owned();
        }
    }

    pub(crate) fn zoom_to_platform(&mut self) {
        let bounds = self.platform_bounds();
        if self.camera.fit_aspect(&bounds, self.viewport_aspect) {
            self.status = "Zoomed to platform".to_owned();
        }
    }

    pub(crate) fn zoom_to_all(&mut self) {
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

    pub(crate) fn zoom_to_screen_rect(&mut self, viewport_rect: egui::Rect, sel_rect: egui::Rect) {
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

    pub(crate) fn draw_view_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
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
                            StageBar::separator(ui);
                            let is_ortho = self.is_orthographic();
                            if icon_label_button(
                                ui,
                                if is_ortho { "Isometric" } else { "Perspective" },
                                "sls_view_ortho",
                                true,
                                is_ortho,
                            )
                            .backing_on_hover_text(
                                "Toggle camera projection between Perspective and Isometric (Orthographic)",
                            )
                            .clicked()
                            {
                                self.toggle_projection();
                            }
                        });
                    });
            });
        });
    }

    pub(crate) fn draw_edit_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let chrome = Container::new(ctx, Surface::Chrome);
        let edit_panel = chrome.edge("sls_edit_bar", Edge::Top).exact_size(36.0).resizable(false);
        let running_3d = self.is_nest3d_running();
        chrome.show_edge(ui, edit_panel, chrome.edge_frame(t.toolbar), |ui| {
            scope(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("sls_edit_scroll")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        ui.add_enabled_ui(!running_3d, |ui| {
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
        });
    }

    pub(crate) fn draw_analyze_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
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

                            if icon_label_button(
                                ui,
                                "Slice Distribution",
                                "sls_slice_z",
                                true,
                                self.slice_dist_open,
                            )
                            .backing_on_hover_text(
                                "Toggle collapsible Slice Layer Distribution sidebar (sintered XY area vs Z height)",
                            )
                            .clicked()
                            {
                                self.slice_dist_open = !self.slice_dist_open;
                                if self.slice_dist_open {
                                    self.start_slice_distribution();
                                }
                            }

                            if icon_label_button(
                                ui,
                                "Thermal Map",
                                "sls_thermal_map",
                                true,
                                self.thermal_map_open,
                            )
                            .backing_on_hover_text(
                                "Toggle 3D sintered powder-bed thermal density map (20×20×20 mm local density)",
                            )
                            .clicked()
                            {
                                self.toggle_thermal_map();
                            }

                            if icon_label_button(
                                ui,
                                "Collision Check",
                                "sls_collision",
                                true,
                                self.collision_check_active,
                            )
                            .backing_on_hover_text(
                                "Detect part-to-part collisions & clearance violations with 3D voxel highlight (Ctrl+K)",
                            )
                            .clicked()
                            {
                                self.toggle_collision_check();
                            }

                            const PLACEHOLDERS: [(&str, &str, &str); 5] = [
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
                            ];

                            for (label, icon, tip) in PLACEHOLDERS {
                                icon_label_button(ui, label, icon, false, false).backing_on_hover_text(tip);
                            }
                        });
                    });
            });
        });
    }

    pub(crate) fn draw_nest_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let chrome = Container::new(ctx, Surface::Chrome);
        let nest_panel = chrome.edge("sls_nest_bar", Edge::Top).exact_size(36.0).resizable(false);
        let running_3d = self.is_nest3d_running();
        let can_run = !self.job.instances.is_empty() && !running_3d && self.calc_task.is_none();
        chrome.show_edge(ui, nest_panel, chrome.edge_frame(t.toolbar), |ui| {
            scope(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("sls_nest_scroll")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.label(egui::RichText::new("Nest").strong().color(t.text));
                            StageBar::separator(ui);

                            if icon_label_button(
                                ui,
                                "2D Nesting",
                                "sls_auto_nest",
                                can_run,
                                self.nest2d_dialog_open,
                            )
                            .backing_on_hover_text(
                                "Configure and run 2D auto-nesting (To Platform or In Place outside platform)",
                            )
                            .clicked()
                            {
                                self.open_nest2d_dialog();
                            }
                            if icon_label_button(
                                ui,
                                "3D Nesting",
                                "sls_auto_nest",
                                can_run,
                                self.nest3d_dialog_open || running_3d,
                            )
                            .backing_on_hover_text(
                                "3D EDT volumetric packing across the build chamber with interlocking protection",
                            )
                            .clicked()
                            {
                                self.open_nest3d_dialog();
                            }
                            if icon_label_button(
                                ui,
                                "Gravity Settle",
                                "sls_auto_nest",
                                can_run,
                                self.gravity_settle_dialog_open,
                            )
                            .backing_on_hover_text(
                                "Compact parts downward under physics gravity (V-HACD) preserving clearance and avoiding interlocking",
                            )
                            .clicked()
                            {
                                self.open_gravity_settle_dialog();
                            }
                        });
                    });
            });
        });
    }

    pub(crate) fn draw_nest3d_progress_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let Some(task) = &self.nest3d_task else {
            return;
        };
        let total = task.latest.total.max(1);
        let done = task.latest.done.min(total);
        let frac = (done as f32 / total as f32).clamp(0.0, 1.0);
        let elapsed_s = task.started_at.elapsed().as_secs_f32();
        let label = task.latest.label.clone();
        let best_max_z = task.latest.best_max_z_mm;
        let best_density = task.latest.best_density_pct;
        let best_placed = task.latest.best_placed;
        let best_overflow = task.latest.best_overflow;
        let stopping = task.stop_and_apply.load(std::sync::atomic::Ordering::Relaxed)
            || task.cancel.load(std::sync::atomic::Ordering::Relaxed);

        let mut stop_apply_clicked = false;
        let mut cancel_clicked = false;
        let chrome = Container::new(ctx, Surface::Chrome);
        let bar_panel = chrome.edge("sls_nest3d_progress", Edge::Bottom).exact_size(36.0).resizable(false);
        chrome.show_edge(ui, bar_panel, chrome.edge_frame(t.toolbar), |ui| {
            scope(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(180.0)
                            .animate(true)
                            .text(label),
                    );
                    StageBar::separator(ui);
                    ui.label(
                        egui::RichText::new(format!(
                            "Max Z: {best_max_z:.1} mm   |   Density: {best_density:.2}%   |   Placed: {best_placed}   |   Overflow: {best_overflow}   |   {elapsed_s:.1}s"
                        ))
                        .size(t.font_small)
                        .color(t.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_enabled_ui(!stopping, |ui| {
                            if button(ui, "Cancel", Button::Small)
                                .backing_on_hover_text("Abort 3D nesting and restore original bed (Esc)")
                                .clicked()
                            {
                                cancel_clicked = true;
                            }
                            if button(ui, "Stop & Apply Best", Button::Primary)
                                .backing_on_hover_text("Stop search early, verify clearance, and commit best layout found")
                                .clicked()
                            {
                                stop_apply_clicked = true;
                            }
                        });
                    });
                });
            });
        });

        if cancel_clicked {
            self.request_nest3d_cancel();
        } else if stop_apply_clicked {
            self.request_nest3d_stop_and_apply();
        }
    }

    pub(crate) fn draw_bottom_bar(&self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
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

}
