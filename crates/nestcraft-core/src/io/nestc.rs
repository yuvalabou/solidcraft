//! `.nestc` project archive reader and writer.
//!
//! Version 2 stores compact human-readable `project.json` metadata alongside
//! separate content-addressed, lossless binary mesh entries (`meshes/<sha256>.nmesh`)
//! inside a Deflate ZIP container. Version 1 inline-JSON archives remain readable.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::model::{
    BedOrigin, BuildJob, GroupId, InstanceId, MachineProfile, NoBuildZone, PartGroup, PartId,
    PartInstance, PartPrototype, RotationConstraint, Transform3x4, TriMesh,
};
use crate::{NestCraftError, Result};

const PROJECT_ENTRY: &str = "project.json";
const NESTC_FORMAT_VERSION: u32 = 2;

const NMESH_MAGIC: [u8; 4] = *b"NME1";
const NMESH_VERSION: u16 = 1;
const NMESH_FLAG_POS_SHUFFLED: u16 = 0x0001;
const NMESH_FLAG_IDX_DELTA_VARINT: u16 = 0x0002;
const NMESH_DEFAULT_FLAGS: u16 = NMESH_FLAG_POS_SHUFFLED | NMESH_FLAG_IDX_DELTA_VARINT;
const NMESH_KNOWN_FLAGS: u16 = NMESH_FLAG_POS_SHUFFLED | NMESH_FLAG_IDX_DELTA_VARINT;
const NMESH_HEADER_BYTES: usize = 16;

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

/// Summary geometry metadata stored in `project.json` so browsers/reports can inspect parts
/// without decompressing `.nmesh` geometry blobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NestcMeshMeta {
    vertex_count: u32,
    triangle_count: u32,
    bounds: [[f32; 3]; 2],
    volume_mm3: f64,
    surface_area_mm2: f64,
}

