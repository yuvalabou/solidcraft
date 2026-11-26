//! Left sidebar: Build Parts Browser and Selected Part configuration.

use crate::app::*;

use nestcraft_core::model::{
    GroupId, InstanceId, PartId,
    RotationConstraint,
};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Edge, Surface};
use solidcraft_ui_kit::{
    Button, Segments, StatusTone, button, catalog_footer_with_tip, checkbox_row,
    icon_label_button, row, scope, section, segmented, status_line, value_row,
};


impl NestCraftApp {
    pub(crate) fn ensure_part_thumbnails(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn draw_parts_browser(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
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
                                self.purge_unused_part_caches();
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
                        let unlocked = !self.is_nest3d_running();
                        ui.add_enabled_ui(unlocked, |ui| {
                            self.draw_selected_part_section(ui, t);
                        });
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


    pub(crate) fn draw_selected_part_section(&mut self, ui: &mut egui::Ui, t: &Tokens) {
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
            let mut inspector_dirty = false;
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

                inspector_dirty = changed;
            }

            if let Some(new_euler) = applied_euler {
                inspector_dirty |= self.job.set_instance_euler_around_center(sel_id, new_euler);
            }
            if drop_floor {
                inspector_dirty |= self.job.drop_instance_to_floor(sel_id);
            }
            if clamp_floor {
                inspector_dirty |= self.job.clamp_instance_above_floor(sel_id);
            }
            if center_xy {
                inspector_dirty |= self.job.center_instance_xy(sel_id);
            }
            if inspector_dirty {
                self.mark_dirty();
            }
            if let Some(axis) = rot_step {
                self.rotate_selected_90(axis);
            }
        });
    }

}
