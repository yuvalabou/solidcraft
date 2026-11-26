//! Pure-mesh 3MF reader and writer (including Production Extension `p:path` build items).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use crate::model::{BuildJob, IDENTITY_TRANSFORM, MachineProfile, Transform3x4, TriMesh};
use crate::{NestCraftError, Result};

/// One object + placement extracted from a 3MF archive.
#[derive(Debug, Clone)]
pub struct ThreeMfLoadedItem {
    /// Part name.
    pub name: String,
    /// Triangle mesh in part-local coordinates.
    pub mesh: Arc<TriMesh>,
    /// Rigid/affine placement transform from the `<build><item>` (or identity).
    pub transform: Transform3x4,
}

/// Read all mesh build items from a `.3mf` archive.
pub fn read_threemf(bytes: &[u8]) -> Result<Vec<ThreeMfLoadedItem>> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| NestCraftError::InvalidInput(format!("invalid 3MF zip: {e}")))?;

    let root_path = find_root_model_path(&mut archive)?;
    let mut model_cache: HashMap<String, ParsedModel> = HashMap::new();
    let root_xml = read_zip_text(&mut archive, &root_path)?;
    let root_model = parse_model_xml(&root_xml)?;
    model_cache.insert(normalize_part_path(&root_path), root_model.clone());

    let mut items = Vec::new();
    for build_item in &root_model.build_items {
        let model_key = build_item
            .path
            .as_deref()
            .map(normalize_part_path)
            .unwrap_or_else(|| normalize_part_path(&root_path));
        if !model_cache.contains_key(&model_key) {
            let xml = read_zip_text(&mut archive, &model_key)?;
            let parsed = parse_model_xml(&xml)?;
            model_cache.insert(model_key.clone(), parsed);
        }
        if let Some(model) = model_cache.get(&model_key).cloned() {
            collect_object_meshes(
                &model,
                &model_key,
                build_item.object_id,
                build_item.transform,
                &mut archive,
                &mut model_cache,
                &mut items,
            )?;
        }
    }

    if items.is_empty() {
        for obj in root_model.objects.values() {
            if let Some(mesh) = &obj.mesh {
                items.push(ThreeMfLoadedItem {
                    name: obj.name.clone(),
                    mesh: mesh.clone(),
                    transform: IDENTITY_TRANSFORM,
                });
            }
        }
    }

    if items.is_empty() {
        return Err(NestCraftError::InvalidInput("3MF contains no mesh objects".to_owned()));
    }
    Ok(items)
}

/// Write a [`BuildJob`] to a `.3mf` archive with `<build><item>` transforms.
pub fn write_threemf(job: &BuildJob) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default();

        zip.start_file("[Content_Types].xml", opts)
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip write failed: {e}")))?;
        zip.write_all(CONTENT_TYPES_XML.as_bytes())
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip write failed: {e}")))?;

        zip.start_file("_rels/.rels", opts)
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip write failed: {e}")))?;
        zip.write_all(ROOT_RELS_XML.as_bytes())
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip write failed: {e}")))?;

        let model_xml = build_model_xml(job);
        zip.start_file("3D/3dmodel.model", opts)
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip write failed: {e}")))?;
        zip.write_all(model_xml.as_bytes())
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip write failed: {e}")))?;

        zip.finish()
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip finish failed: {e}")))?;
    }
    Ok(buf.into_inner())
}

/// Convenience helper: load a 3MF buffer directly into a [`BuildJob`].
pub fn read_threemf_into_job(bytes: &[u8], machine: MachineProfile) -> Result<BuildJob> {
    let loaded = read_threemf(bytes)?;
    let mut job = BuildJob::new(machine);
    for item in loaded {
        let (pid, _) = job.add_or_reuse_part(item.name, item.mesh);
        let _ = job.add_instance(pid, item.transform)?;
    }
    Ok(job)
}

#[derive(Clone)]
struct ParsedModel {
    objects: HashMap<u32, ParsedObject>,
    build_items: Vec<BuildItem>,
}

#[derive(Clone)]
struct ParsedObject {
    name: String,
    mesh: Option<Arc<TriMesh>>,
    components: Vec<ComponentRef>,
}