impl NestcMeshMeta {
    fn from_mesh(mesh: &TriMesh) -> Result<Self> {
        let vertex_count = u32::try_from(mesh.positions.len())
            .map_err(|_| NestCraftError::InvalidInput("mesh vertex count exceeds u32".to_owned()))?;
        let triangle_count = u32::try_from(mesh.indices.len())
            .map_err(|_| NestCraftError::InvalidInput("mesh triangle count exceeds u32".to_owned()))?;
        Ok(Self {
            vertex_count,
            triangle_count,
            bounds: mesh.bounds,
            volume_mm3: mesh.volume_mm3,
            surface_area_mm2: mesh.surface_area_mm2,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct NestcPart {
    id: u32,
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    positions: Vec<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    indices: Vec<[u32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mesh_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mesh_meta: Option<NestcMeshMeta>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    positions: Vec<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    indices: Vec<[u32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mesh_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mesh_meta: Option<NestcMeshMeta>,
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

/// Encode a [`TriMesh`] into a lossless `.nmesh` binary blob using the default filters
/// (byte-lane shuffled IEEE-754 positions + delta zigzag varint triangle indices).
pub fn encode_nmesh(mesh: &TriMesh) -> Result<Vec<u8>> {
    encode_nmesh_with_flags(mesh, NMESH_DEFAULT_FLAGS)
}

fn encode_nmesh_with_flags(mesh: &TriMesh, flags: u16) -> Result<Vec<u8>> {
    if (flags & !NMESH_KNOWN_FLAGS) != 0 {
        return Err(NestCraftError::InvalidInput(format!("unsupported nmesh flags: {flags:#x}")));
    }
    if mesh.positions.is_empty() || mesh.indices.is_empty() {
        return Err(NestCraftError::InvalidInput("cannot encode empty mesh".to_owned()));
    }
    let v_count = u32::try_from(mesh.positions.len())
        .map_err(|_| NestCraftError::InvalidInput("mesh vertex count exceeds u32".to_owned()))?;
    let t_count = u32::try_from(mesh.indices.len())
        .map_err(|_| NestCraftError::InvalidInput("mesh triangle count exceeds u32".to_owned()))?;

    let pos_bytes = mesh.positions.len().saturating_mul(12);
    let idx_est = if (flags & NMESH_FLAG_IDX_DELTA_VARINT) != 0 {
        mesh.indices.len().saturating_mul(6)
    } else {
        mesh.indices.len().saturating_mul(12)
    };
    let mut out = Vec::with_capacity(NMESH_HEADER_BYTES + pos_bytes + idx_est);

    out.extend_from_slice(&NMESH_MAGIC);
    out.extend_from_slice(&NMESH_VERSION.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&v_count.to_le_bytes());
    out.extend_from_slice(&t_count.to_le_bytes());

    if (flags & NMESH_FLAG_POS_SHUFFLED) != 0 {
        for axis in 0..3 {
            for lane in 0..4 {
                for p in &mesh.positions {
                    let b = p[axis].to_bits().to_le_bytes();
                    out.push(b[lane]);
                }
            }
        }
    } else {
        for p in &mesh.positions {
            for c in p {
                out.extend_from_slice(&c.to_bits().to_le_bytes());
            }
        }
    }

    if (flags & NMESH_FLAG_IDX_DELTA_VARINT) != 0 {
        let mut prev: i64 = 0;
        for tri in &mesh.indices {
            for &idx in tri {
                let cur = i64::from(idx);
                let delta = cur - prev;
                let zz = ((delta << 1) ^ (delta >> 63)) as u64;
                write_uvarint(zz, &mut out);
                prev = cur;
            }
        }
    } else {
        for tri in &mesh.indices {
            for &idx in tri {
                out.extend_from_slice(&idx.to_le_bytes());
            }
        }
    }

    Ok(out)
}

fn write_uvarint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn read_uvarint(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut result = 0_u64;
    let mut shift = 0_u32;
    for step in 0..10 {
        let b = *bytes
            .get(*cursor)
            .ok_or_else(|| NestCraftError::InvalidInput("truncated varint in .nmesh".to_owned()))?;
        *cursor += 1;
        if step == 9 && (b & 0xfe) != 0 {
            return Err(NestCraftError::InvalidInput("overflowing varint in .nmesh".to_owned()));
        }
        result |= u64::from(b & 0x7f) << shift;
        if (b & 0x80) == 0 {
            return Ok(result);
        }
        shift += 7;
    }
    Err(NestCraftError::InvalidInput("overlong varint in .nmesh".to_owned()))
}

/// Decode a lossless `.nmesh` binary blob back into a validated [`TriMesh`].
pub fn decode_nmesh(bytes: &[u8]) -> Result<TriMesh> {
    let header = bytes
        .get(..NMESH_HEADER_BYTES)
        .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh header".to_owned()))?;
    if header[0..4] != NMESH_MAGIC {
        return Err(NestCraftError::InvalidInput("invalid .nmesh magic".to_owned()));
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != NMESH_VERSION {
        return Err(NestCraftError::InvalidInput(format!("unsupported .nmesh version {version}")));
    }
    let flags = u16::from_le_bytes([header[6], header[7]]);
    if (flags & !NMESH_KNOWN_FLAGS) != 0 {
        return Err(NestCraftError::InvalidInput(format!("unknown .nmesh flags {flags:#x}")));
    }
    let v_count = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    let t_count = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
    if v_count == 0 || t_count == 0 {
        return Err(NestCraftError::InvalidInput(".nmesh has zero vertices or triangles".to_owned()));
    }

    let pos_bytes = v_count
        .checked_mul(12)
        .ok_or_else(|| NestCraftError::InvalidInput(".nmesh vertex count overflow".to_owned()))?;
    let body = bytes
        .get(NMESH_HEADER_BYTES..)
        .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh body".to_owned()))?;
    let pos_slice = body
        .get(..pos_bytes)
        .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh vertex payload".to_owned()))?;
    let idx_slice = body
        .get(pos_bytes..)
        .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh index payload".to_owned()))?;

    if (flags & NMESH_FLAG_IDX_DELTA_VARINT) != 0 {
        let min_idx_bytes = t_count
            .checked_mul(3)
            .ok_or_else(|| NestCraftError::InvalidInput(".nmesh triangle count overflow".to_owned()))?;
        if idx_slice.len() < min_idx_bytes {
            return Err(NestCraftError::InvalidInput("truncated .nmesh varint index payload".to_owned()));
        }
    } else {
        let exact_idx_bytes = t_count
            .checked_mul(12)
            .ok_or_else(|| NestCraftError::InvalidInput(".nmesh triangle count overflow".to_owned()))?;
        if idx_slice.len() != exact_idx_bytes {
            return Err(NestCraftError::InvalidInput("invalid .nmesh raw index payload length".to_owned()));
        }
    }

    let mut positions = Vec::with_capacity(v_count);
    if (flags & NMESH_FLAG_POS_SHUFFLED) != 0 {
        let axis_stride = v_count * 4;
        let x_planes = pos_slice
            .get(0..axis_stride)
            .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh X planes".to_owned()))?;
        let y_planes = pos_slice
            .get(axis_stride..axis_stride * 2)
            .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh Y planes".to_owned()))?;
        let z_planes = pos_slice
            .get(axis_stride * 2..axis_stride * 3)
            .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh Z planes".to_owned()))?;
        for i in 0..v_count {
            let x = read_plane_f32(x_planes, v_count, i)?;
            let y = read_plane_f32(y_planes, v_count, i)?;
            let z = read_plane_f32(z_planes, v_count, i)?;
            if !x.is_finite() || !y.is_finite() || !z.is_finite() {
                return Err(NestCraftError::InvalidInput(".nmesh contains non-finite vertex".to_owned()));
            }
            positions.push([x, y, z]);
        }
    } else {
        for chunk in pos_slice.as_chunks::<12>().0 {
            let x = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let y = f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let z = f32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
            if !x.is_finite() || !y.is_finite() || !z.is_finite() {
                return Err(NestCraftError::InvalidInput(".nmesh contains non-finite vertex".to_owned()));
            }
            positions.push([x, y, z]);
        }
    }

    let mut indices = Vec::with_capacity(t_count);
    let v_limit = i64::from(
        u32::try_from(v_count)
            .map_err(|_| NestCraftError::InvalidInput(".nmesh vertex count exceeds u32".to_owned()))?,
    );
    if (flags & NMESH_FLAG_IDX_DELTA_VARINT) != 0 {
        let mut cursor = 0_usize;
        let mut prev: i64 = 0;
        for _ in 0..t_count {
            let mut tri = [0_u32; 3];
            for slot in &mut tri {
                let zz = read_uvarint(idx_slice, &mut cursor)?;
                let delta = ((zz >> 1) as i64) ^ -((zz & 1) as i64);
                let cur = prev
                    .checked_add(delta)
                    .ok_or_else(|| NestCraftError::InvalidInput(".nmesh index delta overflow".to_owned()))?;
                if cur < 0 || cur >= v_limit {
                    return Err(NestCraftError::InvalidInput(".nmesh triangle index out of range".to_owned()));
                }
                *slot = u32::try_from(cur)
                    .map_err(|_| NestCraftError::InvalidInput(".nmesh index conversion error".to_owned()))?;
                prev = cur;
            }
            indices.push(tri);
        }
        if cursor != idx_slice.len() {
            return Err(NestCraftError::InvalidInput("trailing bytes in .nmesh index stream".to_owned()));
        }
    } else {
        for chunk in idx_slice.as_chunks::<12>().0 {
            let i0 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let i1 = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let i2 = u32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
            if (i0 as usize) >= v_count || (i1 as usize) >= v_count || (i2 as usize) >= v_count {
                return Err(NestCraftError::InvalidInput(".nmesh raw triangle index out of range".to_owned()));
            }
            indices.push([i0, i1, i2]);
        }
    }

    TriMesh::from_indexed(positions, indices)
}

fn read_plane_f32(planes: &[u8], v_count: usize, idx: usize) -> Result<f32> {
    let b0 = *planes
        .get(idx)
        .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh plane 0".to_owned()))?;
    let b1 = *planes
        .get(v_count + idx)
        .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh plane 1".to_owned()))?;
    let b2 = *planes
        .get(v_count * 2 + idx)
        .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh plane 2".to_owned()))?;
    let b3 = *planes
        .get(v_count * 3 + idx)
        .ok_or_else(|| NestCraftError::InvalidInput("truncated .nmesh plane 3".to_owned()))?;
    Ok(f32::from_le_bytes([b0, b1, b2, b3]))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

fn validate_mesh_ref(path: &str) -> Result<()> {
    if !path.starts_with("meshes/") || !path.ends_with(".nmesh") || path.contains("..") {
        return Err(NestCraftError::InvalidInput(format!("invalid .nestc mesh_ref '{path}'")));
    }
    Ok(())
}

/// Serialize a [`BuildJob`] into a compressed `.nestc` v2 project archive.
pub fn write_nestc(job: &BuildJob) -> Result<Vec<u8>> {
    write_nestc_with_progress(job, None)
}

/// Serialize a [`BuildJob`] into a compressed `.nestc` v2 project archive, reporting `(done, total, stage)`.
pub fn write_nestc_with_progress(
    job: &BuildJob,
    progress: Option<&dyn Fn(usize, usize, &str)>,
) -> Result<Vec<u8>> {
    const TOTAL: usize = 1000;
    if let Some(cb) = progress {
        cb(0, TOTAL, "Preparing parts...");
    }

    let referenced_parts: HashSet<PartId> = job.instances.iter().map(|i| i.part_id).collect();
    let active_parts: Vec<&PartPrototype> = job
        .parts
        .iter()
        .filter(|p| referenced_parts.contains(&p.id))
        .collect();

    // Collect unique Arc<TriMesh> pointers to avoid encoding shared meshes twice.
    let mut unique_meshes: Vec<(usize, Arc<TriMesh>)> = Vec::new();
    let mut seen_ptrs: HashSet<usize> = HashSet::new();
    for p in &active_parts {
        let ptr = Arc::as_ptr(&p.mesh) as usize;
        if seen_ptrs.insert(ptr) {
            unique_meshes.push((ptr, Arc::clone(&p.mesh)));
        }
    }
    if let Some(z) = &job.no_build_zone {
        let ptr = Arc::as_ptr(&z.mesh) as usize;
        if seen_ptrs.insert(ptr) {
            unique_meshes.push((ptr, Arc::clone(&z.mesh)));
        }
    }

    if let Some(cb) = progress {
        cb(150, TOTAL, "Encoding binary meshes...");
    }

    let encoded_items: Vec<(usize, String, Vec<u8>)> = encode_unique_meshes(&unique_meshes)?;
    let mut ptr_to_ref: HashMap<usize, String> = HashMap::with_capacity(encoded_items.len());
    let mut blobs_by_ref: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (ptr, mesh_ref, blob) in encoded_items {
        ptr_to_ref.insert(ptr, mesh_ref.clone());
        blobs_by_ref.entry(mesh_ref).or_insert(blob);
    }

    if let Some(cb) = progress {
        cb(300, TOTAL, "Serializing project metadata...");
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

    let mut parts = Vec::with_capacity(active_parts.len());
    for p in active_parts {
        let ptr = Arc::as_ptr(&p.mesh) as usize;
        let mesh_ref = ptr_to_ref
            .get(&ptr)
            .cloned()
            .ok_or_else(|| NestCraftError::InvalidInput("missing encoded mesh ref".to_owned()))?;
        parts.push(NestcPart {
            id: p.id.0,
            name: p.name.clone(),
            positions: Vec::new(),
            indices: Vec::new(),
            mesh_ref: Some(mesh_ref),
            mesh_meta: Some(NestcMeshMeta::from_mesh(&p.mesh)?),
            default_constraint: to_nestc_constraint(p.default_constraint),
        });
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
    let no_build_zone = match &job.no_build_zone {
        Some(z) => {
            let ptr = Arc::as_ptr(&z.mesh) as usize;
            let mesh_ref = ptr_to_ref
                .get(&ptr)
                .cloned()
                .ok_or_else(|| NestCraftError::InvalidInput("missing encoded NBZ mesh ref".to_owned()))?;
            Some(NestcNoBuildZone {
                name: z.name.clone(),
                positions: Vec::new(),
                indices: Vec::new(),
                mesh_ref: Some(mesh_ref),
                mesh_meta: Some(NestcMeshMeta::from_mesh(&z.mesh)?),
                position: z.position,
                rotation_deg: z.rotation_deg,
                clearance_mm: z.clearance_mm,
                enabled: z.enabled,
                visible: z.visible,
                locked: z.locked,
            })
        }
        None => None,
    };

    let project = NestcProject {
        version: NESTC_FORMAT_VERSION,
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

    let json = serde_json::to_vec(&project)
        .map_err(|e| NestCraftError::InvalidInput(format!("failed to serialize .nestc project: {e}")))?;

    if let Some(cb) = progress {
        cb(450, TOTAL, "Compressing archive...");
    }

    let total_bytes: usize = json.len() + blobs_by_ref.values().map(Vec::len).sum::<usize>();
    let total_bytes = total_bytes.max(1);
    let mut written = 0usize;

    let mut out = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut out);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        write_zip_entry_chunked(
            &mut zip,
            PROJECT_ENTRY,
            &json,
            opts,
            &mut written,
            total_bytes,
            progress,
        )?;
        for (mesh_ref, blob) in &blobs_by_ref {
            write_zip_entry_chunked(
                &mut zip,
                mesh_ref,
                blob,
                opts,
                &mut written,
                total_bytes,
                progress,
            )?;
        }

        zip.finish()
            .map_err(|e| NestCraftError::InvalidInput(format!("failed to finalize .nestc archive: {e}")))?;
    }
    if let Some(cb) = progress {
        cb(950, TOTAL, "Finalizing archive...");
    }
    Ok(out.into_inner())
}

fn encode_unique_meshes(unique_meshes: &[(usize, Arc<TriMesh>)]) -> Result<Vec<(usize, String, Vec<u8>)>> {
    if unique_meshes.len() <= 1 {
        let mut out = Vec::with_capacity(unique_meshes.len());
        for (ptr, mesh) in unique_meshes {
            let blob = encode_nmesh(mesh)?;
            let mesh_ref = format!("meshes/{}.nmesh", sha256_hex(&blob));
            out.push((*ptr, mesh_ref, blob));
        }
        return Ok(out);
    }

    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(unique_meshes.len())
        .clamp(1, 16);
    let chunk_size = unique_meshes.len().div_ceil(threads);

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(threads);
        for chunk in unique_meshes.chunks(chunk_size) {
            handles.push(s.spawn(move || -> Result<Vec<(usize, String, Vec<u8>)>> {
                let mut local = Vec::with_capacity(chunk.len());
                for (ptr, mesh) in chunk {
                    let blob = encode_nmesh(mesh)?;
                    let mesh_ref = format!("meshes/{}.nmesh", sha256_hex(&blob));
                    local.push((*ptr, mesh_ref, blob));
                }
                Ok(local)
            }));
        }
        let mut combined = Vec::with_capacity(unique_meshes.len());
        for h in handles {
            let part_res = h
                .join()
                .map_err(|_| NestCraftError::InvalidInput("mesh encoder worker panicked".to_owned()))?;
            combined.extend(part_res?);
        }
        Ok(combined)
    })
}

fn write_zip_entry_chunked(
    zip: &mut ZipWriter<&mut Cursor<Vec<u8>>>,
    entry_name: &str,
    data: &[u8],
    opts: SimpleFileOptions,
    written: &mut usize,
    total_bytes: usize,
    progress: Option<&dyn Fn(usize, usize, &str)>,
) -> Result<()> {
    zip.start_file(entry_name, opts)
        .map_err(|e| NestCraftError::InvalidInput(format!("failed to write .nestc archive entry {entry_name}: {e}")))?;
    for chunk in data.chunks(64 * 1024) {
        zip.write_all(chunk)
            .map_err(|e| NestCraftError::InvalidInput(format!("failed to write .nestc payload: {e}")))?;
        *written += chunk.len();
        if let Some(cb) = progress {
            let pct = 450 + ((*written * 500) / total_bytes).min(500);
            cb(pct, 1000, "Compressing archive...");
        }
    }
    Ok(())
}

/// Deserialize a `.nestc` project archive (v1 inline JSON or v2 hybrid binary) into a [`BuildJob`].
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
    drop(entry);

    let project: NestcProject = serde_json::from_slice(&json)
        .map_err(|e| NestCraftError::InvalidInput(format!("invalid .nestc project JSON: {e}")))?;
    if project.version == 0 || project.version > NESTC_FORMAT_VERSION {
        return Err(NestCraftError::InvalidInput(format!(
            "unsupported .nestc version {}",
            project.version
        )));
    }

    let mut unique_refs_set: HashSet<String> = HashSet::new();
    for p in &project.parts {
        if let Some(ref path) = p.mesh_ref {
            validate_mesh_ref(path)?;
            unique_refs_set.insert(path.clone());
        }
    }
    if let Some(ref z) = project.no_build_zone
        && let Some(ref path) = z.mesh_ref
    {
        validate_mesh_ref(path)?;
        unique_refs_set.insert(path.clone());
    }
    let unique_refs: Vec<String> = unique_refs_set.into_iter().collect();
    let decoded_meshes = decode_unique_mesh_refs(bytes, &mut archive, &unique_refs)?;

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
        let mesh = match p.mesh_ref {
            Some(ref path) => decoded_meshes
                .get(path)
                .cloned()
                .ok_or_else(|| NestCraftError::InvalidInput(format!("missing decoded mesh '{path}'")))?,
            None => Arc::new(TriMesh::from_indexed(p.positions, p.indices)?),
        };
        job.parts.push(PartPrototype {
            id: PartId(p.id),
            name: p.name,
            mesh,
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
        let mesh = match z.mesh_ref {
            Some(ref path) => decoded_meshes
                .get(path)
                .cloned()
                .ok_or_else(|| NestCraftError::InvalidInput(format!("missing decoded NBZ mesh '{path}'")))?,
            None => Arc::new(TriMesh::from_indexed(z.positions, z.indices)?),
        };
        job.no_build_zone = Some(NoBuildZone {
            name: z.name,
            mesh,
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

fn read_and_decode_mesh_entry(archive: &mut ZipArchive<Cursor<&[u8]>>, path: &str) -> Result<Arc<TriMesh>> {
    let mut entry = archive
        .by_name(path)
        .map_err(|e| NestCraftError::InvalidInput(format!("missing mesh entry '{path}' in .nestc: {e}")))?;
    let mut buf = Vec::new();
    entry
        .read_to_end(&mut buf)
        .map_err(|e| NestCraftError::InvalidInput(format!("failed to read mesh entry '{path}': {e}")))?;
    Ok(Arc::new(decode_nmesh(&buf)?))
}

fn decode_unique_mesh_refs(
    bytes: &[u8],
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    unique_refs: &[String],
) -> Result<HashMap<String, Arc<TriMesh>>> {
    if unique_refs.is_empty() {
        return Ok(HashMap::new());
    }
    if unique_refs.len() == 1 {
        let path = &unique_refs[0];
        let mesh = read_and_decode_mesh_entry(archive, path)?;
        let mut map = HashMap::with_capacity(1);
        map.insert(path.clone(), mesh);
        return Ok(map);
    }

    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(unique_refs.len())
        .clamp(1, 16);
    let chunk_size = unique_refs.len().div_ceil(threads);

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(threads);
        for chunk in unique_refs.chunks(chunk_size) {
            handles.push(s.spawn(move || -> Result<Vec<(String, Arc<TriMesh>)>> {
                let mut local_archive = ZipArchive::new(Cursor::new(bytes))
                    .map_err(|e| NestCraftError::InvalidInput(format!("invalid .nestc archive: {e}")))?;
                let mut local = Vec::with_capacity(chunk.len());
                for path in chunk {
                    let mesh = read_and_decode_mesh_entry(&mut local_archive, path)?;
                    local.push((path.clone(), mesh));
                }
                Ok(local)
            }));
        }
        let mut map = HashMap::with_capacity(unique_refs.len());
        for h in handles {
            let chunk_res = h
                .join()
                .map_err(|_| NestCraftError::InvalidInput("mesh decoder worker panicked".to_owned()))?;
            for (path, mesh) in chunk_res? {
                map.insert(path, mesh);
            }
        }
        Ok(map)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{meshes_bit_equal, transform_from_translation_euler_deg};

    fn write_legacy_v1_nestc(job: &BuildJob) -> Vec<u8> {
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
        let parts = job
            .parts
            .iter()
            .map(|p| NestcPart {
                id: p.id.0,
                name: p.name.clone(),
                positions: p.mesh.positions.clone(),
                indices: p.mesh.indices.clone(),
                mesh_ref: None,
                mesh_meta: None,
                default_constraint: to_nestc_constraint(p.default_constraint),
            })
            .collect();
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
            mesh_ref: None,
            mesh_meta: None,
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
        let json = serde_json::to_vec(&project).expect("serialize v1");
        let mut out = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut out);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file(PROJECT_ENTRY, opts).expect("start entry");
            zip.write_all(&json).expect("write json");
            zip.finish().expect("finish zip");
        }
        out.into_inner()
    }

    fn write_v2_with_nmesh_flags(job: &BuildJob, flags: u16) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut blobs = BTreeMap::new();
        let mut parts = Vec::new();
        for p in &job.parts {
            let blob = encode_nmesh_with_flags(&p.mesh, flags).expect("encode nmesh");
            let mesh_ref = format!("meshes/{}.nmesh", sha256_hex(&blob));
            blobs.entry(mesh_ref.clone()).or_insert(blob);
            parts.push(NestcPart {
                id: p.id.0,
                name: p.name.clone(),
                positions: Vec::new(),
                indices: Vec::new(),
                mesh_ref: Some(mesh_ref),
                mesh_meta: Some(NestcMeshMeta::from_mesh(&p.mesh).expect("meta")),
                default_constraint: to_nestc_constraint(p.default_constraint),
            });
        }
        let project = NestcProject {
            version: NESTC_FORMAT_VERSION,
            machine: NestcMachine::FormigaP110,
            clearance_mm: job.clearance_mm,
            wall_margin_mm: job.wall_margin_mm,
            min_z_mm: job.min_z_mm,
            bed_origin: NestcBedOrigin::FrontLeft,
            material_id: job.material_id.clone(),
            parts,
            groups: Vec::new(),
            instances: job
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
                .collect(),
            no_build_zone: None,
        };
        let json = serde_json::to_vec(&project).expect("serialize");
        let mut zip = ZipWriter::new(&mut out);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file(PROJECT_ENTRY, opts).expect("start project");
        zip.write_all(&json).expect("write project");
        for (name, blob) in blobs {
            zip.start_file(&name, opts).expect("start nmesh");
            zip.write_all(&blob).expect("write nmesh");
        }
        zip.finish().expect("finish zip");
        out.into_inner()
    }

    fn uv_sphere_mesh(radius: f32, rings: usize, sectors: usize, offset: [f32; 3]) -> Arc<TriMesh> {
        let mut positions = Vec::with_capacity((rings + 1) * sectors);
        let mut indices = Vec::with_capacity(rings * sectors * 2);
        for r in 0..=rings {
            let phi = std::f32::consts::PI * (r as f32) / (rings as f32);
            let (sp, cp) = phi.sin_cos();
            for s in 0..sectors {
                let theta = 2.0 * std::f32::consts::PI * (s as f32) / (sectors as f32);
                let (st, ct) = theta.sin_cos();
                positions.push([
                    offset[0] + radius * sp * ct,
                    offset[1] + radius * sp * st,
                    offset[2] + radius * cp,
                ]);
            }
        }
        for r in 0..rings {
            for s in 0..sectors {
                let s_next = (s + 1) % sectors;
                let a = (r * sectors + s) as u32;
                let b = (r * sectors + s_next) as u32;
                let c = ((r + 1) * sectors + s) as u32;
                let d = ((r + 1) * sectors + s_next) as u32;
                indices.push([a, c, b]);
                indices.push([b, c, d]);
            }
        }
        Arc::new(TriMesh::from_indexed(positions, indices).expect("sphere mesh"))
    }

    #[test]
    fn roundtrips_build_job_through_nestc() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![[-0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
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
        job.no_build_zone = Some(NoBuildZone::new("Zone A", mesh.clone()));

        let bytes = write_nestc(&job).expect("write nestc");

        // Verify v2 ZIP layout: project.json has version == 2, mesh_meta, no inline geometry, and meshes/*.nmesh entry.
        {
            let mut archive = ZipArchive::new(Cursor::new(&bytes)).expect("zip");
            let mut entry = archive.by_name(PROJECT_ENTRY).expect("project.json");
            let mut json_str = String::new();
            entry.read_to_string(&mut json_str).expect("read project.json");
            drop(entry);
            let parsed: NestcProject = serde_json::from_str(&json_str).expect("parse json");
            assert_eq!(parsed.version, 2);
            assert!(parsed.parts[0].positions.is_empty());
            assert!(parsed.parts[0].indices.is_empty());
            assert!(parsed.parts[0].mesh_meta.is_some());
            assert!(parsed.parts[0].mesh_ref.as_deref().is_some_and(|r| r.starts_with("meshes/")));
            let nmesh_count = (0..archive.len())
                .filter(|&idx| {
                    archive
                        .name_for_index(idx)
                        .is_some_and(|r| r.is_ok_and(|n| n.starts_with("meshes/")))
                })
                .count();
            assert_eq!(nmesh_count, 1, "shared tet+NBZ mesh should be stored once");
        }

        let loaded = read_nestc(&bytes).expect("read nestc");
        assert_eq!(loaded.bed_origin, BedOrigin::Center);
        assert!((loaded.min_z_mm - 3.0).abs() < 1e-4);
        assert!((loaded.wall_margin_mm - 8.0).abs() < 1e-4);
        assert_eq!(loaded.parts.len(), 1);
        assert_eq!(loaded.groups.len(), 1);
        assert_eq!(loaded.instances.len(), 1);
        assert_eq!(loaded.instances[0].group_id, Some(gid));
        assert!(loaded.no_build_zone.is_some());
        assert!(meshes_bit_equal(&loaded.parts[0].mesh, &mesh));
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

    #[test]
    fn reads_legacy_v1_inline_json_nestc() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![[0.0, 0.0, 0.0], [12.0, 0.0, 0.0], [0.0, 12.0, 0.0], [0.0, 0.0, 12.0]],
                vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
            )
            .expect("valid tet"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("legacy_tet", mesh.clone());
        let _ = job.add_instance_centered_on_floor(pid).expect("placed");
        job.no_build_zone = Some(NoBuildZone::new("Legacy Zone", mesh.clone()));

        let v1_bytes = write_legacy_v1_nestc(&job);
        let loaded = read_nestc(&v1_bytes).expect("read legacy v1 nestc");
        assert_eq!(loaded.parts.len(), 1);
        assert_eq!(loaded.parts[0].name, "legacy_tet");
        assert!(meshes_bit_equal(&loaded.parts[0].mesh, &mesh));
        assert!(loaded.no_build_zone.is_some());
    }

    #[test]
    fn deduplicates_identical_meshes_and_prunes_unused_parts_in_nestc() {
        let mesh_a = uv_sphere_mesh(10.0, 8, 12, [0.0, 0.0, 0.0]);
        let mesh_b = Arc::new(
            TriMesh::from_indexed(mesh_a.positions.clone(), mesh_a.indices.clone()).expect("copy"),
        );
        let orphan = uv_sphere_mesh(5.0, 6, 8, [1.0, 2.0, 3.0]);

        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let p1 = job.add_part("Part_Alpha", mesh_a);
        let p2 = job.add_part("Part_Beta", mesh_b);
        let _orphan_pid = job.add_part("Orphan_Unused", orphan);
        let _ = job.add_instance_centered_on_floor(p1).expect("i1");
        let _ = job.add_instance_on_floor_next(p2).expect("i2");

        let bytes = write_nestc(&job).expect("write v2");
        let archive = ZipArchive::new(Cursor::new(&bytes)).expect("zip");
        let nmesh_count = (0..archive.len())
            .filter(|&idx| {
                archive
                    .name_for_index(idx)
                    .is_some_and(|r| r.is_ok_and(|n| n.starts_with("meshes/")))
            })
            .count();
        assert_eq!(nmesh_count, 1, "identical geometries must share one .nmesh entry");

        let loaded = read_nestc(&bytes).expect("read v2");
        assert_eq!(loaded.parts.len(), 2, "unreferenced orphan prototype should be pruned");
        assert_eq!(loaded.parts[0].name, "Part_Alpha");
        assert_eq!(loaded.parts[1].name, "Part_Beta");
        assert!(Arc::ptr_eq(&loaded.parts[0].mesh, &loaded.parts[1].mesh));
    }

    #[test]
    fn hostile_nmesh_payloads_never_panic() {
        assert!(decode_nmesh(b"").is_err());
        assert!(decode_nmesh(b"BAD1\x01\x00\x00\x00\x01\x00\x00\x00\x01\x00\x00\x00").is_err());
        // Bad version
        assert!(decode_nmesh(b"NME1\x02\x00\x00\x00\x01\x00\x00\x00\x01\x00\x00\x00").is_err());
        // Unknown flags
        assert!(decode_nmesh(b"NME1\x01\x00\x08\x00\x01\x00\x00\x00\x01\x00\x00\x00").is_err());
        // Zero counts
        assert!(decode_nmesh(b"NME1\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00").is_err());
        // Huge u32::MAX counts (OOM guard)
        let mut huge = Vec::new();
        huge.extend_from_slice(&NMESH_MAGIC);
        huge.extend_from_slice(&1_u16.to_le_bytes());
        huge.extend_from_slice(&NMESH_DEFAULT_FLAGS.to_le_bytes());
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_nmesh(&huge).is_err());

        // Valid mesh corrupted with out-of-range index, truncated varint, overlong varint, NaN vertex
        let valid_mesh = TriMesh::from_indexed(
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            vec![[0, 1, 2]],
        )
        .expect("valid");
        let valid = encode_nmesh(&valid_mesh).expect("encode");
        // Truncate payload
        assert!(decode_nmesh(&valid[..valid.len() - 1]).is_err());
        // Trailing garbage
        let mut trailing = valid.clone();
        trailing.push(0x00);
        assert!(decode_nmesh(&trailing).is_err());
        // Overlong varint (11 continuation bytes)
        let mut overlong = valid[..NMESH_HEADER_BYTES + 36].to_vec();
        overlong.extend(std::iter::repeat_n(0x80_u8, 11));
        assert!(decode_nmesh(&overlong).is_err());

        // Raw flags with NaN coordinate
        let mut raw = encode_nmesh_with_flags(&valid_mesh, 0).expect("raw encode");
        raw[NMESH_HEADER_BYTES..NMESH_HEADER_BYTES + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(decode_nmesh(&raw).is_err());
        // Raw flags with out-of-range index
        let mut bad_idx = encode_nmesh_with_flags(&valid_mesh, 0).expect("raw encode");
        let idx_off = NMESH_HEADER_BYTES + 36;
        bad_idx[idx_off..idx_off + 4].copy_from_slice(&99_u32.to_le_bytes());
        assert!(decode_nmesh(&bad_idx).is_err());
    }

    #[test]
    fn nestc_v2_vs_v1_size_and_speed_benchmark() {
        let mut job = BuildJob::new(MachineProfile::FarsoonHT403P);
        // 4 distinct tessellated meshes (~72,000 triangles total), 20 instances total.
        let meshes = [
            uv_sphere_mesh(24.0, 90, 100, [0.0, 0.0, 25.0]),
            uv_sphere_mesh(18.5, 90, 100, [1.25, -0.5, 20.0]),
            uv_sphere_mesh(31.0, 90, 100, [-2.0, 3.5, 32.0]),
            uv_sphere_mesh(14.0, 90, 100, [0.0, 0.0, 15.0]),
        ];
        for (idx, mesh) in meshes.iter().enumerate() {
            let pid = job.add_part(format!("SpherePart_{idx}"), Arc::clone(mesh));
            for copy in 0..5 {
                let tf = transform_from_translation_euler_deg(
                    [(idx as f32) * 70.0 - 100.0, (copy as f32) * 65.0 - 130.0, 0.0],
                    [0.0, 0.0, (copy as f32) * 15.0],
                );
                let _ = job.add_instance(pid, tf).expect("placed");
            }
        }

        let t0 = std::time::Instant::now();
        let v1_bytes = write_legacy_v1_nestc(&job);
        let v1_write_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = std::time::Instant::now();
        let v1_loaded = read_nestc(&v1_bytes).expect("read v1");
        let v1_read_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let t2 = std::time::Instant::now();
        let v2_raw_bytes = write_v2_with_nmesh_flags(&job, 0);
        let v2_raw_write_ms = t2.elapsed().as_secs_f64() * 1000.0;

        let t3 = std::time::Instant::now();
        let v2_raw_loaded = read_nestc(&v2_raw_bytes).expect("read v2 raw");
        let v2_raw_read_ms = t3.elapsed().as_secs_f64() * 1000.0;

        let t4 = std::time::Instant::now();
        let v2_bytes = write_nestc(&job).expect("write v2");
        let v2_write_ms = t4.elapsed().as_secs_f64() * 1000.0;

        let t5 = std::time::Instant::now();
        let v2_loaded = read_nestc(&v2_bytes).expect("read v2");
        let v2_read_ms = t5.elapsed().as_secs_f64() * 1000.0;

        println!(
            "[nestc benchmark: {} parts, {} instances, {} tris]\n\
             v1 zipped JSON        : {:>9} bytes | write {:>7.2} ms | read {:>7.2} ms\n\
             v2 raw binary .nmesh  : {:>9} bytes ({:.1}% of v1) | write {:>7.2} ms | read {:>7.2} ms\n\
             v2 shuffled+varint    : {:>9} bytes ({:.1}% of v1) | write {:>7.2} ms | read {:>7.2} ms",
            job.parts.len(),
            job.instances.len(),
            job.parts.iter().map(|p| p.mesh.indices.len()).sum::<usize>(),
            v1_bytes.len(),
            v1_write_ms,
            v1_read_ms,
            v2_raw_bytes.len(),
            (v2_raw_bytes.len() as f64 * 100.0) / (v1_bytes.len() as f64),
            v2_raw_write_ms,
            v2_raw_read_ms,
            v2_bytes.len(),
            (v2_bytes.len() as f64 * 100.0) / (v1_bytes.len() as f64),
            v2_write_ms,
            v2_read_ms,
        );

        assert!(
            v2_bytes.len() * 100 < v1_bytes.len() * 55,
            "expected v2 ({}) to be < 55% of v1 ({})",
            v2_bytes.len(),
            v1_bytes.len()
        );
        for (orig, (a, (b, c))) in job.parts.iter().zip(
            v1_loaded
                .parts
                .iter()
                .zip(v2_raw_loaded.parts.iter().zip(v2_loaded.parts.iter())),
        ) {
            assert!(meshes_bit_equal(&orig.mesh, &a.mesh));
            assert!(meshes_bit_equal(&orig.mesh, &b.mesh));
            assert!(meshes_bit_equal(&orig.mesh, &c.mesh));
        }
    }
}
