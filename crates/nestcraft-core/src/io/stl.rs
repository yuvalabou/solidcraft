//! Binary and ASCII STL mesh parser.

use std::collections::HashMap;

use crate::model::{Transform3x4, TriMesh, transform_point};
use crate::{NestCraftError, Result};

/// Encode `mesh` (optionally transformed by `transform`) into a binary STL buffer.
pub fn write_binary_stl(mesh: &TriMesh, transform: Option<&Transform3x4>) -> Result<Vec<u8>> {
    let tri_count = u32::try_from(mesh.indices.len())
        .map_err(|_| NestCraftError::InvalidInput("triangle count exceeds binary STL u32 limit".to_owned()))?;
    let mut buf = Vec::with_capacity(84 + mesh.indices.len() * 50);
    let mut header = [0_u8; 80];
    let tag = b"NestCraft binary STL";
    header[..tag.len()].copy_from_slice(tag);
    buf.extend_from_slice(&header);
    buf.extend_from_slice(&tri_count.to_le_bytes());

    for (tri_idx, tri) in mesh.indices.iter().enumerate() {
        let raw_p = [
            mesh.positions.get(tri[0] as usize).copied().unwrap_or([0.0; 3]),
            mesh.positions.get(tri[1] as usize).copied().unwrap_or([0.0; 3]),
            mesh.positions.get(tri[2] as usize).copied().unwrap_or([0.0; 3]),
        ];
        let p = match transform {
            Some(tf) => [
                transform_point(tf, raw_p[0]),
                transform_point(tf, raw_p[1]),
                transform_point(tf, raw_p[2]),
            ],
            None => raw_p,
        };
        let n = compute_unit_normal(p[0], p[1], p[2])
            .unwrap_or_else(|| mesh.tri_normals.get(tri_idx).copied().unwrap_or([0.0, 0.0, 1.0]));
        for f in n.into_iter().chain(p[0]).chain(p[1]).chain(p[2]) {
            buf.extend_from_slice(&f.to_le_bytes());
        }
        buf.extend_from_slice(&0_u16.to_le_bytes());
    }
    Ok(buf)
}

fn compute_unit_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> Option<[f32; 3]> {
    let ux = b[0] - a[0];
    let uy = b[1] - a[1];
    let uz = b[2] - a[2];
    let vx = c[0] - a[0];
    let vy = c[1] - a[1];
    let vz = c[2] - a[2];
    let nx = uy * vz - uz * vy;
    let ny = uz * vx - ux * vz;
    let nz = ux * vy - uy * vx;
    let len = (nx * nx + ny * ny + nz * nz).sqrt();
    if len > 1e-12 && len.is_finite() {
        Some([nx / len, ny / len, nz / len])
    } else {
        None
    }
}

/// Parse a binary or ASCII STL buffer into a [`TriMesh`].
pub fn parse_stl(bytes: &[u8]) -> Result<TriMesh> {
    if is_binary_stl(bytes) {
        parse_binary_stl(bytes)
    } else {
        parse_ascii_stl(bytes)
    }
}

fn is_binary_stl(bytes: &[u8]) -> bool {
    if bytes.len() < 84 {
        return false;
    }
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    let expected_len = count.checked_mul(50).and_then(|n| n.checked_add(84));
    expected_len == Some(bytes.len())
}

fn parse_binary_stl(bytes: &[u8]) -> Result<TriMesh> {
    if bytes.len() < 84 {
        return Err(NestCraftError::InvalidInput("binary STL shorter than 84-byte header".to_owned()));
    }
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    let expected = count
        .checked_mul(50)
        .and_then(|n| n.checked_add(84))
        .ok_or_else(|| NestCraftError::InvalidInput("binary STL triangle count overflow".to_owned()))?;
    if bytes.len() < expected {
        return Err(NestCraftError::InvalidInput("truncated binary STL payload".to_owned()));
    }
    if count == 0 {
        return Err(NestCraftError::InvalidInput("binary STL contains 0 triangles".to_owned()));
    }

    let mut positions = Vec::new();
    let mut indices = Vec::with_capacity(count);
    let mut intern: HashMap<[u32; 3], u32> = HashMap::new();

    for t in 0..count {
        let base = 84 + t * 50;
        // Skip 12-byte facet normal (we compute exact normals from vertices).
        let v0 = read_vec3(&bytes[base + 12..base + 24])?;
        let v1 = read_vec3(&bytes[base + 24..base + 36])?;
        let v2 = read_vec3(&bytes[base + 36..base + 48])?;
        let i0 = intern_vertex(&mut positions, &mut intern, v0)?;
        let i1 = intern_vertex(&mut positions, &mut intern, v1)?;
        let i2 = intern_vertex(&mut positions, &mut intern, v2)?;
        indices.push([i0, i1, i2]);
    }

    TriMesh::from_indexed(positions, indices)
}

fn read_vec3(slice: &[u8]) -> Result<[f32; 3]> {
    if slice.len() < 12 {
        return Err(NestCraftError::InvalidInput("truncated STL vertex".to_owned()));
    }
    let x = f32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
    let y = f32::from_le_bytes([slice[4], slice[5], slice[6], slice[7]]);
    let z = f32::from_le_bytes([slice[8], slice[9], slice[10], slice[11]]);
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return Err(NestCraftError::InvalidInput("STL contains non-finite vertex".to_owned()));
    }
    Ok([x, y, z])
}

