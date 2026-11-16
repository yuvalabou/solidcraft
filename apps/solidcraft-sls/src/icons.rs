//! Zero-touch SLS icon extension over `solidcraft_look::icons::DATA`.

use solidcraft_ui_kit::icons::{self, IconExtension, IconSet};

/// Extended icon set aliasing SolidCraft's CAD/mesh masters to semantic SLS actions.
pub static SLS_ICONS: IconSet = IconSet::extended(
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
        ],
        icon_accents: &[],
    },
);

/// Install SolidCraft's glyphs, themes, `&SLS_ICONS`, and IBM Plex fonts.
///
/// Calls the individual installers instead of `solidcraft_look::install(ctx)` so
/// `OnceLock` registers `&SLS_ICONS` rather than locking in the base set.
pub fn install_look_and_icons(ctx: &egui::Context) {
    solidcraft_look::glyphs::install();
    solidcraft_look::themes::install();
    icons::install(&SLS_ICONS);
    solidcraft_look::fonts::install(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sls_icon_aliases_resolve_in_extended_set() {
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
        ] {
            let entry = SLS_ICONS
                .extension()
                .aliases
                .iter()
                .find(|(alias, _, _)| *alias == name);
            assert!(entry.is_some(), "missing SLS icon alias '{name}'");
        }
    }
}
