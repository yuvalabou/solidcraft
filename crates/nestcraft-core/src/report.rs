//! Build report mockup & SLS material/machine pricing estimation.

use crate::manifold;
use crate::model::{Aabb, BuildJob, MachineProfile};

/// SLS powder material properties (sourced from `quoting_editor/data/materials.js`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlsMaterial {
    /// Stable identifier (e.g. `"pa2200"`).
    pub id: &'static str,
    /// Human-readable material name.
    pub name: &'static str,
    /// Un-sintered cake bulk density in g/cm³.
    pub bulk_density_g_cm3: f64,
    /// Sintered part density in g/cm³.
    pub sintered_density_g_cm3: f64,
    /// Virgin powder price in USD/kg (or configured currency).
    pub powder_price_per_kg: f64,
    /// Powder refresh/recovery fraction (`0.0..=1.0`).
    pub recovery_rate: f64,
}

/// Built-in SLS material catalog.
pub const SLS_MATERIALS: &[SlsMaterial] = &[
    SlsMaterial {
        id: "pa2200",
        name: "PA 2200 (Nylon 12)",
        bulk_density_g_cm3: 0.45,
        sintered_density_g_cm3: 0.93,
        powder_price_per_kg: 227.5,
        recovery_rate: 0.50,
    },
    SlsMaterial {
        id: "fs3300",
        name: "FS3300PA",
        bulk_density_g_cm3: 0.48,
        sintered_density_g_cm3: 0.95,
        powder_price_per_kg: 175.0,
        recovery_rate: 0.50,
    },
    SlsMaterial {
        id: "pa2210fr",
        name: "PA2210FR (Flame Retardant)",
        bulk_density_g_cm3: 0.45,
        sintered_density_g_cm3: 1.06,
        powder_price_per_kg: 240.0,
        recovery_rate: 0.50,
    },
    SlsMaterial {
        id: "fs3401gb",
        name: "FS3401GB (Glass Bead PA12)",
        bulk_density_g_cm3: 0.75,
        sintered_density_g_cm3: 1.25,
        powder_price_per_kg: 210.0,
        recovery_rate: 0.70,
    },
    SlsMaterial {
        id: "pbt01",
        name: "PBT-01",
        bulk_density_g_cm3: 0.68,
        sintered_density_g_cm3: 1.20,
        powder_price_per_kg: 290.0,
        recovery_rate: 0.30,
    },
    SlsMaterial {
        id: "pa3200gb",
        name: "PA3200GB (Glass Bead)",
        bulk_density_g_cm3: 0.75,
        sintered_density_g_cm3: 1.25,
        powder_price_per_kg: 210.0,
        recovery_rate: 0.70,
    },
];

/// Look up an SLS material by `id`, falling back to `PA 2200`.
pub fn material_by_id(id: &str) -> &'static SlsMaterial {
    SLS_MATERIALS
        .iter()
        .find(|m| m.id == id)
        .unwrap_or(&SLS_MATERIALS[0])
}

/// Machine-specific build & pricing defaults (from `quoting_editor/data/machines.js`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MachinePricingSpec {
    /// Bottom powder pillow / warm-up height in mm.
    pub pillow_z_mm: f64,
    /// Fixed machine/operator cost per build.
    pub machine_cost_per_build: f64,
    /// Average vertical build speed in mm/hour.
    pub build_rate_mm_per_hr: f64,
    /// Setup + cool-down + breakout hours.
    pub setup_hrs: f64,
    /// Commercial markup multiplier.
    pub margin: f64,
}

/// Return pricing defaults matched to the selected `MachineProfile`.
pub fn machine_pricing_spec(machine: &MachineProfile) -> MachinePricingSpec {
    let name = machine.name().to_ascii_lowercase();
    if name.contains("403") || name.contains("farsoon") {
        MachinePricingSpec {
            pillow_z_mm: 13.0,
            machine_cost_per_build: 250.0,
            build_rate_mm_per_hr: 15.0,
            setup_hrs: 2.0,
            margin: 1.30,
        }
    } else if name.contains("p100") {
        MachinePricingSpec {
            pillow_z_mm: 2.0,
            machine_cost_per_build: 200.0,
            build_rate_mm_per_hr: 14.0,
            setup_hrs: 0.5,
            margin: 1.30,
        }
    } else {
        MachinePricingSpec {
            pillow_z_mm: 10.0,
            machine_cost_per_build: 200.0,
            build_rate_mm_per_hr: 16.0,
            setup_hrs: 0.5,
            margin: 1.30,
        }
    }
}

