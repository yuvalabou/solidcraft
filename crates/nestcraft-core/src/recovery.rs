//! Failed-print / partial-build recovery classification and reprint job extraction.

use std::collections::HashMap;

use crate::model::{BuildJob, InstanceId, PartId};
use crate::report::is_bounds_inside_machine;

/// Print completion status of a placed instance at a failure Z height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FailedPrintStatus {
    /// Fully printed (`max_z <= failed_z_mm`); no reprint needed.
    Completed,
    /// Interrupted mid-print (`min_z < failed_z_mm < max_z`); ruined and must be reprinted.
    Partial { pct_printed: f32 },
    /// Never started (`min_z >= failed_z_mm`); must be reprinted.
    NotStarted,
}

/// Classification row for a single placed instance.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedPrintPartRow {
    pub instance_id: InstanceId,
    pub part_id: PartId,
    pub name: String,
    pub min_z_mm: f32,
    pub max_z_mm: f32,
    pub volume_cm3: f64,
    pub status: FailedPrintStatus,
}

impl FailedPrintPartRow {
    /// Return `true` when this instance must be sent to print again.
    pub fn needs_reprint(&self) -> bool {
        !matches!(self.status, FailedPrintStatus::Completed)
    }
}

/// Summary report classifying a build job against an aborted Z height.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedPrintReport {
    pub failed_z_mm: f32,
    pub job_max_z_mm: f32,
    pub completed_count: usize,
    pub partial_count: usize,
    pub not_started_count: usize,
    pub reprint_volume_cm3: f64,
    pub rows: Vec<FailedPrintPartRow>,
}

impl FailedPrintReport {
    /// Classify all visible placed parts inside `job`'s machine against `failed_z_mm`.
    pub fn classify(job: &BuildJob, failed_z_mm: f32) -> Self {
        const EPS: f32 = 1e-3;
        let z_cut = failed_z_mm.max(0.0);
        let mut job_max_z_mm = 0.0_f32;
        let mut completed_count = 0usize;
        let mut partial_count = 0usize;
        let mut not_started_count = 0usize;
        let mut reprint_volume_cm3 = 0.0_f64;
        let mut rows = Vec::new();

        for inst in &job.instances {
            if !job.is_instance_visible(inst) {
                continue;
            }
            let Some(proto) = job.part(inst.part_id) else {
                continue;
            };
            let wb = inst.world_bounds(proto.mesh.bounds);
            if !is_bounds_inside_machine(job, wb) {
                continue;
            }
            let min_z = wb[0][2];
            let max_z = wb[1][2];
            job_max_z_mm = job_max_z_mm.max(max_z);
            let vol_cm3 = proto.mesh.volume_mm3 / 1_000.0;

            let status = if max_z <= z_cut + EPS {
                completed_count += 1;
                FailedPrintStatus::Completed
            } else if min_z >= z_cut - EPS {
                not_started_count += 1;
                reprint_volume_cm3 += vol_cm3;
                FailedPrintStatus::NotStarted
            } else {
                partial_count += 1;
                reprint_volume_cm3 += vol_cm3;
                let h = (max_z - min_z).max(1e-4);
                let pct = (((z_cut - min_z) / h) * 100.0).clamp(0.0, 100.0);
                FailedPrintStatus::Partial { pct_printed: pct }
            };

            rows.push(FailedPrintPartRow {
                instance_id: inst.id,
                part_id: inst.part_id,
                name: proto.name.clone(),
                min_z_mm: min_z,
                max_z_mm: max_z,
                volume_cm3: vol_cm3,
                status,
            });
        }

        Self {
            failed_z_mm: z_cut,
            job_max_z_mm,
            completed_count,
            partial_count,
            not_started_count,
            reprint_volume_cm3,
            rows,
        }
    }

    /// Return the instance IDs of all parts that need to be reprinted.
    pub fn reprint_instance_ids(&self) -> Vec<InstanceId> {
        self.rows
            .iter()
            .filter(|r| r.needs_reprint())
            .map(|r| r.instance_id)
            .collect()
    }