#[derive(Clone)]
struct ComponentRef {
    object_id: u32,
    path: Option<String>,
    transform: Transform3x4,
}

#[derive(Clone)]
struct BuildItem {
    object_id: u32,
    path: Option<String>,
    transform: Transform3x4,
}

fn collect_object_meshes(
    model: &ParsedModel,
    current_path: &str,
    object_id: u32,
    parent_xform: Transform3x4,
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    cache: &mut HashMap<String, ParsedModel>,
    out: &mut Vec<ThreeMfLoadedItem>,
) -> Result<()> {
    let Some(obj) = model.objects.get(&object_id) else {
        return Ok(());
    };
    if let Some(mesh) = &obj.mesh {
        out.push(ThreeMfLoadedItem {
            name: obj.name.clone(),
            mesh: mesh.clone(),
            transform: parent_xform,
        });
    }
    for comp in &obj.components {
        let target_path = comp
            .path
            .as_deref()
            .map(normalize_part_path)
            .unwrap_or_else(|| current_path.to_owned());
        if !cache.contains_key(&target_path) {
            let xml = read_zip_text(archive, &target_path)?;
            let parsed = parse_model_xml(&xml)?;
            cache.insert(target_path.clone(), parsed);
        }
        let child_xform = mul_3x4(&parent_xform, &comp.transform);
        if let Some(target_model) = cache.get(&target_path).cloned() {
            collect_object_meshes(
                &target_model,
                &target_path,
                comp.object_id,
                child_xform,
                archive,
                cache,
                out,
            )?;
        }
    }
    Ok(())
}

fn find_root_model_path(archive: &mut ZipArchive<Cursor<&[u8]>>) -> Result<String> {
    if archive.by_name("3D/3dmodel.model").is_ok() {
        return Ok("3D/3dmodel.model".to_owned());
    }
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip index error: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| NestCraftError::InvalidInput(format!("3MF zip entry name error: {e}")))?
            .into_owned();
        if name.ends_with(".model") {
            return Ok(name);
        }
    }
    Err(NestCraftError::InvalidInput("3MF archive contains no .model part".to_owned()))
}

fn normalize_part_path(path: &str) -> String {
    path.trim_start_matches('/').to_owned()
}

fn read_zip_text(archive: &mut ZipArchive<Cursor<&[u8]>>, path: &str) -> Result<String> {
    let norm = normalize_part_path(path);
    let mut file = archive
        .by_name(&norm)
        .map_err(|e| NestCraftError::InvalidInput(format!("missing 3MF part '{norm}': {e}")))?;
    let mut s = String::new();
    file.read_to_string(&mut s)
        .map_err(|e| NestCraftError::InvalidInput(format!("unreadable 3MF part '{norm}': {e}")))?;
    Ok(s)
}