/// Return `true` when `wb` lies completely inside `job`'s machine build envelope.
pub fn is_bounds_inside_machine(job: &BuildJob, wb: Aabb) -> bool {
    const EPS: f32 = 1e-2;
    let ([min_x, min_y], [max_x, max_y]) = job.bed_xy_bounds();
    let max_z = job.machine.size_mm()[2];
    wb[0][0] >= min_x - EPS
        && wb[1][0] <= max_x + EPS
        && wb[0][1] >= min_y - EPS
        && wb[1][1] <= max_y + EPS
        && wb[0][2] >= job.min_z_mm - EPS
        && wb[1][2] <= max_z + EPS
}

/// Per-instance line item in a generated build report.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportPartRow {
    pub instance_id: u32,
    pub name: String,
    pub size_mm: [f32; 3],
    pub volume_cm3: f64,
    pub area_cm2: f64,
    pub min_z_mm: f32,
    pub max_z_mm: f32,
    pub is_manifold: bool,
}

/// Mockup build report generated from a `BuildJob` for the selected machine & material.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildReport {
    pub machine_name: String,
    pub machine_size_mm: [f32; 3],
    pub material: SlsMaterial,
    pub spec: MachinePricingSpec,
    pub included_parts: Vec<ReportPartRow>,
    pub excluded_oob_count: usize,
    pub hidden_excluded_count: usize,
    pub total_part_volume_cm3: f64,
    pub total_surface_area_cm2: f64,
    pub build_max_z_mm: f64,
    pub total_build_height_mm: f64,
    pub cake_volume_cm3: f64,
    pub sintered_mass_g: f64,
    pub fresh_powder_kg: f64,
    pub packing_density_pct: f64,
    pub est_build_hours: f64,
    pub material_cost: f64,
    pub machine_cost: f64,
    pub total_cost: f64,
    pub quoted_price: f64,
}

impl BuildReport {
    /// Build a report accounting only for visible placed parts inside the selected machine.
    pub fn generate(job: &BuildJob) -> Self {
        let material = *material_by_id(&job.material_id);
        let spec = machine_pricing_spec(&job.machine);
        let machine_size_mm = job.machine.size_mm();

        let mut included_parts = Vec::new();
        let mut excluded_oob_count = 0usize;
        let mut hidden_excluded_count = 0usize;
        let mut total_part_volume_cm3 = 0.0_f64;
        let mut total_surface_area_cm2 = 0.0_f64;
        let mut build_max_z_mm = 0.0_f64;

        for inst in &job.instances {
            if !job.is_instance_visible(inst) {
                hidden_excluded_count += 1;
                continue;
            }
            let Some(proto) = job.part(inst.part_id) else {
                continue;
            };
            let wb = inst.world_bounds(proto.mesh.bounds);
            if !is_bounds_inside_machine(job, wb) {
                excluded_oob_count += 1;
                continue;
            }
            let vol_cm3 = proto.mesh.volume_mm3 / 1_000.0;
            let area_cm2 = proto.mesh.surface_area_mm2 / 100.0;
            let is_manifold = manifold::analyze_mesh(&proto.mesh).is_watertight_manifold();
            total_part_volume_cm3 += vol_cm3;
            total_surface_area_cm2 += area_cm2;
            build_max_z_mm = build_max_z_mm.max(f64::from(wb[1][2]));
            included_parts.push(ReportPartRow {
                instance_id: inst.id.0,
                name: proto.name.clone(),
                size_mm: [wb[1][0] - wb[0][0], wb[1][1] - wb[0][1], wb[1][2] - wb[0][2]],
                volume_cm3: vol_cm3,
                area_cm2,
                min_z_mm: wb[0][2],
                max_z_mm: wb[1][2],
                is_manifold,
            });
        }

        let total_build_height_mm = if included_parts.is_empty() {
            0.0
        } else {
            build_max_z_mm + spec.pillow_z_mm
        };
        let bed_area_cm2 = f64::from(machine_size_mm[0]) * f64::from(machine_size_mm[1]) / 100.0;
        let cake_volume_cm3 = bed_area_cm2 * (total_build_height_mm / 10.0);
        let packing_density_pct = if cake_volume_cm3 > 1e-6 {
            (total_part_volume_cm3 / cake_volume_cm3) * 100.0
        } else {
            0.0
        };

        let sintered_mass_g = total_part_volume_cm3 * material.sintered_density_g_cm3;
        let unsintered_cake_cm3 = (cake_volume_cm3 - total_part_volume_cm3).max(0.0);
        let unrecovered_powder_g =
            unsintered_cake_cm3 * material.bulk_density_g_cm3 * (1.0 - material.recovery_rate);
        let fresh_powder_kg = (sintered_mass_g + unrecovered_powder_g) / 1_000.0;

        let est_build_hours = if included_parts.is_empty() {
            0.0
        } else {
            total_build_height_mm / spec.build_rate_mm_per_hr.max(1.0) + spec.setup_hrs
        };
        let material_cost = fresh_powder_kg * material.powder_price_per_kg;
        let machine_cost = if included_parts.is_empty() {
            0.0
        } else {
            spec.machine_cost_per_build
        };
        let total_cost = material_cost + machine_cost;
        let quoted_price = total_cost * spec.margin;

        Self {
            machine_name: job.machine.name().to_owned(),
            machine_size_mm,
            material,
            spec,
            included_parts,
            excluded_oob_count,
            hidden_excluded_count,
            total_part_volume_cm3,
            total_surface_area_cm2,
            build_max_z_mm,
            total_build_height_mm,
            cake_volume_cm3,
            sintered_mass_g,
            fresh_powder_kg,
            packing_density_pct,
            est_build_hours,
            material_cost,
            machine_cost,
            total_cost,
            quoted_price,
        }
    }