fn parse_ascii_stl(bytes: &[u8]) -> Result<TriMesh> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| NestCraftError::InvalidInput("ASCII STL is not valid UTF-8".to_owned()))?;
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    let mut intern: HashMap<[u32; 3], u32> = HashMap::new();
    let mut current_tri = [0_u32; 3];
    let mut vert_in_tri = 0_usize;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("vertex") {
            let mut nums = rest.split_whitespace();
            let x = parse_f32(nums.next())?;
            let y = parse_f32(nums.next())?;
            let z = parse_f32(nums.next())?;
            let idx = intern_vertex(&mut positions, &mut intern, [x, y, z])?;
            if vert_in_tri < 3 {
                current_tri[vert_in_tri] = idx;
                vert_in_tri += 1;
            }
            if vert_in_tri == 3 {
                indices.push(current_tri);
                vert_in_tri = 0;
            }
        }
    }

    if vert_in_tri != 0 {
        return Err(NestCraftError::InvalidInput("ASCII STL has incomplete triangle".to_owned()));
    }

    TriMesh::from_indexed(positions, indices)
}

fn parse_f32(tok: Option<&str>) -> Result<f32> {
    let s = tok.ok_or_else(|| NestCraftError::InvalidInput("missing ASCII STL coordinate".to_owned()))?;
    let v: f32 = s
        .parse()
        .map_err(|_| NestCraftError::InvalidInput(format!("invalid ASCII STL float '{s}'")))?;
    if !v.is_finite() {
        return Err(NestCraftError::InvalidInput("ASCII STL contains non-finite coordinate".to_owned()));
    }
    Ok(v)
}

fn intern_vertex(
    positions: &mut Vec<[f32; 3]>,
    intern: &mut HashMap<[u32; 3], u32>,
    v: [f32; 3],
) -> Result<u32> {
    let norm = |f: f32| if f == 0.0 { 0.0_f32.to_bits() } else { f.to_bits() };
    let key = [norm(v[0]), norm(v[1]), norm(v[2])];
    if let Some(&idx) = intern.get(&key) {
        return Ok(idx);
    }
    let idx = u32::try_from(positions.len())
        .map_err(|_| NestCraftError::InvalidInput("STL vertex count exceeds u32::MAX".to_owned()))?;
    positions.push(v);
    intern.insert(key, idx);
    Ok(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ascii_stl_triangle() {
        let stl = b"solid tri\n\
            facet normal 0 0 1\n\
              outer loop\n\
                vertex 0 0 0\n\
                vertex 10 0 0\n\
                vertex 0 10 0\n\
              endloop\n\
            endfacet\n\
            endsolid tri\n";
        let mesh = parse_stl(stl).expect("valid ascii stl");
        assert_eq!(mesh.indices.len(), 1);
        assert_eq!(mesh.positions.len(), 3);
    }

    #[test]
    fn parses_binary_stl_even_with_solid_header() {
        let mut buf = vec![0_u8; 84 + 50];
        buf[..6].copy_from_slice(b"solid ");
        buf[80..84].copy_from_slice(&1_u32.to_le_bytes());
        let p0 = [0.0_f32, 0.0, 0.0];
        let p1 = [5.0_f32, 0.0, 0.0];
        let p2 = [0.0_f32, 5.0, 0.0];
        for (i, p) in [p0, p1, p2].iter().enumerate() {
            let off = 84 + 12 + i * 12;
            buf[off..off + 4].copy_from_slice(&p[0].to_le_bytes());
            buf[off + 4..off + 8].copy_from_slice(&p[1].to_le_bytes());
            buf[off + 8..off + 12].copy_from_slice(&p[2].to_le_bytes());
        }
        let mesh = parse_stl(&buf).expect("binary stl parsed");
        assert_eq!(mesh.indices.len(), 1);
    }

    #[test]
    fn rejects_corrupt_stl_without_panicking() {
        assert!(parse_stl(b"").is_err());
        assert!(parse_stl(&[0_u8; 84]).is_err());
    }

    #[test]
    fn writes_and_roundtrips_transformed_binary_stl() {
        let mesh = TriMesh::from_indexed(
            vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]],
            vec![[0, 1, 2]],
        )
        .expect("valid mesh");
        let tf = [
            [1.0, 0.0, 0.0, 25.0],
            [0.0, 1.0, 0.0, -10.0],
            [0.0, 0.0, 1.0, 5.0],
        ];
        let bytes = write_binary_stl(&mesh, Some(&tf)).expect("written");
        let parsed = parse_stl(&bytes).expect("re-parsed");
        assert_eq!(parsed.indices.len(), 1);
        assert!((parsed.bounds[0][0] - 25.0).abs() < 1e-4);
        assert!((parsed.bounds[0][1] - (-10.0)).abs() < 1e-4);
        assert!((parsed.bounds[0][2] - 5.0).abs() < 1e-4);
    }
}
