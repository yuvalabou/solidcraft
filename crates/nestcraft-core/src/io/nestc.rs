//! `.nestc` project archive reader and writer (compressed JSON inside a zip container).

use std::io::{Cursor, Read, Write};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::model::{
    BedOrigin, BuildJob, GroupId, InstanceId, MachineProfile, NoBuildZone, PartGroup, PartId,
    PartInstance, PartPrototype, RotationConstraint, Transform3x4, TriMesh,
};
use crate::{NestCraftError, Result};

const PROJECT_ENTRY: &str = "project.json";

#[derive(Debug, Serialize, Deserialize)]
struct NestcProject {
    version: u32,
    machine: NestcMachine,
    clearance_mm: f32,
    wall_margin_mm: f32,
    min_z_mm: f32,
    bed_origin: NestcBedOrigin,
    #[serde(default = "default_material_id")]
    material_id: String,
    parts: Vec<NestcPart>,
    groups: Vec<NestcGroup>,
    instances: Vec<NestcInstance>,
    no_build_zone: Option<NestcNoBuildZone>,
}

fn default_material_id() -> String {
    "pa2200".to_owned()
}

#[derive(Debug, Serialize, Deserialize)]
enum NestcMachine {
    FormigaP110,
    FarsoonHT403P,
    Custom { name: String, size_mm: [f32; 3] },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum NestcBedOrigin {
    FrontLeft,
    Center,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum NestcRotationConstraint {
    Free,
    ZOnly,
    Fixed,
}

#[derive(Debug, Serialize, Deserialize)]
struct NestcPart {
    id: u32,
    name: String,
    positions: Vec<[f32; 3]>,
    indices: Vec<[u32; 3]>,
    default_constraint: NestcRotationConstraint,
}

#[derive(Debug, Serialize, Deserialize)]
struct NestcGroup {
    id: u32,
    name: String,
    hidden: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct NestcInstance {
    id: u32,
    part_id: u32,
    transform: Transform3x4,
    constraint: NestcRotationConstraint,
    locked: bool,
    hidden: bool,
    #[serde(default)]
    transparent: bool,
    #[serde(default)]
    color: Option<[u8; 3]>,
    group_id: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct NestcNoBuildZone {
    name: String,
    positions: Vec<[f32; 3]>,
    indices: Vec<[u32; 3]>,
    position: [f32; 3],
    rotation_deg: [f32; 3],
    clearance_mm: f32,
    enabled: bool,
    visible: bool,
    locked: bool,
}

fn to_nestc_constraint(c: RotationConstraint) -> NestcRotationConstraint {
    match c {
        RotationConstraint::Free => NestcRotationConstraint::Free,
        RotationConstraint::ZOnly => NestcRotationConstraint::ZOnly,
        RotationConstraint::Fixed => NestcRotationConstraint::Fixed,
    }
}

fn from_nestc_constraint(c: NestcRotationConstraint) -> RotationConstraint {
    match c {
        NestcRotationConstraint::Free => RotationConstraint::Free,
        NestcRotationConstraint::ZOnly => RotationConstraint::ZOnly,
        NestcRotationConstraint::Fixed => RotationConstraint::Fixed,
    }
}

/// Serialize a [`BuildJob`] into a compressed `.nestc` project archive.
pub fn write_nestc(job: &BuildJob) -> Result<Vec<u8>> {
    write_nestc_with_progress(job, None)
}

/// Serialize a [`BuildJob`] into a compressed `.nestc` project archive, reporting `(done, total, stage)`.
pub fn write_nestc_with_progress(
    job: &BuildJob,
    progress: Option<&dyn Fn(usize, usize, &str)>,
) -> Result<Vec<u8>> {
    const TOTAL: usize = 1000;
    if let Some(cb) = progress {
        cb(0, TOTAL, "Preparing parts...");
    }

    let machine = match &job.machine {
        MachineProfile::FormigaP110 => NestcMachine::FormigaP110,
        MachineProfile::FarsoonHT403P => NestcMachine::FarsoonHT403P,
        MachineProfile::Custom { name, size_mm } => NestcMachine::Custom {
            name: name.clone(),
            size_mm: *size_mm,
        },
    };
    let bed_origin = match job.bed_origin {
        BedOrigin::FrontLeft => NestcBedOrigin::FrontLeft,
        BedOrigin::Center => NestcBedOrigin::Center,
    };
    let part_count = job.parts.len().max(1);
    let mut parts = Vec::with_capacity(job.parts.len());
    for (idx, p) in job.parts.iter().enumerate() {
        parts.push(NestcPart {
            id: p.id.0,
            name: p.name.clone(),
            positions: p.mesh.positions.clone(),
            indices: p.mesh.indices.clone(),
            default_constraint: to_nestc_constraint(p.default_constraint),
        });
        if let Some(cb) = progress {
            cb((150 * (idx + 1)) / part_count, TOTAL, "Preparing parts...");
        }
    }
    let groups = job
        .groups
        .iter()
        .map(|g| NestcGroup {
            id: g.id.0,
            name: g.name.clone(),
            hidden: g.hidden,
        })
        .collect();
    let instances = job
        .instances
        .iter()
        .map(|i| NestcInstance {
            id: i.id.0,
            part_id: i.part_id.0,
            transform: i.transform,
            constraint: to_nestc_constraint(i.constraint),
            locked: i.locked,
            hidden: i.hidden,
            transparent: i.transparent,
            color: i.color,
            group_id: i.group_id.map(|g| g.0),
        })
        .collect();
    let no_build_zone = job.no_build_zone.as_ref().map(|z| NestcNoBuildZone {
        name: z.name.clone(),
        positions: z.mesh.positions.clone(),
        indices: z.mesh.indices.clone(),
        position: z.position,
        rotation_deg: z.rotation_deg,
        clearance_mm: z.clearance_mm,
        enabled: z.enabled,
        visible: z.visible,
        locked: z.locked,
    });

    let project = NestcProject {
        version: 1,
        machine,
        clearance_mm: job.clearance_mm,
        wall_margin_mm: job.wall_margin_mm,
        min_z_mm: job.min_z_mm,
        bed_origin,
        material_id: job.material_id.clone(),
        parts,
        groups,
        instances,
        no_build_zone,
    };

    if let Some(cb) = progress {
        cb(150, TOTAL, "Serializing project...");
    }
    let json = serde_json::to_vec(&project)
        .map_err(|e| NestCraftError::InvalidInput(format!("failed to serialize .nestc project: {e}")))?;

    if let Some(cb) = progress {
        cb(450, TOTAL, "Compressing archive...");
    }
    let mut out = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut out);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file(PROJECT_ENTRY, opts)
            .map_err(|e| NestCraftError::InvalidInput(format!("failed to write .nestc archive: {e}")))?;

        let total_bytes = json.len().max(1);
        let mut written = 0usize;
        for chunk in json.chunks(64 * 1024) {
            zip.write_all(chunk)
                .map_err(|e| NestCraftError::InvalidInput(format!("failed to write .nestc payload: {e}")))?;
            written += chunk.len();
            if let Some(cb) = progress {
                let pct = 450 + ((written * 500) / total_bytes).min(500);
                cb(pct, TOTAL, "Compressing archive...");
            }
        }

        zip.finish()
            .map_err(|e| NestCraftError::InvalidInput(format!("failed to finalize .nestc archive: {e}")))?;
    }
    if let Some(cb) = progress {
        cb(950, TOTAL, "Finalizing archive...");
    }
    Ok(out.into_inner())
}

/// Deserialize a `.nestc` project archive into a [`BuildJob`].
pub fn read_nestc(bytes: &[u8]) -> Result<BuildJob> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| NestCraftError::InvalidInput(format!("invalid .nestc archive: {e}")))?;
    let mut entry = archive
        .by_name(PROJECT_ENTRY)
        .map_err(|e| NestCraftError::InvalidInput(format!("missing {PROJECT_ENTRY} in .nestc: {e}")))?;
    let mut json = Vec::new();
    entry
        .read_to_end(&mut json)
        .map_err(|e| NestCraftError::InvalidInput(format!("failed to read .nestc payload: {e}")))?;
    let project: NestcProject = serde_json::from_slice(&json)
        .map_err(|e| NestCraftError::InvalidInput(format!("invalid .nestc project JSON: {e}")))?;

