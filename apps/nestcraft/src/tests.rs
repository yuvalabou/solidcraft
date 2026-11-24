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
        app.apply_machine_slot(1);
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
