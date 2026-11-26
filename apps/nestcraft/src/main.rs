//! NestCraft 3D nesting desktop application.

#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]


mod app;
mod icons;
#[cfg(test)]
mod tests;
mod ui_bars;
mod ui_build_panel;
mod ui_modals;
mod ui_parts_panel;
mod ui_viewport;
mod viewport;

use std::io::Write as _;

use app::NestCraftApp;
use solidcraft_theme::Tokens;
use solidcraft_viewport_gpu::view::GpuTarget;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NestCraft")
            .with_inner_size([1280.0, 820.0])
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        depth_buffer: 0,
        stencil_buffer: 0,
        multisampling: 1,
        ..Default::default()
    };

    eframe::run_native(
        "NestCraft",
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
                    edges: false,
                };
                let scene = GpuTarget {
                    depth: Some(solidcraft_viewport_gpu::depth_format(&rs.device, true)),
                    ..window
                };
                let _ = solidcraft_viewport_egui::install(rs, scene, window, false, false, |err| {
                    let _ = writeln!(std::io::stderr(), "nestcraft wgpu error: {err}");
                });
            }
            Ok(Box::new(NestCraftApp::new()))
        }),
    )
}

impl eframe::App for NestCraftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let window_rect = ui.max_rect();
        solidcraft_theme::apply(&ctx);
        let t = Tokens::get();

        self.poll_load_task(&ctx);
        self.poll_save_task(&ctx);
        self.poll_calc_task(&ctx);
        self.handle_shortcuts(&ctx);
        self.handle_dropped_files(&ctx);
        self.draw_top_bar(ui, &ctx, &t);
        self.draw_edit_bar(ui, &ctx, &t);
        self.draw_nest_bar(ui, &ctx, &t);
        self.draw_analyze_bar(ui, &ctx, &t);
        self.draw_view_bar(ui, &ctx, &t);
        self.draw_bottom_bar(ui, &ctx, &t);
        self.draw_parts_browser(ui, &ctx, &t);
        self.draw_inspector(ui, &ctx, &t);
        self.draw_thermal_map_panel(ui, &ctx, &t);
        self.draw_slice_distribution_panel(ui, &ctx, &t);
        self.draw_viewport(ui, &ctx, &t);
        self.draw_custom_platform_modal(&ctx);
        self.draw_duplicate_modal(&ctx);
        self.draw_move_modal(&ctx);
        self.draw_scale_modal(&ctx);
        self.draw_sinterbox_modal(&ctx);
        self.draw_color_picker_modal(&ctx);
        self.draw_export_stl_modal(&ctx);
        self.draw_report_modal(&ctx);
        self.draw_failed_print_modal(&ctx);
        self.draw_nest2d_modal(&ctx);
        self.draw_below_zero_import_modal(&ctx);
        self.draw_keep_delete_modal(&ctx);
        self.draw_load_progress_modal(&ctx);
        self.draw_save_progress_modal(&ctx, &t);
        self.draw_calc_progress_modal(&ctx);
        self.draw_drop_overlay(&ctx, window_rect, &t);
    }
}