    /// Export a CSV list of parts that need to be reprinted.
    pub fn to_reprint_csv(&self) -> String {
        let mut out = format!(
            "Failed Z (mm),{:.2}\nCompleted Parts,{}\nPartial (Ruined) Parts,{}\nNot Started Parts,{}\nReprint Volume (cm3),{:.3}\n\nInstance ID,Part,Z Min (mm),Z Max (mm),Printed %,Status\n",
            self.failed_z_mm,
            self.completed_count,
            self.partial_count,
            self.not_started_count,
            self.reprint_volume_cm3,
        );
        for r in &self.rows {
            if !r.needs_reprint() {
                continue;
            }
            let (pct, reason) = match r.status {
                FailedPrintStatus::Partial { pct_printed } => (pct_printed, "Partial (Reprint)"),
                FailedPrintStatus::NotStarted => (0.0, "Not Started (Reprint)"),
                FailedPrintStatus::Completed => (100.0, "Completed"),
            };
            let name = if r.name.contains([',', '"', '\n']) {
                format!("\"{}\"", r.name.replace('"', "\"\""))
            } else {
                r.name.clone()
            };
            out.push_str(&format!(
                "{},{},{:.2},{:.2},{:.1},{}\n",
                r.instance_id.0, name, r.min_z_mm, r.max_z_mm, pct, reason
            ));
        }
        out
    }
}

/// Build a new `BuildJob` containing only the parts that must be reprinted after failure at `failed_z_mm`.
pub fn build_reprint_job(source: &BuildJob, failed_z_mm: f32) -> BuildJob {
    let report = FailedPrintReport::classify(source, failed_z_mm);
    let reprint_ids: std::collections::HashSet<InstanceId> =
        report.reprint_instance_ids().into_iter().collect();

    let mut out = BuildJob::new(source.machine.clone());
    out.clearance_mm = source.clearance_mm;
    out.wall_margin_mm = source.wall_margin_mm;
    out.min_z_mm = source.min_z_mm;
    out.bed_origin = source.bed_origin;
    out.material_id = source.material_id.clone();
    out.no_build_zone = source.no_build_zone.clone();

    let mut part_map: HashMap<PartId, PartId> = HashMap::new();
    for inst in &source.instances {
        if !reprint_ids.contains(&inst.id) {
            continue;
        }
        let Some(proto) = source.part(inst.part_id) else {
            continue;
        };
        let new_pid = *part_map
            .entry(proto.id)
            .or_insert_with(|| out.add_part(proto.name.clone(), proto.mesh.clone()));
        if let Ok(new_iid) = out.add_instance(new_pid, inst.transform)
            && let Some(new_inst) = out.instances.iter_mut().find(|i| i.id == new_iid)
        {
            new_inst.constraint = inst.constraint;
            new_inst.color = inst.color;
            new_inst.transparent = inst.transparent;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MachineProfile, TriMesh};
    use std::sync::Arc;

    fn cube_mesh(side: f32) -> Arc<TriMesh> {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [side, 0.0, 0.0],
            [side, side, 0.0],
            [0.0, side, 0.0],
            [0.0, 0.0, side],
            [side, 0.0, side],
            [side, side, side],
            [0.0, side, side],
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
    fn classifies_completed_partial_and_not_started_and_builds_reprint_job() {
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("Cube10", cube_mesh(10.0));
        // 1: Z 0..10 (completed at Z=15)
        job.add_instance(pid, [[1.0, 0.0, 0.0, 10.0], [0.0, 1.0, 0.0, 10.0], [0.0, 0.0, 1.0, 0.0]])
            .expect("c");
        // 2: Z 10..20 (partial 50% at Z=15)
        job.add_instance(pid, [[1.0, 0.0, 0.0, 30.0], [0.0, 1.0, 0.0, 10.0], [0.0, 0.0, 1.0, 10.0]])
            .expect("p");
        // 3: Z 20..30 (not started at Z=15)
        job.add_instance(pid, [[1.0, 0.0, 0.0, 50.0], [0.0, 1.0, 0.0, 10.0], [0.0, 0.0, 1.0, 20.0]])
            .expect("n");

        let rep = FailedPrintReport::classify(&job, 15.0);
        assert_eq!(rep.completed_count, 1);
        assert_eq!(rep.partial_count, 1);
        assert_eq!(rep.not_started_count, 1);
        assert_eq!(rep.reprint_instance_ids().len(), 2);
        assert!(rep.to_reprint_csv().contains("Partial (Reprint)"));
        assert!(rep.to_reprint_csv().contains("Not Started (Reprint)"));

        let reprint = build_reprint_job(&job, 15.0);
        assert_eq!(reprint.instances.len(), 2);
    }
}
