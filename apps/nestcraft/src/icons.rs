//! Zero-touch NestCraft icon extension over `solidcraft_look::icons::DATA`.

use solidcraft_ui_kit::icons::{self, IconExtension, IconSet};

/// Extended icon set aliasing SolidCraft's CAD/mesh masters to semantic NestCraft actions.
pub static NESTCRAFT_ICONS: IconSet = IconSet::extended(
    solidcraft_look::icons::DATA,
    IconExtension {
        masters: &[],
        aliases: &[
            ("sls_import_mesh", "mesh_body", "mesh"),
            ("sls_auto_nest", "bounding_solid", "mesh"),
            ("sls_sinterbox", "perforate", "mesh"),
            ("sls_orient_lock", "print_face", "mesh"),
            ("sls_duplicate", "pattern_rect", "mesh"),
            ("sls_emboss_id", "emboss", "mesh"),
            ("sls_hollow", "shell", "mesh"),
            ("sls_slice_z", "section", "mesh"),
            ("sls_collision", "interference", "constraint"),
            ("sls_thermal_map", "curvature_map", "surface"),
            ("sls_zoom_parts", "select_parts", ""),
            ("sls_zoom_selected", "select_bodies", ""),
            ("sls_zoom_platform", "box", ""),
            ("sls_zoom_all", "fit", ""),
            ("sls_zoom_selection", "zoom", ""),
        ],
        icon_accents: &[],
    },
);

/// Install SolidCraft's glyphs, themes, `&NESTCRAFT_ICONS`, and IBM Plex fonts.
///
/// Calls the individual installers instead of `solidcraft_look::install(ctx)` so
/// `OnceLock` registers `&NESTCRAFT_ICONS` rather than locking in the base set.
pub fn install_look_and_icons(ctx: &egui::Context) {
    solidcraft_look::glyphs::install();
    solidcraft_look::themes::install();
    icons::install(&NESTCRAFT_ICONS);
    solidcraft_look::fonts::install(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nestcraft_icon_aliases_resolve_in_extended_set() {
        for name in [
            "sls_import_mesh",
            "sls_auto_nest",
            "sls_sinterbox",
            "sls_orient_lock",
            "sls_duplicate",
            "sls_emboss_id",
            "sls_hollow",
            "sls_slice_z",
            "sls_collision",
            "sls_thermal_map",
            "sls_zoom_parts",
            "sls_zoom_selected",
            "sls_zoom_platform",
            "sls_zoom_all",
            "sls_zoom_selection",
        ] {
            let entry = NESTCRAFT_ICONS
                .extension()
                .aliases
                .iter()
                .find(|(alias, _, _)| *alias == name);
            assert!(entry.is_some(), "missing NestCraft icon alias '{name}'");
        }
    }
}