    let machine = match project.machine {
        NestcMachine::FormigaP110 => MachineProfile::FormigaP110,
        NestcMachine::FarsoonHT403P => MachineProfile::FarsoonHT403P,
        NestcMachine::Custom { name, size_mm } => MachineProfile::custom(name, size_mm),
    };
    let mut job = BuildJob::new(machine);
    job.clearance_mm = project.clearance_mm;
    job.wall_margin_mm = project.wall_margin_mm;
    job.min_z_mm = project.min_z_mm;
    job.bed_origin = match project.bed_origin {
        NestcBedOrigin::FrontLeft => BedOrigin::FrontLeft,
        NestcBedOrigin::Center => BedOrigin::Center,
    };
    job.material_id = project.material_id;

    let mut max_part_id = 0_u32;
    for p in project.parts {
        max_part_id = max_part_id.max(p.id);
        let mesh = TriMesh::from_indexed(p.positions, p.indices)?;
        job.parts.push(PartPrototype {
            id: PartId(p.id),
            name: p.name,
            mesh: Arc::new(mesh),
            default_constraint: from_nestc_constraint(p.default_constraint),
        });
    }

    let mut max_group_id = 0_u32;
    for g in project.groups {
        max_group_id = max_group_id.max(g.id);
        job.groups.push(PartGroup {
            id: GroupId(g.id),
            name: g.name,
            hidden: g.hidden,
        });
    }