fn parse_model_xml(xml: &str) -> Result<ParsedModel> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut objects: HashMap<u32, ParsedObject> = HashMap::new();
    let mut build_items = Vec::new();

    let mut cur_obj_id: Option<u32> = None;
    let mut cur_obj_name = String::new();
    let mut cur_positions: Vec<[f32; 3]> = Vec::new();
    let mut cur_indices: Vec<[u32; 3]> = Vec::new();
    let mut cur_components: Vec<ComponentRef> = Vec::new();
    let mut in_mesh = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"object" => {
                        let mut id: Option<u32> = None;
                        let mut name = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.local_name().as_ref() {
                                b"id" => id = std::str::from_utf8(&attr.value).ok().and_then(|s| s.parse().ok()),
                                b"name" => name = std::str::from_utf8(&attr.value).ok().map(str::to_owned),
                                _ => {}
                            }
                        }
                        if let Some(oid) = id {
                            cur_obj_id = Some(oid);
                            cur_obj_name = name.unwrap_or_else(|| format!("part_{oid}"));
                            cur_positions.clear();
                            cur_indices.clear();
                            cur_components.clear();
                        }
                    }
                    b"mesh" => {
                        in_mesh = true;
                    }
                    b"vertex" if in_mesh => {
                        let mut x = 0.0_f32;
                        let mut y = 0.0_f32;
                        let mut z = 0.0_f32;
                        for attr in e.attributes().flatten() {
                            let val = std::str::from_utf8(&attr.value).ok().and_then(|s| s.parse::<f32>().ok());
                            match attr.key.local_name().as_ref() {
                                b"x" => if let Some(v) = val { x = v; },
                                b"y" => if let Some(v) = val { y = v; },
                                b"z" => if let Some(v) = val { z = v; },
                                _ => {}
                            }
                        }
                        cur_positions.push([x, y, z]);
                    }
                    b"triangle" if in_mesh => {
                        let mut v1 = 0_u32;
                        let mut v2 = 0_u32;
                        let mut v3 = 0_u32;
                        for attr in e.attributes().flatten() {
                            let val = std::str::from_utf8(&attr.value).ok().and_then(|s| s.parse::<u32>().ok());
                            match attr.key.local_name().as_ref() {
                                b"v1" => if let Some(v) = val { v1 = v; },
                                b"v2" => if let Some(v) = val { v2 = v; },
                                b"v3" => if let Some(v) = val { v3 = v; },
                                _ => {}
                            }
                        }
                        cur_indices.push([v1, v2, v3]);
                    }
                    b"component" if cur_obj_id.is_some() => {
                        let mut oid: Option<u32> = None;
                        let mut path: Option<String> = None;
                        let mut transform = IDENTITY_TRANSFORM;
                        for attr in e.attributes().flatten() {
                            match attr.key.local_name().as_ref() {
                                b"objectid" => oid = std::str::from_utf8(&attr.value).ok().and_then(|s| s.parse().ok()),
                                b"path" => path = std::str::from_utf8(&attr.value).ok().map(str::to_owned),
                                b"transform" => {
                                    if let Ok(s) = std::str::from_utf8(&attr.value)
                                        && let Ok(t) = parse_threemf_transform(s)
                                    {
                                        transform = t;
                                    }
                                }
                                _ => {}
                            }
                        }
                        if let Some(object_id) = oid {
                            cur_components.push(ComponentRef { object_id, path, transform });
                        }
                    }
                    b"item" => {
                        let mut oid: Option<u32> = None;
                        let mut path: Option<String> = None;
                        let mut transform = IDENTITY_TRANSFORM;
                        for attr in e.attributes().flatten() {
                            match attr.key.local_name().as_ref() {
                                b"objectid" => oid = std::str::from_utf8(&attr.value).ok().and_then(|s| s.parse().ok()),
                                b"path" => path = std::str::from_utf8(&attr.value).ok().map(str::to_owned),
                                b"transform" => {
                                    if let Ok(s) = std::str::from_utf8(&attr.value)
                                        && let Ok(t) = parse_threemf_transform(s)
                                    {
                                        transform = t;
                                    }
                                }
                                _ => {}
                            }
                        }
                        if let Some(object_id) = oid {
                            build_items.push(BuildItem { object_id, path, transform });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"mesh" => in_mesh = false,
                b"object" => {
                    if let Some(oid) = cur_obj_id.take() {
                        let mesh = if !cur_indices.is_empty() {
                            Some(Arc::new(TriMesh::from_indexed(
                                std::mem::take(&mut cur_positions),
                                std::mem::take(&mut cur_indices),
                            )?))
                        } else {
                            None
                        };
                        objects.insert(
                            oid,
                            ParsedObject {
                                name: std::mem::take(&mut cur_obj_name),
                                mesh,
                                components: std::mem::take(&mut cur_components),
                            },
                        );
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(NestCraftError::InvalidInput(format!("3MF XML parse error: {e}"))),
            _ => {}
        }
    }

    Ok(ParsedModel { objects, build_items })
}

/// 3MF `transform` attribute has 12 floats in column-major 4x3 order:
/// `m00 m10 m20 m01 m11 m21 m02 m12 m22 m03 m13 m23`.
fn parse_threemf_transform(s: &str) -> Result<Transform3x4> {
    let mut vals = [0.0_f32; 12];
    let mut count = 0_usize;
    for tok in s.split_whitespace() {
        if count >= 12 {
            return Err(NestCraftError::InvalidInput("3MF transform has >12 values".to_owned()));
        }
        let v: f32 = tok
            .parse()
            .map_err(|_| NestCraftError::InvalidInput(format!("invalid 3MF transform float '{tok}'")))?;
        if !v.is_finite() {
            return Err(NestCraftError::InvalidInput("3MF transform contains non-finite value".to_owned()));
        }
        vals[count] = v;
        count += 1;
    }
    if count != 12 {
        return Err(NestCraftError::InvalidInput("3MF transform requires 12 values".to_owned()));
    }
    Ok([
        [vals[0], vals[3], vals[6], vals[9]],
        [vals[1], vals[4], vals[7], vals[10]],
        [vals[2], vals[5], vals[8], vals[11]],
    ])
}

fn format_threemf_transform(m: &Transform3x4) -> String {
    format!(
        "{} {} {} {} {} {} {} {} {} {} {} {}",
        m[0][0], m[1][0], m[2][0],
        m[0][1], m[1][1], m[2][1],
        m[0][2], m[1][2], m[2][2],
        m[0][3], m[1][3], m[2][3],
    )
}

fn mul_3x4(a: &Transform3x4, b: &Transform3x4) -> Transform3x4 {
    let mut out = [[0.0_f32; 4]; 3];
    for r in 0..3 {
        for c in 0..3 {
            out[r][c] = a[r][0] * b[0][c] + a[r][1] * b[1][c] + a[r][2] * b[2][c];
        }
        out[r][3] = a[r][0] * b[0][3] + a[r][1] * b[1][3] + a[r][2] * b[2][3] + a[r][3];
    }
    out
}

fn build_model_xml(job: &BuildJob) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <model unit=\"millimeter\" xml:lang=\"en-US\" xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\">\n\
         <resources>\n",
    );
    for part in &job.parts {
        let _ = writeln!(xml, "  <object id=\"{}\" name=\"{}\" type=\"model\">", part.id.0, escape_xml(&part.name));
        xml.push_str("    <mesh>\n      <vertices>\n");
        for v in &part.mesh.positions {
            let _ = writeln!(xml, "        <vertex x=\"{}\" y=\"{}\" z=\"{}\"/>", v[0], v[1], v[2]);
        }
        xml.push_str("      </vertices>\n      <triangles>\n");
        for t in &part.mesh.indices {
            let _ = writeln!(xml, "        <triangle v1=\"{}\" v2=\"{}\" v3=\"{}\"/>", t[0], t[1], t[2]);
        }
        xml.push_str("      </triangles>\n    </mesh>\n  </object>\n");
    }
    xml.push_str("</resources>\n<build>\n");
    for inst in &job.instances {
        let xform = format_threemf_transform(&inst.transform);
        let _ = writeln!(xml, "  <item objectid=\"{}\" transform=\"{}\"/>", inst.part_id.0, xform);
    }
    xml.push_str("</build>\n</model>\n");
    xml
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const CONTENT_TYPES_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\n\
  <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\n\
  <Default Extension=\"model\" ContentType=\"application/vnd.ms-package.3dmanufacturing-3dmodel+xml\"/>\n\
</Types>\n";

const ROOT_RELS_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n\
  <Relationship Target=\"/3D/3dmodel.model\" Id=\"rel0\" Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\"/>\n\
</Relationships>\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transform_from_translation_euler_deg;

    #[test]
    fn roundtrips_job_through_threemf() {
        let mesh = Arc::new(
            TriMesh::from_indexed(
                vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
                vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
            )
            .expect("valid tetra"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("tetra", mesh);
        let xf = transform_from_translation_euler_deg([25.0, 30.0, 0.0], [0.0, 0.0, 90.0]);
        job.add_instance(pid, xf).expect("instance added");

        let bytes = write_threemf(&job).expect("3mf written");
        let reloaded = read_threemf_into_job(&bytes, MachineProfile::FormigaP110).expect("3mf read");
        assert_eq!(reloaded.parts.len(), 1);
        assert_eq!(reloaded.instances.len(), 1);
        let t = reloaded.instances[0].translation();
        assert!((t[0] - 25.0).abs() < 1e-4);
        assert!((t[1] - 30.0).abs() < 1e-4);
    }

    #[test]
    fn rejects_invalid_3mf_zip() {
        assert!(read_threemf(b"not a zip").is_err());
    }
}