    /// Render a printable HTML build report sheet.
    pub fn to_html(&self) -> String {
        let mut rows = String::new();
        for p in &self.included_parts {
            let status = if p.is_manifold { "OK" } else { "Mesh Error" };
            rows.push_str(&format!(
                "<tr><td>#{}</td><td>{}</td><td>{:.1} × {:.1} × {:.1}</td><td>{:.2}</td><td>{:.2}</td><td>{}</td></tr>\n",
                p.instance_id,
                escape_html(&p.name),
                p.size_mm[0],
                p.size_mm[1],
                p.size_mm[2],
                p.volume_cm3,
                p.area_cm2,
                status
            ));
        }
        format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>NestCraft Build Report</title>\
            <style>body{{font-family:sans-serif;margin:32px;color:#1b1f23}}h1{{margin-bottom:4px}}\
            .meta{{color:#555;margin-bottom:20px}}table{{border-collapse:collapse;width:100%;margin-top:16px}}\
            th,td{{border:1px solid #d0d7de;padding:6px 10px;text-align:left;font-size:13px}}th{{background:#f6f8fa}}\
            .grid{{display:grid;grid-template-columns:repeat(4,1fr);gap:12px;margin:16px 0}}\
            .card{{border:1px solid #d0d7de;border-radius:6px;padding:10px}}.lbl{{font-size:11px;color:#57606a}}\
            .val{{font-size:18px;font-weight:600}}</style></head><body>\
            <h1>NestCraft SLS Build Report (Mockup)</h1>\
            <div class=\"meta\">Machine: <b>{}</b> ({:.0}×{:.0}×{:.0} mm) &nbsp;|&nbsp; Material: <b>{}</b> &nbsp;|&nbsp; Excluded out-of-bounds: {}</div>\
            <div class=\"grid\">\
              <div class=\"card\"><div class=\"lbl\">Parts Inside Machine</div><div class=\"val\">{}</div></div>\
              <div class=\"card\"><div class=\"lbl\">Total Part Volume</div><div class=\"val\">{:.2} cm³</div></div>\
              <div class=\"card\"><div class=\"lbl\">Build Height (+{:.0}mm pillow)</div><div class=\"val\">{:.1} mm</div></div>\
              <div class=\"card\"><div class=\"lbl\">Packing Density</div><div class=\"val\">{:.2}%</div></div>\
              <div class=\"card\"><div class=\"lbl\">Fresh Powder</div><div class=\"val\">{:.2} kg</div></div>\
              <div class=\"card\"><div class=\"lbl\">Est. Build Time</div><div class=\"val\">{:.1} h</div></div>\
              <div class=\"card\"><div class=\"lbl\">Material + Machine</div><div class=\"val\">${:.2} + ${:.2}</div></div>\
              <div class=\"card\"><div class=\"lbl\">Quoted Price ({:.2}×)</div><div class=\"val\">${:.2}</div></div>\
            </div>\
            <table><thead><tr><th>ID</th><th>Part</th><th>Dimensions (mm)</th><th>Volume (cm³)</th><th>Area (cm²)</th><th>Mesh</th></tr></thead>\
            <tbody>{}</tbody></table></body></html>",
            escape_html(&self.machine_name),
            self.machine_size_mm[0],
            self.machine_size_mm[1],
            self.machine_size_mm[2],
            escape_html(self.material.name),
            self.excluded_oob_count,
            self.included_parts.len(),
            self.total_part_volume_cm3,
            self.spec.pillow_z_mm,
            self.total_build_height_mm,
            self.packing_density_pct,
            self.fresh_powder_kg,
            self.est_build_hours,
            self.material_cost,
            self.machine_cost,
            self.spec.margin,
            self.quoted_price,
            rows
        )
    }

    /// Render a CSV build summary and part table.
    pub fn to_csv(&self) -> String {
        let mut out = format!(
            "Machine,{}\nMaterial,{}\nParts Inside Machine,{}\nExcluded Out-of-Bounds,{}\nTotal Volume (cm3),{:.3}\nBuild Height (mm),{:.2}\nPacking Density (%),{:.2}\nFresh Powder (kg),{:.3}\nEst Build Time (h),{:.2}\nMaterial Cost,{:.2}\nMachine Cost,{:.2}\nQuoted Price,{:.2}\n\nInstance ID,Part,Size X (mm),Size Y (mm),Size Z (mm),Volume (cm3),Area (cm2),Manifold\n",
            csv_cell(&self.machine_name),
            csv_cell(self.material.name),
            self.included_parts.len(),
            self.excluded_oob_count,
            self.total_part_volume_cm3,
            self.total_build_height_mm,
            self.packing_density_pct,
            self.fresh_powder_kg,
            self.est_build_hours,
            self.material_cost,
            self.machine_cost,
            self.quoted_price,
        );
        for p in &self.included_parts {
            out.push_str(&format!(
                "{},{},{:.2},{:.2},{:.2},{:.3},{:.3},{}\n",
                p.instance_id,
                csv_cell(&p.name),
                p.size_mm[0],
                p.size_mm[1],
                p.size_mm[2],
                p.volume_cm3,
                p.area_cm2,
                if p.is_manifold { "OK" } else { "Error" },
            ));
        }
        out
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TriMesh;
    use std::sync::Arc;

    fn unit_cube_mesh() -> Arc<TriMesh> {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [10.0, 0.0, 0.0],
            [10.0, 10.0, 0.0],
            [0.0, 10.0, 0.0],
            [0.0, 0.0, 10.0],
            [10.0, 0.0, 10.0],
            [10.0, 10.0, 10.0],
            [0.0, 10.0, 10.0],
        ];
        let indices = vec![
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
        ];
        Arc::new(TriMesh::from_indexed(positions, indices).expect("cube"))
    }

    #[test]
    fn report_excludes_out_of_bounds_parts_and_uses_material() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        job.material_id = "fs3300".to_owned();
        let pid = job.add_part("Cube", unit_cube_mesh());
        let tr_in = [[1.0, 0.0, 0.0, 20.0], [0.0, 1.0, 0.0, 20.0], [0.0, 0.0, 1.0, 0.0]];
        let tr_out = [[1.0, 0.0, 0.0, 500.0], [0.0, 1.0, 0.0, 20.0], [0.0, 0.0, 1.0, 0.0]];
        job.add_instance(pid, tr_in).expect("in");
        job.add_instance(pid, tr_out).expect("out");

        let rep = BuildReport::generate(&job);
        assert_eq!(rep.included_parts.len(), 1);
        assert_eq!(rep.excluded_oob_count, 1);
        assert_eq!(rep.material.id, "fs3300");
        assert!(rep.quoted_price > rep.machine_cost);
        assert!(rep.to_html().contains("FS3300PA"));
        assert!(rep.to_csv().contains("Excluded Out-of-Bounds,1"));
    }
}
