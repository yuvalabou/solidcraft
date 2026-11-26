use crate::app::*;
use crate::viewport;
use std::collections::HashSet;
use std::sync::Arc;

use nestcraft_core::io::stl;
use nestcraft_core::manifold::{self};
use nestcraft_core::model::{
    IDENTITY_TRANSFORM, NoBuildZone, TriMesh,
};
use solidcraft_viewport::StandardView;

    

    #[test]
    fn export_part_menu_label_pluralizes_for_multi_selection() {
        assert_eq!(export_part_menu_label(0), "Export Part...");
        assert_eq!(export_part_menu_label(1), "Export Part...");
        assert_eq!(export_part_menu_label(2), "Export Parts...");
    }

    #[test]
    fn view_zoom_helpers_frame_parts_platform_and_screen_rect() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        app.job.instances[0].set_translation([10.0, 10.0, 0.0]);

        app.zoom_to_selected();
        let selected_dist = app.camera.distance;

        app.zoom_to_platform();
        let platform_dist = app.camera.distance;
        assert!(platform_dist > selected_dist);

        app.zoom_to_all_parts();
        assert!((app.camera.distance - selected_dist).abs() < 1e-4);

        app.zoom_to_all();
        assert!(app.camera.distance >= platform_dist - 1e-4);

        let viewport = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let half_rect = egui::Rect::from_center_size(viewport.center(), egui::vec2(400.0, 300.0));
        let before_dist = app.camera.distance;
        app.zoom_to_screen_rect(viewport, half_rect);
        assert!((app.camera.distance - before_dist * 0.5).abs() < 1e-4);
        let _ = iid;
    }

    #[test]
    fn app_initializes_and_syncs_scene() {
        let mut app = NestCraftApp::new();
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert!(app.scene_slot.lock().is_ok_and(|g| g.is_some()));
    }

    #[test]
    fn dragging_instance_uses_moving_layer_without_dirtying_scene() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        let start = app.job.instances[0].translation();
        app.sync_scene_if_dirty();
        let key_before = app.scene_key;

        app.drag_state = Some(DragState {
            anchor_id: iid,
            instances: vec![(iid, start)],
            start_hit: start,
            plane_z: start[2],
            min_allowed_dz: f32::NEG_INFINITY,
        });
        app.start_moving_layer(viewport::DragExclude::Instances(vec![iid]));
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert_ne!(app.scene_key, key_before);
        let key_during_drag = app.scene_key;

        // Simulated drag frame: update translation without marking scene dirty.
        app.job.instances[0].set_translation([start[0] + 15.0, start[1] - 5.0, start[2] + 3.0]);
        assert!(!app.scene_dirty);
        app.sync_scene_if_dirty();
        assert_eq!(app.scene_key, key_during_drag);

        let mv = app.current_moving_layer().expect("moving layer active");
        assert_eq!(mv.key, app.moving_key);
        assert!((mv.transform[3][0] - 15.0).abs() < 1e-4);
        assert!((mv.transform[3][1] + 5.0).abs() < 1e-4);
        assert!((mv.transform[3][2] - 3.0).abs() < 1e-4);
    }

    #[test]
    fn sample_part_and_90_deg_rotation_update_scene() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        assert_eq!(app.job.instances.len(), 1);
        let iid = app.selected.expect("selected");
        app.rotate_selected_90(0);
        app.sync_scene_if_dirty();
        assert!(!app.scene_dirty);
        assert!(app.job.drop_instance_to_floor(iid));
        assert!(app.job.is_instance_in_bounds(iid));
    }

    #[test]
    fn shortcut_helpers_nudge_duplicate_hide_lock_and_delete() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        assert!(app.selected.is_some());
        let before = app.job.instances[0].translation();
        app.nudge_selected([5.0, -3.0, 2.0]);
        let after = app.job.instances[0].translation();
        assert!((after[0] - (before[0] + 5.0)).abs() < 1e-4);
        assert!((after[1] - (before[1] - 3.0)).abs() < 1e-4);
        assert!((after[2] - (before[2] + 2.0)).abs() < 1e-4);

        app.toggle_selected_hidden();
        assert!(app.job.instances[0].hidden);
        app.toggle_selected_transparent();
        assert!(app.job.instances[0].transparent);
        app.toggle_selected_transparent();
        assert!(!app.job.instances[0].transparent);
        app.set_selected_color(Some([0x22, 0xAA, 0xFF]));
        assert_eq!(app.job.instances[0].color, Some([0x22, 0xAA, 0xFF]));
        app.set_selected_color(None);
        assert_eq!(app.job.instances[0].color, None);
        app.toggle_selected_locked();
        assert!(app.job.instances[0].locked);

        app.dup_axes = [true, false, false];
        app.dup_counts = [3, 1, 1];
        app.dup_exact_total = false;
        app.duplicate_selected();
        assert_eq!(app.job.instances.len(), 3);
        app.delete_selected();
        assert_eq!(app.job.instances.len(), 2);
    }

    #[test]
    fn import_files_loads_multiple_stls_and_reports_partial_failures() {
        let dir = std::env::temp_dir().join(format!("nestcraft_multi_import_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let ascii_stl = b"solid tri\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 10 0 0\nvertex 0 10 0\nendloop\nendfacet\nendsolid tri\n";
        let stl_a = dir.join("part_a.stl");
        let stl_b = dir.join("part_b.STL");
        let bad = dir.join("notes.txt");
        assert!(std::fs::write(&stl_a, ascii_stl).is_ok());
        assert!(std::fs::write(&stl_b, ascii_stl).is_ok());
        assert!(std::fs::write(&bad, b"not a mesh").is_ok());

        let mut app = NestCraftApp::new();
        app.scene_dirty = false;
        app.import_files(&[&stl_a, &stl_b]);
        assert_eq!(app.job.instances.len(), 2);
        assert!(app.scene_dirty);
        assert!(app.status.contains("Imported 2 item(s) from 2 file(s)"), "status: {}", app.status);

        app.import_files(&[&stl_a, &bad]);
        assert_eq!(app.job.instances.len(), 3);
        assert!(app.status.contains("1 failed"), "status: {}", app.status);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_cycle_and_hint_track_camera_orientation() {
        let mut app = NestCraftApp::new();
        assert_eq!(app.current_standard_view(), Some(StandardView::Iso));

        app.cycle_standard_view();
        assert_eq!(app.current_standard_view(), Some(StandardView::Top));
        app.cycle_standard_view();
        assert_eq!(app.current_standard_view(), Some(StandardView::Bottom));

        app.camera.orbit_unlimited_through_roll(40.0, 25.0);
        assert_eq!(app.current_standard_view(), None);

        app.cycle_standard_view();
        assert_eq!(app.current_standard_view(), Some(StandardView::Iso));
    }

    #[test]
    fn side_views_map_arrows_to_z_and_screen_horizontal() {
        let mut app = NestCraftApp::new();
        assert_eq!(app.arrow_nudge_axes(), ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]));

        app.set_standard_view(StandardView::Front);
        assert_eq!(app.arrow_nudge_axes(), ([1.0, 0.0, 0.0], [0.0, 0.0, 1.0]));

        app.set_standard_view(StandardView::Back);
        assert_eq!(app.arrow_nudge_axes(), ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]));

        app.set_standard_view(StandardView::Right);
        assert_eq!(app.arrow_nudge_axes(), ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0]));

        app.set_standard_view(StandardView::Left);
        assert_eq!(app.arrow_nudge_axes(), ([0.0, -1.0, 0.0], [0.0, 0.0, 1.0]));

        app.set_standard_view(StandardView::Top);
        assert_eq!(app.arrow_nudge_axes(), ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]));
    }

    #[test]
    fn compact_distribution_recalculates_grid_across_enabled_axes() {
        assert_eq!(distribute_compact_grid(6, [true, true, false]), [3, 2, 1]);
        assert_eq!(distribute_compact_grid(12, [true, true, true]), [3, 2, 2]);
        assert_eq!(distribute_compact_grid(5, [false, true, false]), [1, 5, 1]);
    }

    #[test]
    fn ctrl_click_and_shift_click_multi_select_in_tree() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        app.add_sample_part();
        let i3 = app.selected.expect("i3");
        app.add_sample_part();
        let i4 = app.selected.expect("i4");

        let order = vec![i1, i2, i3, i4];
        app.select_instance(Some(i1));
        assert_eq!(app.selected_set.len(), 1);

        // Shift+Click i3 selects range i1..=i3.
        app.select_tree_range(&order, i3);
        assert_eq!(app.selected_set, HashSet::from([i1, i2, i3]));

        // Ctrl+Click toggles i2 off and i4 on.
        app.toggle_select_instance(i2);
        assert_eq!(app.selected_set, HashSet::from([i1, i3]));
        app.toggle_select_instance(i4);
        assert_eq!(app.selected_set, HashSet::from([i1, i3, i4]));

        // Delete removes all selected instances.
        app.delete_selected();
        assert_eq!(app.job.instances.len(), 1);
        assert_eq!(app.job.instances[0].id, i2);
    }

    #[test]
    fn no_build_zone_config_and_stl_roundtrip_in_dir() {
        let dir = std::env::temp_dir().join(format!("nestcraft_nbz_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let ascii_stl = b"solid nbz\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 10 0 0\nvertex 0 10 0\nendloop\nendfacet\nendsolid nbz\n";
        assert!(std::fs::write(dir.join("zone.stl"), ascii_stl).is_ok());

        let mesh = Arc::new(stl::parse_stl(ascii_stl).expect("valid stl"));
        let mut zone = NoBuildZone::new("Recoater Keep-Out", mesh);
        zone.position = [15.0, 20.0, 5.0];
        zone.rotation_deg = [0.0, 0.0, 90.0];
        zone.clearance_mm = 3.5;
        zone.enabled = true;
        zone.visible = false;

        assert!(save_no_build_zone_config_to_dir(&dir, &zone).is_ok());
        let loaded = load_no_build_zone_from_dir(&dir).expect("loaded zone");
        assert_eq!(loaded.name, "Recoater Keep-Out");
        assert_eq!(loaded.position, [15.0, 20.0, 5.0]);
        assert_eq!(loaded.rotation_deg, [0.0, 0.0, 90.0]);
        assert!((loaded.clearance_mm - 3.5).abs() < 1e-4);
        assert!(loaded.enabled);
        assert!(!loaded.visible);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn machines_config_roundtrip_and_min_z_floor_work() {
        let dir = std::env::temp_dir().join(format!("nestcraft_machines_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut presets = default_machine_presets();
        presets[0].size_mm = [165.0, 215.0, 325.0];
        presets[0].wall_margin_mm = 7.5;
        presets[0].min_z_mm = 2.0;
        presets[0].clearance_mm = 3.0;
        assert!(save_machine_presets_to_dir(&dir, &presets).is_ok());
        let loaded = load_machine_presets_from_dir(&dir).expect("loaded presets");
        assert_eq!(loaded, presets);
        let _ = std::fs::remove_dir_all(&dir);

        let mut app = NestCraftApp::new();
        app.job.min_z_mm = 5.0;
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        assert!(app.job.is_instance_above_floor(iid));
        app.nudge_selected([0.0, 0.0, -10.0]);
        assert!(app.job.is_instance_above_floor(iid));
        let wb = app.job.instances[0].world_bounds(app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds);
        assert!((wb[0][2] - 5.0).abs() < 1e-3);
    }

    #[test]
    fn move_selection_supports_relative_and_absolute_modes() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");

        // Select both i1 and i2 (with i2 as primary anchor).
        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);
        let p1_before = app.job.instances[0].translation();
        let p2_before = app.job.instances[1].translation();

        // Relative move by (+10, -5, +4)
        app.apply_move_selection(false, [10.0, -5.0, 4.0]);
        let p1_rel = app.job.instances[0].translation();
        let p2_rel = app.job.instances[1].translation();
        assert!((p1_rel[0] - (p1_before[0] + 10.0)).abs() < 1e-4);
        assert!((p2_rel[1] - (p2_before[1] - 5.0)).abs() < 1e-4);
        assert!((p2_rel[2] - (p2_before[2] + 4.0)).abs() < 1e-4);

        // Absolute move places anchor i2 at (50, 60, 10) and preserves relative offset to i1.
        app.apply_move_selection(true, [50.0, 60.0, 10.0]);
        let p1_abs = app.job.instances[0].translation();
        let p2_abs = app.job.instances[1].translation();
        assert!((p2_abs[0] - 50.0).abs() < 1e-4);
        assert!((p2_abs[1] - 60.0).abs() < 1e-4);
        assert!((p2_abs[2] - 10.0).abs() < 1e-4);
        assert!(((p2_abs[0] - p1_abs[0]) - (p2_rel[0] - p1_rel[0])).abs() < 1e-4);
    }

    #[test]
    fn save_bed_and_export_individual_stls_work() {
        let dir = std::env::temp_dir().join(format!("nestcraft_mvp_io_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let mut app = NestCraftApp::new();
        app.add_sample_part();
        app.add_sample_part();
        app.export_stl_prefix = "job_שלום".to_owned();
        app.export_stl_suffix = "_sls!".to_owned();
        app.export_stl_include_id = true;

        let exported = app.export_individual_stls(&dir).expect("exported stls");
        assert_eq!(exported, 2);
        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.is_ascii()
            && !f.contains('#')
            && !f.contains(' ')
            && f.starts_with("job_")
            && f.ends_with("_sls_.stl")));

        // Switching machines does not carry over the active machine's No-Build Zone.
        let mesh = app.job.parts[0].mesh.clone();
        app.job.no_build_zone = Some(NoBuildZone::new("P110 Zone", mesh));
        app.apply_machine_slot(3);
        assert_ne!(
            app.job.no_build_zone.as_ref().map(|z| z.name.as_str()),
            Some("P110 Zone")
        );

        let bed_path = dir.join("test_bed.nestc");
        app.save_bed_file(&bed_path);
        assert!(bed_path.exists());

        let mut app2 = NestCraftApp::new();
        app2.open_bed_file(&bed_path);
        assert_eq!(app2.job.instances.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn async_load_task_processes_stl_files_and_tracks_progress() {
        let dir = std::env::temp_dir().join(format!("nestcraft_async_load_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let ascii_stl = b"solid tri\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 10 0 0\nvertex 0 10 0\nendloop\nendfacet\nendsolid tri\n";
        let stl_a = dir.join("a.stl");
        let stl_b = dir.join("b.stl");
        assert!(std::fs::write(&stl_a, ascii_stl).is_ok());
        assert!(std::fs::write(&stl_b, ascii_stl).is_ok());

        let ctx = egui::Context::default();
        let mut app = NestCraftApp::new();
        app.start_import_files(vec![stl_a, stl_b], &ctx);
        assert!(app.load_task.is_some());

        for _ in 0..100 {
            app.poll_load_task(&ctx);
            if app.load_task.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.load_task.is_none());
        assert_eq!(app.job.instances.len(), 2);
        assert!(app.below_zero_import_dialog.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn importing_stl_preserves_model_location_and_prompts_below_zero() {
        let dir = std::env::temp_dir().join(format!("nestcraft_model_loc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let above_stl = dir.join("above.stl");
        let above_txt = b"solid tri\nfacet normal 0 0 1\nouter loop\nvertex 40 55 12\nvertex 50 55 12\nvertex 40 65 22\nendloop\nendfacet\nendsolid tri\n";
        assert!(std::fs::write(&above_stl, above_txt).is_ok());

        let below_stl = dir.join("below.stl");
        let below_txt = b"solid tri\nfacet normal 0 0 1\nouter loop\nvertex 40 55 -8\nvertex 50 55 -8\nvertex 40 65 2\nendloop\nendfacet\nendsolid tri\n";
        assert!(std::fs::write(&below_stl, below_txt).is_ok());

        let mut app = NestCraftApp::new();
        assert!(app.import_single_file(&above_stl).is_ok());
        assert!(app.below_zero_import_dialog.is_empty());
        let id_above = app.job.instances[0].id;
        assert!((app.job.instance_min_z(id_above).expect("min_z") - 12.0).abs() < 1e-4);

        assert!(app.import_single_file(&below_stl).is_ok());
        assert_eq!(app.below_zero_import_dialog.len(), 1);
        let id_below = app.job.instances[1].id;
        assert!((app.job.instance_min_z(id_below).expect("min_z") + 8.0).abs() < 1e-4);

        app.reset_below_zero_imported_parts();
        assert!(app.below_zero_import_dialog.is_empty());
        assert!(app.job.instance_min_z(id_below).expect("min_z").abs() < 1e-4);
        let proto = app.job.part(app.job.instances[1].part_id).expect("part");
        let wb = app.job.instances[1].world_bounds(proto.mesh.bounds);
        assert!((wb[0][0] - 40.0).abs() < 1e-4);
        assert!((wb[0][1] - 55.0).abs() < 1e-4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_and_redo_restore_unlimited_steps() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        let p0 = app.job.instances[0].translation();

        app.nudge_selected([10.0, 0.0, 0.0]);
        let p1 = app.job.instances[0].translation();
        assert!((p1[0] - (p0[0] + 10.0)).abs() < 1e-4);

        app.toggle_selected_transparent();
        assert!(app.job.instances[0].transparent);

        app.delete_selected();
        assert!(app.job.instances.is_empty());

        // Undo delete -> transparent -> nudge -> add_sample_part
        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 1);
        assert_eq!(app.job.instances[0].id, iid);
        assert!(app.job.instances[0].transparent);

        assert!(app.undo());
        assert!(!app.job.instances[0].transparent);

        assert!(app.undo());
        assert!((app.job.instances[0].translation()[0] - p0[0]).abs() < 1e-4);

        assert!(app.undo());
        assert!(app.job.instances.is_empty());

        // Stack exhausted
        assert!(!app.undo());

        // Redo all the way back
        assert!(app.redo());
        assert_eq!(app.job.instances.len(), 1);
        assert!(app.redo());
        assert!((app.job.instances[0].translation()[0] - (p0[0] + 10.0)).abs() < 1e-4);
        assert!(app.redo());
        assert!(app.job.instances[0].transparent);
        assert!(app.redo());
        assert!(app.job.instances.is_empty());
        assert!(!app.redo());
    }

    #[test]
    fn merge_selected_parts_and_shells_to_parts_roundtrip_with_undo() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        assert_eq!(app.job.instances.len(), 2);

        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);
        app.merge_selected_parts();
        assert_eq!(app.job.instances.len(), 1);
        let merged_id = app.selected.expect("merged");

        app.shells_to_parts_for(vec![merged_id]);
        assert_eq!(app.job.instances.len(), 2);
        assert_eq!(app.selected_set.len(), 2);

        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 1);
        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 2);

        // Merging with keep_sources = true preserves originals (2 + 1 = 3).
        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);
        app.merge_selected_parts_with_keep(&[i1, i2], true);
        assert_eq!(app.job.instances.len(), 3);
        let kept_merged_id = app.selected.expect("kept_merged");

        // Splitting with keep_source = true preserves the merged source (3 + 2 = 5).
        app.shells_to_parts_for_with_keep(vec![kept_merged_id], true);
        assert_eq!(app.job.instances.len(), 5);
        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 3);
    }

    #[test]
    fn faulty_placed_instance_ids_flags_non_watertight_meshes() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        assert!(app.faulty_placed_instance_ids().is_empty());

        // Single open triangle is non-watertight and must be flagged.
        let open_mesh = TriMesh::from_indexed(
            vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]],
            vec![[0, 1, 2]],
        )
        .expect("open triangle mesh");
        let pid = app.job.add_part("OpenTriangle".to_owned(), Arc::new(open_mesh));
        let faulty_id = app.job.add_instance_on_floor_next(pid).expect("instance");
        assert_eq!(app.faulty_placed_instance_ids(), vec![faulty_id]);
    }

    #[test]
    fn rotate_transform_around_pivot_rotates_90_deg_around_z() {
        let orig = [
            [1.0, 0.0, 0.0, 20.0],
            [0.0, 1.0, 0.0, 10.0],
            [0.0, 0.0, 1.0, 5.0],
        ];
        let pivot = [10.0, 10.0, 5.0];
        let rotated = rotate_transform_around_pivot(orig, pivot, [0.0, 0.0, 1.0], std::f32::consts::FRAC_PI_2);
        assert!((rotated[0][3] - 10.0).abs() < 1e-4);
        assert!((rotated[1][3] - 20.0).abs() < 1e-4);
        assert!((rotated[2][3] - 5.0).abs() < 1e-4);
    }

    #[test]
    fn rotating_off_origin_part_keeps_its_own_centroid_fixed() {
        let mut app = NestCraftApp::new();
        // Box with local coordinates far from (0, 0, 0): [100..120, 50..70, 10..30]
        let mesh = TriMesh::from_indexed(
            vec![
                [100.0, 50.0, 10.0],
                [120.0, 50.0, 10.0],
                [120.0, 70.0, 10.0],
                [100.0, 70.0, 10.0],
                [100.0, 50.0, 30.0],
                [120.0, 50.0, 30.0],
                [120.0, 70.0, 30.0],
                [100.0, 70.0, 30.0],
            ],
            vec![
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
            ],
        )
        .expect("valid box");
        let pid = app.job.add_part("off_origin", Arc::new(mesh));
        let iid = app.job.add_instance(pid, IDENTITY_TRANSFORM).expect("placed");
        app.select_instance(Some(iid));

        app.rotate_selected_90(2);
        let proto = app.job.part(pid).expect("part");
        let wb = app.job.instances[0].world_bounds(proto.mesh.bounds);
        let cx = (wb[0][0] + wb[1][0]) * 0.5;
        let cy = (wb[0][1] + wb[1][1]) * 0.5;
        let cz = (wb[0][2] + wb[1][2]) * 0.5;
        assert!((cx - 110.0).abs() < 1e-3, "cx={cx}");
        assert!((cy - 60.0).abs() < 1e-3, "cy={cy}");
        assert!((cz - 20.0).abs() < 1e-3, "cz={cz}");

        app.rotate_selected_by(0, 45.0);
        let euler = app.job.instances[0].euler_deg();
        assert!((euler[0] - 45.0).abs() < 1e-3);
        assert!((euler[2] - 90.0).abs() < 1e-3);
    }

    #[test]
    fn render_part_thumbnail_produces_shaded_20x20_image() {
        let mesh = create_sample_bracket_mesh(0).expect("sample mesh");
        let img = render_part_thumbnail(&mesh);
        assert_eq!(img.size, [THUMB_PX, THUMB_PX]);
        let opaque_pixels = img.pixels.iter().filter(|c| c.a() == 255).count();
        assert!(opaque_pixels > 20 && opaque_pixels < THUMB_PX * THUMB_PX);
    }

    #[test]
    fn group_and_ungroup_selected_parts_with_undo() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);

        app.group_selected_parts("My Group".to_owned());
        assert_eq!(app.job.groups.len(), 1);
        assert!(app.job.instances.iter().all(|i| i.group_id.is_some()));

        app.ungroup_selected_parts();
        assert!(app.job.instances.iter().all(|i| i.group_id.is_none()));

        assert!(app.undo());
        assert!(app.job.instances.iter().all(|i| i.group_id.is_some()));
        assert!(app.undo());
        assert!(app.job.groups.is_empty());
        assert!(app.job.instances.iter().all(|i| i.group_id.is_none()));

        app.job.create_group("Empty 1");
        app.job.create_group("Empty 2");
        assert_eq!(app.job.remove_empty_groups(), 2);
        assert!(app.job.groups.is_empty());
    }

    #[test]
    fn align_instance_face_to_neg_z_rotates_top_face_downward() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        let bounds = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
        let top_before = app.job.instances[0].world_bounds(bounds)[1][2];

        // Aligning +Z top face to -Z flips the part upside down by 180 deg.
        assert!(app.job.align_instance_face_to_neg_z(iid, [0.0, 0.0, 1.0]));
        let m = app.job.instances[0].transform;
        let wn_z = m[2][2];
        assert!((wn_z + 1.0).abs() < 1e-4, "expected -1.0, got {wn_z}");
        let top_after = app.job.instances[0].world_bounds(bounds)[1][2];
        assert!((top_before - top_after).abs() < 1e-3);
    }

    #[test]
    fn scaling_part_supports_uniform_per_axis_and_imperial_metric_with_undo() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let iid = app.selected.expect("selected");
        let dims0 = {
            let b = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
            app.job.instances[0].world_bounds(b)
        };
        let dx0 = dims0[1][0] - dims0[0][0];
        let dy0 = dims0[1][1] - dims0[0][1];
        let dz0 = dims0[1][2] - dims0[0][2];

        // Per-axis scale (2x X, 1x Y, 0.5x Z)
        app.apply_scale_selection([2.0, 1.0, 0.5]);
        let b1 = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
        let wb1 = app.job.instances[0].world_bounds(b1);
        assert!(((wb1[1][0] - wb1[0][0]) - dx0 * 2.0).abs() < 1e-3);
        assert!(((wb1[1][1] - wb1[0][1]) - dy0).abs() < 1e-3);
        assert!(((wb1[1][2] - wb1[0][2]) - dz0 * 0.5).abs() < 1e-3);

        // Imperial -> Metric (x 25.4)
        app.apply_scale_selection([25.4, 25.4, 25.4]);
        let b2 = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
        let wb2 = app.job.instances[0].world_bounds(b2);
        assert!(((wb2[1][0] - wb2[0][0]) - dx0 * 2.0 * 25.4).abs() < 1e-2);

        // Undo back to original size
        assert!(app.undo());
        assert!(app.undo());
        let b_undo = app.job.part(app.job.instances[0].part_id).expect("part").mesh.bounds;
        let wb_undo = app.job.instances[0].world_bounds(b_undo);
        assert!(((wb_undo[1][0] - wb_undo[0][0]) - dx0).abs() < 1e-3);
        let _ = iid;
    }

    #[test]
    fn create_sinterbox_groups_multi_selection_and_undoes_cleanly() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        app.select_instance(Some(i1));
        app.toggle_select_instance(i2);

        app.open_sinterbox_dialog();
        assert!(app.sinterbox_dialog_open);
        assert_eq!(app.sinterbox_label_text, "Sample Bracket #1");

        app.apply_create_sinterbox();
        assert_eq!(app.job.groups.len(), 1);
        assert_eq!(app.job.instances.len(), 3);
        let gid = app.job.groups[0].id;
        assert!(app.job.instances.iter().all(|i| i.group_id == Some(gid)));

        let cage_inst = app.job.instances.last().expect("cage");
        assert!(cage_inst.transparent);
        let cage_mesh = &app.job.part(cage_inst.part_id).expect("cage part").mesh;
        assert!(manifold::analyze_mesh(cage_mesh).is_watertight_manifold());

        assert!(app.undo());
        assert!(app.job.groups.is_empty());
        assert_eq!(app.job.instances.len(), 2);
    }

    #[test]
    fn create_sinterbox_single_part_with_multiplication_creates_batch_and_group() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        assert_eq!(app.selected_set.len(), 1);

        app.open_sinterbox_dialog();
        app.sinterbox_single_copies = 4;
        app.sinterbox_shape_idx = 1; // Tight Fit
        app.apply_create_sinterbox();

        assert_eq!(app.job.groups.len(), 1);
        assert_eq!(app.job.instances.len(), 5); // 4 parts + 1 cage
        let gid = app.job.groups[0].id;
        assert!(app.job.instances.iter().all(|i| i.group_id == Some(gid)));

        let cage_inst = app.job.instances.last().expect("cage");
        let cage_mesh = &app.job.part(cage_inst.part_id).expect("cage part").mesh;
        assert!(manifold::analyze_mesh(cage_mesh).is_watertight_manifold());

        assert!(app.undo());
        assert!(app.job.groups.is_empty());
        assert_eq!(app.job.instances.len(), 1);
    }

    #[test]
    fn rotation_matrix_matches_rotate_transform_and_moving_layer_active() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let inst = &app.job.instances[0];
        let orig = inst.transform;
        let pivot = [10.0, 20.0, 5.0];
        let axis = world_axis(0);
        let angle = 0.75_f32;

        let rotated = rotate_transform_around_pivot(orig, pivot, axis, angle);
        let m4 = rotation_matrix_4x4_col_major(pivot, axis, angle);

        // Apply `m4 * orig` to a test point [1, 2, 3] and compare with `rotated`.
        let p = [1.0_f32, 2.0, 3.0];
        let mut p_orig = [0.0_f32; 3];
        let mut p_rot = [0.0_f32; 3];
        for r in 0..3 {
            p_orig[r] = orig[r][0] * p[0] + orig[r][1] * p[1] + orig[r][2] * p[2] + orig[r][3];
            p_rot[r] = rotated[r][0] * p[0] + rotated[r][1] * p[1] + rotated[r][2] * p[2] + rotated[r][3];
        }
        let mut p_m4 = [0.0_f32; 3];
        for r in 0..3 {
            p_m4[r] = m4[0][r] * p_orig[0] + m4[1][r] * p_orig[1] + m4[2][r] * p_orig[2] + m4[3][r];
            assert!((p_m4[r] - p_rot[r]).abs() < 1e-4, "mismatch at {r}: {} vs {}", p_m4[r], p_rot[r]);
        }

        let id = inst.id;
        app.rotate_drag = Some(RotateDragState {
            pivot,
            pivot_screen: egui::pos2(100.0, 100.0),
            axis: 2,
            angle_rad: 0.5,
            prev_pos: egui::pos2(110.0, 100.0),
            edge_tangent: egui::vec2(1.0, 0.0),
            instances: vec![(id, orig)],
        });
        app.start_moving_layer(viewport::DragExclude::Instances(vec![id]));
        let layer = app.current_moving_layer().expect("moving layer during rotate_drag");
        assert_eq!(layer.transform, rotation_matrix_4x4_col_major(pivot, world_axis(2), 0.5));
    }

    #[test]
    fn projection_toggle_and_default_view_axes() {
        let mut app = NestCraftApp::new();
        assert!(!app.is_orthographic());

        // Default view looks from Front-Left (+X right, +Y far/away).
        let (right, _, back) = app.camera.basis();
        assert!(right.x > 0.0, "Default view should put +X to the right: right={right:?}");
        assert!(back.y < 0.0, "Default view should put +Y far/away: back={back:?}");

        let initial_hh = app.camera.half_height();
        app.toggle_projection();
        assert!(app.is_orthographic());
        assert_eq!(app.camera.fov, 0.0);
        assert!((app.camera.half_height() - initial_hh).abs() < 1e-6);

        let reset_cam = app.default_camera_for_job();
        assert_eq!(reset_cam.fov, 0.0);
        assert!((reset_cam.yaw - std::f64::consts::FRAC_PI_4).abs() < 1e-9);

        app.toggle_projection();
        assert!(!app.is_orthographic());
        assert!((app.camera.half_height() - initial_hh).abs() < 1e-6);
    }

    #[test]
    fn collision_check_updates_incrementally_for_single_part_moves_and_ignores_selection() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let i1 = app.selected.expect("i1");
        app.add_sample_part();
        let i2 = app.selected.expect("i2");
        app.add_sample_part();
        let i3 = app.selected.expect("i3");

        // Place i1 and i2 overlapping, and i3 far away in free space.
        app.job.instances[0].set_translation([0.0, 0.0, 0.0]);
        app.job.instances[1].set_translation([10.0, 0.0, 0.0]);
        app.job.instances[2].set_translation([200.0, 0.0, 0.0]);

        app.collision_check_active = true;
        let initial_rep = app.collision_cache.sync(&app.job, COLLISION_VOXEL_PITCH_MM);
        app.apply_collision_report(&initial_rep, true);
        assert_eq!(app.collision_cache.last_tested_pairs(), 3);
        assert_eq!(app.scene_cache.collision_ids, HashSet::from([i1, i2]));

        // Selecting i3 must not spawn a background calc task or re-test any pairs.
        app.select_instance(Some(i3));
        assert!(app.calc_task.is_none());
        assert_eq!(app.collision_cache.last_tested_pairs(), 3);

        // Nudging i3 in free space completes synchronously without spawning a background task.
        app.nudge_selected([5.0, 0.0, 0.0]);
        assert!(app.calc_task.is_none());
        assert_eq!(app.collision_cache.last_tested_pairs(), 2);
        assert_eq!(app.scene_cache.collision_ids, HashSet::from([i1, i2]));

        // Live-dragging i3 into i2 tests only 2 pairs (i3 vs i1, i3 vs i2) and adds i3 to collision_ids.
        let i3_start = app.job.instances[2].translation();
        app.drag_state = Some(DragState {
            anchor_id: i3,
            instances: vec![(i3, i3_start)],
            start_hit: i3_start,
            plane_z: i3_start[2],
            min_allowed_dz: f32::NEG_INFINITY,
        });
        app.job.instances[2].set_translation([15.0, 0.0, 0.0]);
        app.update_live_drag_collision();
        assert_eq!(app.collision_cache.last_tested_pairs(), 2);
        assert_eq!(app.scene_cache.collision_ids, HashSet::from([i1, i2, i3]));

        // Moving i3 back to free space resolves i3's collision while preserving static pair (i1, i2).
        app.job.instances[2].set_translation([200.0, 0.0, 0.0]);
        app.update_live_drag_collision();
        assert_eq!(app.collision_cache.last_tested_pairs(), 2);
        assert_eq!(app.scene_cache.collision_ids, HashSet::from([i1, i2]));
    }

    #[test]
    fn calc_progress_modal_shown_only_for_initial_calculations_not_updates() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        app.add_sample_part();
        app.add_sample_part();
        app.job.instances[0].set_translation([0.0, 0.0, 0.0]);
        app.job.instances[1].set_translation([10.0, 0.0, 0.0]);
        app.job.instances[2].set_translation([200.0, 0.0, 0.0]);

        // Pressing Collision Check button shows progress modal; toggling off cancels task; toggling on again shows it.
        app.toggle_collision_check();
        assert!(app.calc_task.as_ref().is_some_and(|t| t.show_progress));
        app.toggle_collision_check();
        assert!(app.calc_task.is_none());
        app.toggle_collision_check();
        assert!(app.calc_task.as_ref().is_some_and(|t| t.show_progress));

        // After cache is initialized, moving a part into overlap triggers an update without progress modal.
        app.calc_task = None;
        let _ = app.collision_cache.sync(&app.job, COLLISION_VOXEL_PITCH_MM);
        app.job.instances[2].set_translation([15.0, 0.0, 0.0]);
        app.refresh_collision_check();
        assert!(app.calc_task.as_ref().is_some_and(|t| !t.show_progress));

        // Initial thermal map shows progress; refreshing existing thermal map does not.
        app.calc_task = None;
        app.start_thermal_map();
        assert!(app.calc_task.as_ref().is_some_and(|t| t.show_progress));
        app.calc_task = None;
        app.thermal_report = Some(nestcraft_core::thermal::ThermalMapReport::compute(&app.job));
        app.refresh_thermal_map();
        assert!(app.calc_task.as_ref().is_some_and(|t| !t.show_progress));

        // Initial slice distribution shows progress; dirtying & recomputing keeps cache and hides progress.
        app.calc_task = None;
        app.start_slice_distribution();
        assert!(app.calc_task.as_ref().is_some_and(|t| t.show_progress));
        app.calc_task = None;
        app.slice_dist_cache = Some(nestcraft_core::slice::SliceDistribution::compute(&app.job, 1.0));
        app.slice_dist_open = true;
        app.mark_dirty();
        assert!(app.slice_dist_cache.is_some());
        assert!(app.slice_dist_dirty);
        app.calc_task = None;
        app.start_slice_distribution();
        assert!(!app.slice_dist_dirty);
        assert!(app.calc_task.as_ref().is_some_and(|t| !t.show_progress));
    }

    #[test]
    fn async_save_bed_tracks_progress_writes_file_and_sets_hint() {
        let dir = std::env::temp_dir().join(format!("nestcraft_async_save_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let bed_path = dir.join("saved_job.nestc");

        let ctx = egui::Context::default();
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        app.add_sample_part();

        app.start_save_bed(bed_path.clone(), &ctx);
        assert!(app.save_task.is_some());

        for _ in 0..100 {
            app.poll_save_task(&ctx);
            if app.save_task.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.save_task.is_none());
        assert!(bed_path.exists());
        assert!(matches!(
            &app.save_hint,
            Some((msg, false, _)) if msg.contains("Saved") && msg.contains("saved_job.nestc")
        ));

        let mut app2 = NestCraftApp::new();
        app2.open_bed_file(&bed_path);
        assert_eq!(app2.job.instances.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_save_filename_uses_date_bed_and_material_convention() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_782), (2026, 11, 25));

        assert_eq!(
            format_job_save_filename("20261125", "Farsoon HT403P", "pa2200", "nestc"),
            "20261125_ht403_2200.nestc"
        );
        assert_eq!(
            format_job_save_filename("20261125", "EOS Formiga P110", "pa2200", "nestc"),
            "20261125_p110_2200.nestc"
        );
        assert_eq!(
            format_job_save_filename("20261125", "Farsoon HT403P", "fs3300", "3mf"),
            "20261125_ht403_3300.3mf"
        );
        assert_eq!(
            format_job_save_filename("20261125", "EOS Formiga P110 Velocis", "pa2200", "nestc"),
            "20261125_p110v_2200.nestc"
        );
        assert_eq!(
            format_job_save_filename("20261125", "EOS Formiga P100", "pa2200", "nestc"),
            "20261125_p100_2200.nestc"
        );
        assert_eq!(
            format_job_save_filename("20261125", "Farsoon SS403P", "fs3300", "3mf"),
            "20261125_ss403_3300.3mf"
        );
        assert_eq!(
            format_job_save_filename("20261125", "Custom SLS", "pa2210fr", "nestc"),
            "20261125_custom_sls_2210fr.nestc"
        );

        let app = NestCraftApp::new();
        let name = app.default_save_filename("nestc");
        assert!(name.ends_with("_p110_2200.nestc"), "unexpected name: {name}");
        assert!(name[..8].bytes().all(|b| b.is_ascii_digit()), "expected yyyymmdd prefix: {name}");
    }

    #[test]
    fn five_predefined_machines_configure_nbz_materials_and_legacy_migration() {
        let presets = default_machine_presets();
        assert_eq!(presets.len(), MACHINE_SLOT_COUNT);
        assert_eq!(presets[0].name, "EOS Formiga P110");
        assert_eq!(presets[1].name, "EOS Formiga P110 Velocis");
        assert_eq!(presets[2].name, "EOS Formiga P100");
        assert_eq!(presets[3].name, "Farsoon HT403P");
        assert_eq!(presets[4].name, "Farsoon SS403P");
        assert_eq!(presets[5].name, "Custom SLS");

        // P110, P110 Velocis, P100 share [200, 250, 330] Front-Left.
        for slot in [0, 1, 2] {
            assert_eq!(presets[slot].size_mm, [200.0, 250.0, 330.0]);
            assert_eq!(presets[slot].bed_origin, BedOriginCfg::FrontLeft);
        }
        // P110 & P110 Velocis share NBZ directory; P100 has no NBZ.
        assert!(slot_supports_no_build_zone(0));
        assert!(slot_supports_no_build_zone(1));
        assert_eq!(no_build_zone_dir_for_slot(0), no_build_zone_dir_for_slot(1));
        assert!(!slot_supports_no_build_zone(2));
        assert!(no_build_zone_dir_for_slot(2).is_none());

        // HT403P and SS403P share [400, 400, 450] Center origin with distinct default materials.
        assert_eq!(presets[3].size_mm, [400.0, 400.0, 450.0]);
        assert_eq!(presets[4].size_mm, [400.0, 400.0, 450.0]);
        assert_eq!(presets[3].bed_origin, BedOriginCfg::Center);
        assert_eq!(presets[4].bed_origin, BedOriginCfg::Center);
        assert_eq!(presets[3].material_id, "pa2200");
        assert_eq!(presets[4].material_id, "fs3300");

        let mut app = NestCraftApp::new();
        app.machine_presets = presets;
        app.apply_machine_slot(3);
        assert_eq!(app.job.material_id, "pa2200");
        app.apply_machine_slot(4);
        assert_eq!(app.job.material_id, "fs3300");

        let mesh = app.job.parts.first().map(|p| p.mesh.clone()).unwrap_or_else(|| {
            Arc::new(create_sample_bracket_mesh(0).expect("bracket"))
        });
        app.job.no_build_zone = Some(NoBuildZone::new("Temp Zone", mesh));
        app.apply_machine_slot(2);
        assert!(app.job.no_build_zone.is_none());

        // Legacy 3-slot machines.json migrates cleanly to 6 slots preserving custom values.
        let dir = std::env::temp_dir().join(format!("nestcraft_legacy_machines_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let legacy_json = serde_json::json!([
            {
                "name": "EOS Formiga P110",
                "size_mm": [195.0, 245.0, 325.0],
                "wall_margin_mm": 6.0,
                "min_z_mm": 1.0,
                "clearance_mm": 3.0,
                "bed_origin": "FrontLeft"
            },
            {
                "name": "Farsoon HT403P",
                "size_mm": [390.0, 390.0, 440.0],
                "wall_margin_mm": 5.0,
                "min_z_mm": 0.0,
                "clearance_mm": 2.5,
                "bed_origin": "Center"
            },
            {
                "name": "My Custom",
                "size_mm": [300.0, 300.0, 300.0],
                "wall_margin_mm": 4.0,
                "min_z_mm": 0.0,
                "clearance_mm": 2.0,
                "bed_origin": "FrontLeft"
            }
        ]);
        assert!(std::fs::write(dir.join("machines.json"), legacy_json.to_string()).is_ok());
        let migrated = load_machine_presets_from_dir(&dir).expect("migrated 6 slots");
        assert_eq!(migrated[0].size_mm, [195.0, 245.0, 325.0]);
        assert_eq!(migrated[1].name, "EOS Formiga P110 Velocis");
        assert_eq!(migrated[1].size_mm, [195.0, 245.0, 325.0]);
        assert_eq!(migrated[2].name, "EOS Formiga P100");
        assert_eq!(migrated[2].size_mm, [195.0, 245.0, 325.0]);
        assert_eq!(migrated[3].size_mm, [390.0, 390.0, 440.0]);
        assert_eq!(migrated[4].name, "Farsoon SS403P");
        assert_eq!(migrated[4].size_mm, [390.0, 390.0, 440.0]);
        assert_eq!(migrated[4].material_id, "fs3300");
        assert_eq!(migrated[5].name, "My Custom");
        assert_eq!(migrated[5].size_mm, [300.0, 300.0, 300.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_deduplicates_prototypes_and_delete_prunes_with_undo() {
        let dir = std::env::temp_dir().join(format!("nestcraft_dedup_prune_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        // Multi-instance 3MF with 2 instances of 1 part.
        let mut src_job = nestcraft_core::model::BuildJob::new(nestcraft_core::model::MachineProfile::FormigaP110);
        let mesh = Arc::new(create_sample_bracket_mesh(0).expect("bracket"));
        let pid = src_job.add_part("Bracket3MF", mesh);
        let _ = src_job.add_instance_centered_on_floor(pid).expect("i1");
        let _ = src_job.add_instance_on_floor_next(pid).expect("i2");
        let threemf_bytes = nestcraft_core::io::threemf::write_threemf(&src_job).expect("3mf");
        let threemf_path = dir.join("multi.3mf");
        assert!(std::fs::write(&threemf_path, threemf_bytes).is_ok());

        // Single STL file imported twice, plus a differently named copy with identical STL bytes.
        let stl_bytes = stl::write_binary_stl(&src_job.parts[0].mesh, None).expect("stl");
        let stl_path = dir.join("single.stl");
        let stl_copy_path = dir.join("single_copy.stl");
        assert!(std::fs::write(&stl_path, &stl_bytes).is_ok());
        assert!(std::fs::write(&stl_copy_path, &stl_bytes).is_ok());

        let mut app = NestCraftApp::new();
        let _ = app.import_single_file(&threemf_path).expect("import 3mf");
        assert_eq!(app.job.instances.len(), 2);
        assert_eq!(app.job.parts.len(), 1, "multi-instance 3MF should share 1 prototype");

        let _ = app.import_single_file(&stl_path).expect("import stl 1");
        let _ = app.import_single_file(&stl_path).expect("import stl 2");
        assert_eq!(app.job.instances.len(), 4);
        assert_eq!(app.job.parts.len(), 2, "re-importing same STL should reuse prototype");
        let stl_pid = app.job.parts[1].id;

        let _ = app.import_single_file(&stl_copy_path).expect("import stl copy");
        assert_eq!(app.job.instances.len(), 5);
        assert_eq!(app.job.parts.len(), 3, "different filename keeps its own prototype");
        assert!(Arc::ptr_eq(&app.job.parts[1].mesh, &app.job.parts[2].mesh));

        // Delete both `single` STL instances and verify prototype + caches are pruned, then undo restores them.
        let stl_insts: Vec<_> = app
            .job
            .instances
            .iter()
            .filter(|i| i.part_id == stl_pid)
            .map(|i| i.id)
            .collect();
        app.selected_set = stl_insts.into_iter().collect();
        app.selected = app.selected_set.iter().copied().next();
        app.delete_selected();
        assert_eq!(app.job.instances.len(), 3);
        assert_eq!(app.job.parts.len(), 2);
        assert!(!app.scene_cache.parts.contains_key(&stl_pid));

        assert!(app.undo());
        assert_eq!(app.job.instances.len(), 5);
        assert_eq!(app.job.parts.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_save_reuses_path_until_machine_or_config_changes() {
        let dir = std::env::temp_dir().join(format!("nestcraft_quicksave_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let bed_path = dir.join("bed_one.nestc");
        let bed_path_two = dir.join("bed_two.nestc");

        let ctx = egui::Context::default();
        let mut app = NestCraftApp::new();
        assert_eq!(app.quick_save_target(), None);

        app.add_sample_part();
        app.start_save_bed(bed_path.clone(), &ctx);
        for _ in 0..100 {
            app.poll_save_task(&ctx);
            if app.save_task.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(app.quick_save_target(), Some(bed_path.clone()));

        // Adding/moving parts keeps the same machine & config, so quick-save remains active.
        app.add_sample_part();
        app.nudge_selected([5.0, 0.0, 0.0]);
        assert_eq!(app.quick_save_target(), Some(bed_path.clone()));

        // Changing material clears quick-save; reverting it restores quick-save.
        let prev_mat = app.job.material_id.clone();
        app.job.material_id = "fs3300".to_owned();
        assert_eq!(app.quick_save_target(), None);
        app.job.material_id = prev_mat;
        assert_eq!(app.quick_save_target(), Some(bed_path.clone()));

        // Changing wall margin, min Z, clearance, bed origin, or machine slot invalidates quick-save.
        let prev_margin = app.job.wall_margin_mm;
        app.job.wall_margin_mm += 1.0;
        assert_eq!(app.quick_save_target(), None);
        app.job.wall_margin_mm = prev_margin;

        let prev_min_z = app.job.min_z_mm;
        app.job.min_z_mm += 2.0;
        assert_eq!(app.quick_save_target(), None);
        app.job.min_z_mm = prev_min_z;

        let prev_clearance = app.job.clearance_mm;
        app.job.clearance_mm += 0.5;
        assert_eq!(app.quick_save_target(), None);
        app.job.clearance_mm = prev_clearance;

        let prev_origin = app.job.bed_origin;
        app.job.bed_origin = nestcraft_core::model::BedOrigin::Center;
        assert_eq!(app.quick_save_target(), None);
        app.job.bed_origin = prev_origin;
        assert_eq!(app.quick_save_target(), Some(bed_path.clone()));

        app.apply_machine_slot(3);
        assert_eq!(app.quick_save_target(), None);

        // Saving under a new path enables quick-save for the new path.
        app.save_bed_file(&bed_path_two);
        assert_eq!(app.quick_save_target(), Some(bed_path_two.clone()));

        // Opening an existing .nestc bed sets quick-save to the opened path.
        let mut app2 = NestCraftApp::new();
        app2.open_bed_file(&bed_path);
        assert_eq!(app2.quick_save_target(), Some(bed_path));

        // Loading an unsaved generated job (e.g. Reprint Job) clears quick-save.
        let reprint = nestcraft_core::model::BuildJob::new(nestcraft_core::model::MachineProfile::FormigaP110);
        app2.apply_loaded_bed(reprint, "Reprint Job", None);
        assert_eq!(app2.quick_save_target(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nest3d_dialog_memory_autofit_cancel_and_undo_behave_cleanly() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        app.add_sample_part();
        assert_eq!(app.job.instances.len(), 2);

        app.open_nest3d_dialog();
        assert!(app.nest3d_dialog_open);
        assert!(app.nest3d_sys_avail_ram_bytes > 0);

        // Tight RAM cap triggers auto-fit coarsening without panicking.
        let fitted = nestcraft_core::nest3d::auto_fit_nest3d_resolution(
            &app.job,
            &app.nest3d_config,
            Some(&app.selected_set),
            4 * 1024 * 1024,
        );
        assert!((0.5..=4.0).contains(&fitted));

        // Starting and polling a 3D nest pushes a single undo snapshot when finished.
        let orig_tfs: Vec<_> = app.job.instances.iter().map(|i| i.transform).collect();
        app.nest3d_config.resolution_mm = 2.0;
        let ctx = egui::Context::default();
        app.start_nest3d(&ctx);
        assert!(app.is_nest3d_running());
        assert!(!app.nest3d_dialog_open);

        for _ in 0..200 {
            app.poll_nest3d_task(&ctx);
            if !app.is_nest3d_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!app.is_nest3d_running());
        assert!(app.nest3d_last_result.is_some());
        assert!(!app.undo_stack.is_empty());

        // Undo restores the pre-nest transforms cleanly.
        assert!(app.undo());
        let restored_tfs: Vec<_> = app.job.instances.iter().map(|i| i.transform).collect();
        assert_eq!(restored_tfs, orig_tfs);

        // Starting again and cancelling immediately restores original bed without adding undo entries.
        let undo_len_before_cancel = app.undo_stack.len();
        app.start_nest3d(&ctx);
        app.request_nest3d_cancel();
        for _ in 0..200 {
            app.poll_nest3d_task(&ctx);
            if !app.is_nest3d_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!app.is_nest3d_running());
        assert_eq!(app.undo_stack.len(), undo_len_before_cancel);
        let after_cancel_tfs: Vec<_> = app.job.instances.iter().map(|i| i.transform).collect();
        assert_eq!(after_cancel_tfs, orig_tfs);
    }

    #[test]
    fn dragging_130_parts_skips_sync_collision_when_inactive_or_multi_select() {
        let mut app = NestCraftApp::new();
        app.add_sample_part();
        let pid = app.job.parts[0].id;
        for _ in 1..130 {
            let _ = app.job.add_instance(pid, IDENTITY_TRANSFORM).expect("add 130th");
        }
        assert_eq!(app.job.instances.len(), 130);

        let all_ids: Vec<nestcraft_core::model::InstanceId> =
            app.job.instances.iter().map(|i| i.id).collect();
        app.selected_set = all_ids.iter().copied().collect();
        app.selected = all_ids.first().copied();
        let drag_instances: Vec<(nestcraft_core::model::InstanceId, [f32; 3])> = app
            .job
            .instances
            .iter()
            .map(|i| (i.id, i.translation()))
            .collect();
        app.drag_state = Some(DragState {
            anchor_id: all_ids[0],
            instances: drag_instances,
            start_hit: [0.0, 0.0, 0.0],
            plane_z: 0.0,
            min_allowed_dz: 0.0,
        });

        // 1. With collision_check_active == false, dragging 130 overlapping parts must not run
        // collision detection or populate hl_slot.
        app.start_moving_layer(viewport::DragExclude::Instances(all_ids));
        assert!(app.calc_task.is_none());
        app.update_live_drag_collision();
        assert!(!app.collision_overlay_nonempty);
        assert!(app.hl_slot.lock().is_ok_and(|g| g.is_none()));

        // 2. Even with collision_check_active == true, multi-part drag (>1 part) skips synchronous
        // UI-thread collision_cache.sync and defers refresh to drag release.
        app.collision_check_active = true;
        let _ = app.collision_cache.sync(&app.job, COLLISION_VOXEL_PITCH_MM);
        for inst in &mut app.job.instances {
            inst.set_translation([12.0, 4.0, 0.0]);
        }
        app.update_live_drag_collision();
        assert!(app.calc_task.is_none());
        assert_ne!(
            app.collision_cache.diff(&app.job, COLLISION_VOXEL_PITCH_MM),
            nestcraft_core::collision::CollisionDiff::NoChange
        );
    }

    #[test]
    fn gravity_settle_dialog_runs_compacts_and_supports_undo_and_cancel() {
        let mut app = NestCraftApp::new();
        app.job.no_build_zone = None;
        app.job.wall_margin_mm = 5.0;
        app.job.min_z_mm = 0.0;
        app.job.clearance_mm = 2.0;
        app.add_sample_part();
        app.add_sample_part();
        assert_eq!(app.job.instances.len(), 2);

        let [cx, cy] = app.job.bed_center_xy();
        app.job.instances[0].set_translation([cx - 15.0, cy - 10.0, 25.0]);
        app.job.instances[1].set_translation([cx - 15.0, cy - 10.0, 65.0]);

        app.open_gravity_settle_dialog();
        assert!(app.gravity_settle_dialog_open);

        let orig_tfs: Vec<_> = app.job.instances.iter().map(|i| i.transform).collect();
        let ctx = egui::Context::default();
        app.start_gravity_settle(&ctx);
        assert!(app.is_nest3d_running());
        assert!(!app.gravity_settle_dialog_open);

        for _ in 0..200 {
            app.poll_nest3d_task(&ctx);
            if !app.is_nest3d_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!app.is_nest3d_running());
        let res = app.gravity_settle_last_result.as_ref().expect("settle result");
        assert!(res.z_reduction_mm > 0.0, "expected Z compaction: {res:?}");
        assert!(!app.undo_stack.is_empty());

        // Single Ctrl+Z undo restores original transforms cleanly.
        assert!(app.undo());
        let restored_tfs: Vec<_> = app.job.instances.iter().map(|i| i.transform).collect();
        assert_eq!(restored_tfs, orig_tfs);

        // Cancel mid-flight restores original bed without pushing an undo entry.
        let undo_len_before_cancel = app.undo_stack.len();
        app.start_gravity_settle(&ctx);
        app.request_nest3d_cancel();
        for _ in 0..200 {
            app.poll_nest3d_task(&ctx);
            if !app.is_nest3d_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!app.is_nest3d_running());
        assert_eq!(app.undo_stack.len(), undo_len_before_cancel);
        let after_cancel_tfs: Vec<_> = app.job.instances.iter().map(|i| i.transform).collect();
        assert_eq!(after_cancel_tfs, orig_tfs);
    }