    let mut max_inst_id = 0_u32;
    for i in project.instances {
        max_inst_id = max_inst_id.max(i.id);
        job.instances.push(PartInstance {
            id: InstanceId(i.id),
            part_id: PartId(i.part_id),
            transform: i.transform,
            constraint: from_nestc_constraint(i.constraint),
            locked: i.locked,
            hidden: i.hidden,
            transparent: i.transparent,
            color: i.color,
            group_id: i.group_id.map(GroupId),
        });
    }

    if let Some(z) = project.no_build_zone {
        let mesh = TriMesh::from_indexed(z.positions, z.indices)?;
        job.no_build_zone = Some(NoBuildZone {
            name: z.name,
            mesh: Arc::new(mesh),
            position: z.position,
            rotation_deg: z.rotation_deg,
            clearance_mm: z.clearance_mm,
            enabled: z.enabled,
            visible: z.visible,
            locked: z.locked,
        });
    }

    job.next_part_id = max_part_id.saturating_add(1).max(1);
    job.next_instance_id = max_inst_id.saturating_add(1).max(1);
    job.next_group_id = max_group_id.saturating_add(1).max(1);
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transform_from_translation_euler_deg;

    #[test]
    fn roundtrips_build_job_through_nestc() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
                vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
            )
            .expect("valid tet"),
        );
        let mut job = BuildJob::new(MachineProfile::FarsoonHT403P);
        job.min_z_mm = 3.0;
        job.wall_margin_mm = 8.0;
        let pid = job.add_part("tet", mesh.clone());
        let gid = job.create_group("Batch 1");
        let iid = job
            .add_instance(pid, transform_from_translation_euler_deg([10.0, 20.0, 3.0], [0.0, 0.0, 45.0]))
            .expect("placed");
        assert!(job.set_instance_group(iid, Some(gid)));
        job.no_build_zone = Some(NoBuildZone::new("Zone A", mesh));

        let bytes = write_nestc(&job).expect("write nestc");
        let loaded = read_nestc(&bytes).expect("read nestc");
        assert_eq!(loaded.bed_origin, BedOrigin::Center);
        assert!((loaded.min_z_mm - 3.0).abs() < 1e-4);
        assert!((loaded.wall_margin_mm - 8.0).abs() < 1e-4);
        assert_eq!(loaded.parts.len(), 1);
        assert_eq!(loaded.groups.len(), 1);
        assert_eq!(loaded.instances.len(), 1);
        assert_eq!(loaded.instances[0].group_id, Some(gid));
        assert!(loaded.no_build_zone.is_some());
    }

    #[test]
    fn write_nestc_with_progress_reports_stages_and_roundtrips() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
                vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
            )
            .expect("valid tet"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("tet", mesh);
        let _ = job.add_instance_centered_on_floor(pid).expect("placed");

        let updates = std::cell::RefCell::new(Vec::<(usize, usize, String)>::new());
        let bytes = write_nestc_with_progress(
            &job,
            Some(&|done, total, stage| {
                updates.borrow_mut().push((done, total, stage.to_owned()));
            }),
        )
        .expect("write nestc with progress");

        let updates = updates.into_inner();
        assert!(updates.len() >= 4, "expected multiple stage updates");
        assert!(updates.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(updates.iter().all(|u| u.1 == 1000 && !u.2.is_empty()));
        assert_eq!(updates.last().map(|u| u.0), Some(950));

        let loaded = read_nestc(&bytes).expect("read nestc");
        assert_eq!(loaded.parts.len(), 1);
        assert_eq!(loaded.instances.len(), 1);
    }
}
