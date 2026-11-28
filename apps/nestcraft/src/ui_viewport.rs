//! Interactive 3D viewport rendering and pointer/handle interactions.

use crate::app::*;
use crate::viewport;

use nestcraft_core::model::{
    GroupId, InstanceId,
    RotationConstraint,
};
use solidcraft_theme::Tokens;
use solidcraft_ui_kit::container::{BackingTooltipExt as _, Container, Surface};
use solidcraft_ui_kit::view_compass::{self, Compass, ViewAction, ViewAxes};
use solidcraft_ui_kit::icon_label_button;
use solidcraft_viewport::vec::Vec3Ops as _;
use solidcraft_viewport_egui::{ViewportCallback, paint_background};
use solidcraft_viewport_gpu::view::{Frame, GpuScene};


impl NestCraftApp {
    pub(crate) fn start_instance_drag(
        &mut self,
        anchor_id: InstanceId,
        rect: egui::Rect,
        pos: egui::Pos2,
        anchor_pos: [f32; 3],
    ) {
        let floor_z = self.job.min_z_mm.max(0.0);
        let mut min_allowed_dz = f32::NEG_INFINITY;
        let mut drag_instances = Vec::new();
        for i in &self.job.instances {
            if self.selected_set.contains(&i.id) && !i.locked {
                drag_instances.push((i.id, i.translation()));
                if let Some(proto) = self.job.part(i.part_id) {
                    let cur_min_z = i.world_bounds(proto.mesh.bounds)[0][2];
                    min_allowed_dz = min_allowed_dz.max(floor_z - cur_min_z);
                }
            }
        }
        if !drag_instances.is_empty()
            && let Some(hit_pt) = viewport::intersect_camera_plane(&self.camera, rect, pos, anchor_pos)
        {
            self.push_undo();
            let drag_ids: Vec<InstanceId> = drag_instances.iter().map(|(id, _)| *id).collect();
            self.drag_state = Some(DragState {
                anchor_id,
                instances: drag_instances,
                start_hit: hit_pt,
                plane_z: anchor_pos[2],
                min_allowed_dz,
            });
            self.start_moving_layer(viewport::DragExclude::Instances(drag_ids));
        }
    }

    pub(crate) fn draw_viewport(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, t: &Tokens) {
        let central = Container::new(ctx, Surface::Viewport).central();
        central.show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            self.viewport_aspect = f64::from(rect.width() / rect.height().max(1.0));
            let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());

            let compass_centre = egui::pos2(rect.right() - 62.0, rect.top() + 62.0);
            let home_centre = egui::pos2(compass_centre.x - 37.0, compass_centre.y - 37.0);
            let over_compass = resp
                .hover_pos()
                .is_some_and(|p| p.distance(compass_centre) <= 52.0 || p.distance(home_centre) <= 14.0);
            let over_view_bar = resp.hover_pos().is_some_and(|p| {
                self.view_bar_rect.is_some_and(|r| r.contains(p))
                    || self.repair_badge_rect.is_some_and(|r| r.contains(p))
            });

            if self.selected_set.is_empty() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.rotate_gizmo_open = false;
            }
            let z_only_gizmo = !self.selected_set.is_empty()
                && self
                    .job
                    .instances
                    .iter()
                    .filter(|i| {
                        self.selected_set.contains(&i.id)
                            && !i.locked
                            && i.constraint != RotationConstraint::Fixed
                    })
                    .all(|i| i.constraint == RotationConstraint::ZOnly);
            let gizmo = if self.rotate_gizmo_open && self.align_face_target.is_none() && !self.zoom_rect_mode {
                viewport::project_rotation_gizmo(&self.job, &self.selected_set, &self.camera, rect, z_only_gizmo)
            } else {
                None
            };
            let centerpoints = viewport::selected_centerpoints(&self.job, &self.selected_set, &self.camera, rect);
            let rot_handle = viewport::selected_rotation_handle(&self.job, &self.selected_set, &self.camera, rect);
            let pointer_hover = resp.hover_pos().filter(|_| !over_compass && !over_view_bar && self.align_face_target.is_none() && !self.zoom_rect_mode);
            let hovered_rot = rot_handle.filter(|(p, _)| pointer_hover.is_some_and(|hp| hp.distance(*p) <= 12.0));
            let hovered_ring_axis: Option<usize> = if hovered_rot.is_none()
                && let (Some(g), Some(hp)) = (&gizmo, pointer_hover)
            {
                g.rings
                    .iter()
                    .filter_map(|ring| {
                        let min_d = ring
                            .screen_points
                            .windows(2)
                            .map(|w| point_segment_distance(hp, w[0], w[1]))
                            .fold(f32::INFINITY, f32::min);
                        (min_d <= 8.0).then_some((ring.axis, min_d))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(axis, _)| axis)
            } else {
                None
            };
            let hovered_center = if hovered_rot.is_none() && hovered_ring_axis.is_none() {
                centerpoints
                    .iter()
                    .find(|c| pointer_hover.is_some_and(|hp| hp.distance(c.screen_pos) <= 9.0))
                    .copied()
            } else {
                None
            };
            if self.drag_state.is_some() || self.rotate_drag.is_some() {
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if hovered_rot.is_some() || hovered_ring_axis.is_some() || hovered_center.is_some() {
                ctx.set_cursor_icon(egui::CursorIcon::Grab);
            }

            if !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && self.zoom_rect_mode
                && resp.drag_started_by(egui::PointerButton::Secondary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                self.viewport_ctx_menu = None;
                self.zoom_rect = Some((pos, pos));
            }

            if resp.dragged_by(egui::PointerButton::Middle) {
                let delta = resp.drag_delta();
                self.camera.pan(f64::from(delta.x), f64::from(delta.y), f64::from(rect.height().max(1.0)));
            } else if resp.dragged_by(egui::PointerButton::Secondary) {
                self.viewport_ctx_menu = None;
                if let Some((_, cur)) = &mut self.zoom_rect {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        *cur = pos;
                    }
                } else if !self.zoom_rect_mode {
                    let delta = resp.drag_delta();
                    if ui.input(|i| i.modifiers.shift) {
                        self.camera.pan(f64::from(delta.x), f64::from(delta.y), f64::from(rect.height().max(1.0)));
                    } else {
                        self.camera.orbit_unlimited_through_roll(f64::from(-delta.x) * 0.75, f64::from(delta.y) * 0.75);
                    }
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

            let editing_locked = self.is_nest3d_running();
            if !editing_locked
                && !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && resp.drag_started_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                if let (Some(axis), Some(g)) = (hovered_ring_axis, &gizmo) {
                    let rot_instances: Vec<(InstanceId, [[f32; 4]; 3])> = self
                        .job
                        .instances
                        .iter()
                        .filter(|i| {
                            self.selected_set.contains(&i.id)
                                && !i.locked
                                && i.constraint != RotationConstraint::Fixed
                                && (i.constraint != RotationConstraint::ZOnly || axis == 2)
                        })
                        .map(|i| (i.id, i.transform))
                        .collect();
                    if !rot_instances.is_empty() {
                        self.push_undo();
                        let rot_ids: Vec<InstanceId> = rot_instances.iter().map(|(id, _)| *id).collect();
                        let edge_tangent = g
                            .rings
                            .iter()
                            .find(|r| r.axis == axis)
                            .and_then(|r| {
                                let mut best = None;
                                let mut best_d = f32::INFINITY;
                                for w in r.screen_points.windows(2) {
                                    let mid = egui::pos2((w[0].x + w[1].x) * 0.5, (w[0].y + w[1].y) * 0.5);
                                    let d = pos.distance(mid);
                                    if d < best_d {
                                        best_d = d;
                                        let v = w[1] - w[0];
                                        let len = v.length();
                                        if len > 1e-4 {
                                            best = Some(v / len);
                                        }
                                    }
                                }
                                best
                            })
                            .unwrap_or(egui::vec2(1.0, 0.0));
                        self.rotate_drag = Some(RotateDragState {
                            pivot: g.pivot,
                            pivot_screen: g.pivot_screen,
                            axis,
                            angle_rad: 0.0,
                            prev_pos: pos,
                            edge_tangent,
                            instances: rot_instances,
                        });
                        self.start_moving_layer(viewport::DragExclude::Instances(rot_ids));
                    }
                } else if hovered_rot.is_some() {
                    // Clicking the rotation toggle handle is handled on click, not drag.
                } else if let Some(ch) = hovered_center {
                    self.start_instance_drag(ch.id, rect, pos, ch.world_pos);
                } else {
                    let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                    if let Some(iid) = hit {
                        if !self.selected_set.contains(&iid) {
                            self.select_instance(Some(iid));
                        }
                        if let Some(inst) = self.job.instances.iter().find(|i| i.id == iid)
                            && !inst.locked
                        {
                            let tr = inst.translation();
                            self.start_instance_drag(iid, rect, pos, tr);
                        }
                    } else if viewport::pick_no_build_zone(&self.job, &self.camera, rect, pos) {
                        if let Some(zone) = &self.job.no_build_zone
                            && !zone.locked
                        {
                            let zone_pos = zone.position;
                            let plane_z = zone_pos[2];
                            if let Some(hit_pt) = viewport::intersect_camera_plane(&self.camera, rect, pos, zone_pos) {
                                self.push_undo();
                                self.drag_zone = Some(DragZoneState {
                                    start_position: zone_pos,
                                    start_hit: hit_pt,
                                    plane_z,
                                });
                                self.start_moving_layer(viewport::DragExclude::NoBuildZone);
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
            }

            if resp.dragged_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                if let Some(rd) = &mut self.rotate_drag {
                    let w_axis = world_axis(rd.axis);
                    let (_, _, cam_back) = self.camera.basis();
                    let cb = cam_back.to_f32();
                    let view_dot = cb[0] * w_axis[0] + cb[1] * w_axis[1] + cb[2] * w_axis[2];
                    let d_angle = if view_dot.abs() < 0.15 {
                        let delta = pos - rd.prev_pos;
                        delta.dot(rd.edge_tangent) * 0.012
                    } else {
                        let v0 = rd.prev_pos - rd.pivot_screen;
                        let v1 = pos - rd.pivot_screen;
                        if v0.length_sq() > 16.0 && v1.length_sq() > 16.0 {
                            let a0 = v0.y.atan2(v0.x);
                            let a1 = v1.y.atan2(v1.x);
                            let mut da = a1 - a0;
                            while da > std::f32::consts::PI {
                                da -= std::f32::consts::TAU;
                            }
                            while da < -std::f32::consts::PI {
                                da += std::f32::consts::TAU;
                            }
                            if view_dot > 0.0 { -da } else { da }
                        } else {
                            0.0
                        }
                    };
                    rd.prev_pos = pos;
                    rd.angle_rad += d_angle;
                    let pivot = rd.pivot;
                    let angle_rad = rd.angle_rad;
                    if rd.instances.len() > 8 {
                        let by_id: std::collections::HashMap<InstanceId, [[f32; 4]; 3]> =
                            rd.instances.iter().copied().collect();
                        for inst in &mut self.job.instances {
                            if let Some(orig) = by_id.get(&inst.id) {
                                inst.transform = rotate_transform_around_pivot(*orig, pivot, w_axis, angle_rad);
                                self.euler_edit.remove(&inst.id);
                            }
                        }
                    } else {
                        for (id, orig) in &rd.instances {
                            if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == *id) {
                                inst.transform = rotate_transform_around_pivot(*orig, pivot, w_axis, angle_rad);
                                self.euler_edit.remove(id);
                            }
                        }
                    }
                    if self.collision_check_active && d_angle.abs() > 1e-5 {
                        self.update_live_drag_collision();
                    }
                } else if let Some(bs) = &mut self.box_select {
                    bs.current = pos;
                } else if let Some(dz) = self.drag_zone {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if shift {
                        if let Some(z_now) = viewport::intersect_vertical_plane(&self.camera, rect, pos, dz.start_hit)
                            && let Some(zone) = &mut self.job.no_build_zone
                        {
                            zone.position[2] = dz.start_position[2] + (z_now - dz.plane_z);
                        }
                    } else if let Some(hit_now) = viewport::intersect_camera_plane(&self.camera, rect, pos, dz.start_hit)
                        && let Some(zone) = &mut self.job.no_build_zone
                    {
                        zone.position[0] = dz.start_position[0] + (hit_now[0] - dz.start_hit[0]);
                        zone.position[1] = dz.start_position[1] + (hit_now[1] - dz.start_hit[1]);
                        zone.position[2] = dz.start_position[2] + (hit_now[2] - dz.start_hit[2]);
                    }
                } else if let Some(ds) = &self.drag_state {
                    let shift = ui.input(|i| i.modifiers.shift);
                    let raw_delta = if shift {
                        viewport::intersect_vertical_plane(&self.camera, rect, pos, ds.start_hit)
                            .map(|z_now| [0.0, 0.0, z_now - ds.plane_z])
                    } else {
                        viewport::intersect_camera_plane(&self.camera, rect, pos, ds.start_hit).map(|hit_now| {
                            [
                                hit_now[0] - ds.start_hit[0],
                                hit_now[1] - ds.start_hit[1],
                                hit_now[2] - ds.start_hit[2],
                            ]
                        })
                    };
                    if let Some(mut delta) = raw_delta {
                        if ds.min_allowed_dz.is_finite() {
                            delta[2] = delta[2].max(ds.min_allowed_dz);
                        }
                        if ds.instances.len() > 8 {
                            let by_id: std::collections::HashMap<InstanceId, [f32; 3]> =
                                ds.instances.iter().copied().collect();
                            for inst in &mut self.job.instances {
                                if let Some(start_tr) = by_id.get(&inst.id) {
                                    inst.set_translation([
                                        start_tr[0] + delta[0],
                                        start_tr[1] + delta[1],
                                        start_tr[2] + delta[2],
                                    ]);
                                }
                            }
                        } else {
                            for (id, start_tr) in &ds.instances {
                                if let Some(inst) = self.job.instances.iter_mut().find(|i| i.id == *id) {
                                    inst.set_translation([
                                        start_tr[0] + delta[0],
                                        start_tr[1] + delta[1],
                                        start_tr[2] + delta[2],
                                    ]);
                                }
                            }
                        }
                        if self.collision_check_active && resp.drag_delta().length_sq() > 0.01 {
                            self.update_live_drag_collision();
                        }
                    }
                }
            }

            if resp.drag_stopped() {
                let had_part_drag = self.drag_state.is_some() || self.rotate_drag.is_some();
                if self.rotate_drag.take().is_some() {
                    self.mark_dirty();
                    self.status = "Rotated selected part(s)".to_owned();
                }
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
                if let Some((start, end)) = self.zoom_rect.take() {
                    let sel_rect = egui::Rect::from_two_pos(start, end);
                    if sel_rect.width() > 4.0 && sel_rect.height() > 4.0 {
                        self.zoom_to_screen_rect(rect, sel_rect);
                        self.zoom_rect_mode = false;
                    }
                }
                if self.drag_zone.take().is_some() {
                    self.persist_no_build_zone();
                }
                self.drag_state = None;
                let had_moving = self.moving_target.take().is_some();
                if had_moving {
                    self.mark_dirty();
                }
                if had_part_drag {
                    if self.collision_check_active {
                        if self.collision_cache.is_initialized()
                            && self.collision_cache.diff(&self.job, COLLISION_VOXEL_PITCH_MM)
                                == nestcraft_core::collision::CollisionDiff::NoChange
                        {
                            let rep = self.collision_cache.report();
                            self.apply_collision_report(&rep, true);
                        } else if !had_moving {
                            self.refresh_collision_check();
                        }
                    } else {
                        self.clear_collision_highlight();
                    }
                }
            }

            if !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && self.align_face_target.is_some()
                && let Some(pos) = resp.hover_pos()
            {
                if let Some(pick) = viewport::pick_face(&self.job, &self.camera, rect, pos, self.align_face_target) {
                    if self.hovered_face != Some((pick.instance_id, pick.tri_index)) {
                        let hl = viewport::build_face_highlight_scene(&pick, t);
                        if let Ok(mut guard) = self.hl_slot.lock() {
                            *guard = Some(hl);
                        }
                        self.hl_key = self.hl_key.wrapping_add(1).max(1);
                        self.hovered_face = Some((pick.instance_id, pick.tri_index));
                    }
                } else if self.hovered_face.is_some() {
                    if let Ok(mut guard) = self.hl_slot.lock() {
                        *guard = Some(GpuScene::default());
                    }
                    self.hl_key = self.hl_key.wrapping_add(1).max(1);
                    self.hovered_face = None;
                }
            }

            if !editing_locked
                && !over_compass
                && !over_view_bar
                && self.compass_drag.at.is_none()
                && resp.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = resp.interact_pointer_pos()
            {
                self.viewport_ctx_menu = None;
                if let Some(target_id) = self.align_face_target {
                    if let Some(pick) = viewport::pick_face(&self.job, &self.camera, rect, pos, Some(target_id)) {
                        self.push_undo();
                        self.job.align_instance_face_to_neg_z(pick.instance_id, pick.local_normal);
                        self.job.clamp_instance_above_floor(pick.instance_id);
                        self.euler_edit.remove(&pick.instance_id);
                        self.mark_dirty();
                        self.clear_align_face_mode();
                        self.status = "Aligned selected face to -Z".to_owned();
                    }
                } else if hovered_rot.is_some() {
                    self.rotate_gizmo_open = !self.rotate_gizmo_open;
                    self.status = if self.rotate_gizmo_open {
                        "Rotation gizmo opened (drag X/Y/Z ring, Esc or click handle to close)".to_owned()
                    } else {
                        "Rotation gizmo closed".to_owned()
                    };
                } else {
                    let hit = viewport::pick_instance(&self.job, &self.camera, rect, pos);
                    if ui.input(|i| i.modifiers.command) {
                        if let Some(iid) = hit {
                            self.toggle_select_instance(iid);
                        }
                    } else {
                        self.select_instance(hit);
                    }
                }
            }

            if !editing_locked
                && !over_compass
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
                let mut open_scale = false;
                let mut open_sinterbox = false;
                let mut open_dup = false;
                let mut drop_floor = false;
                let mut center_xy = false;
                let mut rot_z = false;
                let mut toggle_hide = false;
                let mut toggle_transparent = false;
                let mut toggle_lock = false;
                let mut color_choice: Option<ColorMenuChoice> = None;
                let mut group_sel = false;
                let mut ungroup_sel = false;
                let mut assign_group: Option<GroupId> = None;
                let mut align_face = false;
                let mut shells_to_parts = false;
                let mut merge_parts = false;
                let mut export_sel = false;
                let mut delete_sel = false;

                let any_in_group = self
                    .job
                    .instances
                    .iter()
                    .any(|i| self.selected_set.contains(&i.id) && i.group_id.is_some());

                let is_hidden = self
                    .selected
                    .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
                    .is_some_and(|i| i.hidden);
                let is_transparent = self
                    .selected
                    .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
                    .is_some_and(|i| i.transparent);
                let is_locked = self
                    .selected
                    .and_then(|id| self.job.instances.iter().find(|i| i.id == id))
                    .is_some_and(|i| i.locked);

                resp.context_menu(|ui| {
                    if ui.button("Move... (M)").clicked() {
                        open_move = true;
                        ui.close();
                    }
                    if ui.button("Scale... (Ctrl+S)").clicked() {
                        open_scale = true;
                        ui.close();
                    }
                    if ui.button("Duplicate... (Ctrl+D)").clicked() {
                        open_dup = true;
                        ui.close();
                    }
                    if ui.button("Drop to Min Z (Ctrl+Down)").clicked() {
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
                    if ui
                        .button(if is_transparent {
                            "Opaque"
                        } else {
                            "Transparent (50%)"
                        })
                        .clicked()
                    {
                        toggle_transparent = true;
                        ui.close();
                    }
                    if ui.button(if is_locked { "Unlock (L)" } else { "Lock (L)" }).clicked() {
                        toggle_lock = true;
                        ui.close();
                    }
                    if let Some(choice) = show_color_submenu(ui) {
                        color_choice = Some(choice);
                    }
                    if ui.button("Group Selected").clicked() {
                        group_sel = true;
                        ui.close();
                    }
                    if any_in_group && ui.button("Ungroup").clicked() {
                        ungroup_sel = true;
                        ui.close();
                    }
                    if !self.job.groups.is_empty() {
                        ui.menu_button("Assign to Group", |ui| {
                            for g in &self.job.groups {
                                if ui.button(&g.name).clicked() {
                                    assign_group = Some(g.id);
                                    ui.close();
                                }
                            }
                        });
                    }
                    ui.separator();
                    if icon_label_button(ui, "Align Face to Z", "sls_orient_lock", true, false).clicked() {
                        align_face = true;
                        ui.close();
                    }
                    if ui.button("Shells to Parts").clicked() {
                        shells_to_parts = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.selected_set.len() >= 2,
                            egui::Button::new("Merge Selected Parts"),
                        )
                        .clicked()
                    {
                        merge_parts = true;
                        ui.close();
                    }
                    if icon_label_button(ui, "Create Sinterbox...", "sls_sinterbox", true, false).clicked() {
                        open_sinterbox = true;
                        ui.close();
                    }
                    if ui.button(export_part_menu_label(self.selected_set.len())).clicked() {
                        export_sel = true;
                        ui.close();
                    }
                    if ui.button("Delete (Del)").clicked() {
                        delete_sel = true;
                        ui.close();
                    }
                });

                if open_move {
                    self.viewport_ctx_menu = None;
                    self.open_move_dialog();
                } else if open_scale {
                    self.viewport_ctx_menu = None;
                    self.open_scale_dialog();
                } else if open_sinterbox {
                    self.viewport_ctx_menu = None;
                    self.open_sinterbox_dialog();
                } else if open_dup {
                    self.viewport_ctx_menu = None;
                    self.open_duplicate_dialog();
                } else if drop_floor {
                    self.viewport_ctx_menu = None;
                    self.push_undo();
                    let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                    for id in ids {
                        self.job.drop_instance_to_floor(id);
                    }
                    self.mark_dirty();
                    self.status = "Dropped selection to Min Z".to_owned();
                } else if center_xy {
                    self.viewport_ctx_menu = None;
                    if let Some(sel) = self.selected {
                        self.push_undo();
                        if self.job.center_instance_xy(sel) {
                            self.mark_dirty();
                            self.status = "Centered part in XY".to_owned();
                        } else {
                            self.undo_stack.pop();
                        }
                    }
                } else if rot_z {
                    self.viewport_ctx_menu = None;
                    self.rotate_selected_90(2);
                } else if toggle_hide {
                    self.viewport_ctx_menu = None;
                    self.toggle_selected_hidden();
                } else if toggle_transparent {
                    self.viewport_ctx_menu = None;
                    self.toggle_selected_transparent();
                } else if toggle_lock {
                    self.viewport_ctx_menu = None;
                    self.toggle_selected_locked();
                } else if let Some(choice) = color_choice {
                    self.viewport_ctx_menu = None;
                    match choice {
                        ColorMenuChoice::OpenPicker => self.open_color_picker(),
                        ColorMenuChoice::Set(c) => self.set_selected_color(c),
                    }
                } else if group_sel {
                    self.viewport_ctx_menu = None;
                    self.group_selected_parts(String::new());
                } else if ungroup_sel {
                    self.viewport_ctx_menu = None;
                    self.ungroup_selected_parts();
                } else if let Some(gid) = assign_group {
                    self.viewport_ctx_menu = None;
                    self.push_undo();
                    let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                    for id in ids {
                        let _ = self.job.set_instance_group(id, Some(gid));
                    }
                    self.mark_dirty();
                } else if align_face {
                    self.viewport_ctx_menu = None;
                    if let Some(sel) = self.selected {
                        self.start_align_face_mode(sel);
                    }
                } else if shells_to_parts {
                    self.viewport_ctx_menu = None;
                    let ids: Vec<InstanceId> = self.selected_set.iter().copied().collect();
                    self.shells_to_parts_for(ids);
                } else if merge_parts {
                    self.viewport_ctx_menu = None;
                    self.merge_selected_parts();
                } else if export_sel {
                    self.viewport_ctx_menu = None;
                    self.export_stl_selected_only = true;
                    self.export_stl_dialog_open = true;
                } else if delete_sel {
                    self.viewport_ctx_menu = None;
                    self.delete_selected();
                }
            }

            self.sync_scene_if_dirty();

            let painter = ui.painter_at(rect);
            paint_background(&painter, rect, t.viewport_top, t.viewport_middle, t.viewport_bottom);

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
                hl_key: self.hl_key,
                hl_slot: self.hl_slot.clone(),
                hover_body: None,
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
                selection_all: false,
                cap: [0.0; 4],
                images: Vec::new(),
                clouds: Vec::new(),
                analysis: [0.0; 8],
                analysis_colors: [0.0; 36],
                access: None,
                outline: false,
                eye: [e[0], e[1], e[2], if self.camera.fov > 0.0 { 1.0 } else { 0.0 }],
                ao_radius: 0.0,
                ao_samples: 0,
                edge_px: 0.0,
                occlusion: false,
                ssaa: 1.0,
                hover_tint: [0.08, 0.08, 0.08],
                selection_tint: [0.93, 0.70, 0.22],
                hatch: [[0.5, 0.5, 0.5]; solidcraft_viewport::hatch::HATCH_COLOURS],
                sky: [0.0, 0.0, 1.0, 1.0],
                moving: self.current_moving_layer(),
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
            if let Some((start, end)) = self.zoom_rect {
                let sel_rect = egui::Rect::from_two_pos(start, end);
                painter.rect(
                    sel_rect,
                    0.0,
                    t.accent.gamma_multiply(0.14),
                    egui::Stroke::new(1.5, t.accent),
                    egui::StrokeKind::Inside,
                );
            }

            if let Some(g) = &gizmo {
                let p = ui.painter_at(rect);
                let axis_colors = [
                    egui::Color32::from_rgb(0xE0, 0x52, 0x52), // X: red
                    egui::Color32::from_rgb(0x4C, 0xAF, 0x50), // Y: green
                    egui::Color32::from_rgb(0x4A, 0x90, 0xE2), // Z: blue
                ];
                let axis_names = ["X", "Y", "Z"];
                for ring in &g.rings {
                    let color = axis_colors[ring.axis.min(2)];
                    let is_active = self.rotate_drag.as_ref().is_some_and(|rd| rd.axis == ring.axis)
                        || hovered_ring_axis == Some(ring.axis);
                    let stroke_w = if is_active { 3.2 } else { 1.8 };
                    let stroke_col = if is_active { color } else { color.gamma_multiply(0.85) };
                    if ring.screen_points.len() >= 2 {
                        p.add(egui::Shape::line(
                            ring.screen_points.clone(),
                            egui::Stroke::new(stroke_w, stroke_col),
                        ));
                    }
                    if let Some(tip) = ring.axis_tip {
                        p.line_segment([g.pivot_screen, tip], egui::Stroke::new(1.5, color.gamma_multiply(0.8)));
                        p.text(
                            tip + egui::vec2(5.0, -5.0),
                            egui::Align2::LEFT_BOTTOM,
                            axis_names[ring.axis.min(2)],
                            egui::FontId::proportional(11.5),
                            color,
                        );
                    }
                }
                if let Some(rd) = &self.rotate_drag {
                    let label = format!("{:+.1}° {}", rd.angle_rad.to_degrees(), axis_names[rd.axis.min(2)]);
                    let badge_pos = rd.pivot_screen + egui::vec2(0.0, -24.0);
                    let galley = p.layout_no_wrap(label, egui::FontId::monospace(12.0), t.text);
                    let badge_rect = egui::Rect::from_center_size(badge_pos, galley.size() + egui::vec2(12.0, 6.0));
                    p.rect_filled(badge_rect, 4.0, t.panel.gamma_multiply(0.92));
                    p.rect_stroke(badge_rect, 4.0, egui::Stroke::new(1.0, axis_colors[rd.axis.min(2)]), egui::StrokeKind::Inside);
                    p.galley(badge_rect.min + egui::vec2(6.0, 3.0), galley, t.text);
                }
            }

            if !centerpoints.is_empty() || rot_handle.is_some() {
                let p = ui.painter_at(rect);
                for ch in &centerpoints {
                    let is_hovered = hovered_center.is_some_and(|h| h.id == ch.id);
                    let fill = if ch.valid { t.accent } else { t.error };
                    let size = if is_hovered { 9.0 } else { 7.0 };
                    p.rect(
                        egui::Rect::from_center_size(ch.screen_pos, egui::vec2(size, size)),
                        1.0,
                        fill,
                        egui::Stroke::new(1.5, t.panel),
                        egui::StrokeKind::Outside,
                    );
                }
                if let Some((rot_pos, _)) = rot_handle {
                    let active = self.rotate_gizmo_open || self.rotate_drag.is_some() || hovered_rot.is_some();
                    let r = if active { 10.5 } else { 9.0 };
                    let bg = if active { t.accent } else { t.panel.gamma_multiply(0.92) };
                    let fg = if active { t.selected_ink } else { t.accent };
                    p.circle(rot_pos, r, bg, egui::Stroke::new(1.5, t.accent));
                    // Draw a curved rotation arc + arrowhead inside the handle.
                    let arc_r = r * 0.54;
                    let steps = 12usize;
                    let start_a = -0.35_f32;
                    let end_a = 4.05_f32;
                    let pts: Vec<egui::Pos2> = (0..=steps)
                        .map(|i| {
                            let a = start_a + (end_a - start_a) * (i as f32 / steps as f32);
                            egui::pos2(rot_pos.x + arc_r * a.cos(), rot_pos.y + arc_r * a.sin())
                        })
                        .collect();
                    p.add(egui::Shape::line(pts, egui::Stroke::new(1.6, fg)));
                    let tip = egui::pos2(rot_pos.x + arc_r * end_a.cos(), rot_pos.y + arc_r * end_a.sin());
                    let tang = egui::vec2(-end_a.sin(), end_a.cos());
                    let rad = egui::vec2(end_a.cos(), end_a.sin());
                    p.add(egui::Shape::convex_polygon(
                        vec![tip + tang * 2.5, tip - tang * 1.8 + rad * 2.6, tip - tang * 1.8 - rad * 2.4],
                        fg,
                        egui::Stroke::NONE,
                    ));
                }
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
                    turn_around_label: None,
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
                        self.camera = self.default_camera_for_job();
                    }
                    Some(ViewAction::TurnAround) | None => {}
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

            let faulty_ids = self.faulty_placed_instance_ids();
            if !faulty_ids.is_empty() && rect.width() >= 180.0 && rect.height() >= 120.0 {
                let badge_max_rect = egui::Rect::from_min_max(
                    rect.right_bottom() - egui::vec2(220.0, 52.0),
                    rect.right_bottom() - egui::vec2(12.0, 12.0),
                );
                let badge_resp = ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(badge_max_rect)
                        .layout(egui::Layout::right_to_left(egui::Align::Max)),
                    |ui| {
                        let frame_resp = egui::Frame::new()
                            .fill(t.panel.gamma_multiply(0.94))
                            .stroke(egui::Stroke::new(1.25, t.error))
                            .corner_radius(t.radius_sm)
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (icon_rect, _) =
                                        ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::hover());
                                    let p = ui.painter();
                                    let tri = vec![
                                        egui::pos2(icon_rect.center().x, icon_rect.top() + 1.0),
                                        egui::pos2(icon_rect.right() - 1.0, icon_rect.bottom() - 1.0),
                                        egui::pos2(icon_rect.left() + 1.0, icon_rect.bottom() - 1.0),
                                    ];
                                    p.add(egui::Shape::convex_polygon(
                                        tri,
                                        t.error,
                                        egui::Stroke::new(1.0, t.error),
                                    ));
                                    p.text(
                                        icon_rect.center() + egui::vec2(0.0, 1.5),
                                        egui::Align2::CENTER_CENTER,
                                        "!",
                                        egui::FontId::proportional(11.0),
                                        t.selected_ink,
                                    );
                                    ui.label(
                                        egui::RichText::new(format!("{} mesh error(s)", faulty_ids.len()))
                                            .small()
                                            .strong()
                                            .color(t.error),
                                    );
                                });
                            });
                        let click_resp = ui.interact(
                            frame_resp.response.rect,
                            ui.scope_id().with("sls_repair_gate_badge"),
                            egui::Sense::click(),
                        );
                        let tip = format!(
                            "{} placed part(s) have non-manifold, open-edge, or winding errors. Click to select faulty part(s).",
                            faulty_ids.len()
                        );
                        if click_resp.backing_on_hover_text(tip).clicked() {
                            self.selected_set.clear();
                            for &id in &faulty_ids {
                                self.selected_set.insert(id);
                            }
                            self.selected = faulty_ids.last().copied();
                            self.selection_anchor = self.selected;
                            self.status = format!("Selected {} part(s) with mesh errors", faulty_ids.len());
                        }
                        frame_resp.response
                    },
                );
                self.repair_badge_rect = Some(badge_resp.inner.rect);
            } else {
                self.repair_badge_rect = None;
            }
        });
    }
}

/// Shortest Euclidean distance from point `p` to 2D line segment `a`–`b`.
fn point_segment_distance(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let len_sq = ab.length_sq();
    if len_sq <= 1e-6 {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}
