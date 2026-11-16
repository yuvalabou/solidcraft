//! Binary and ASCII STL mesh parser.

use std::collections::HashMap;

use crate::model::TriMesh;
use crate::{Result, SlsError};

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
        return Err(SlsError::InvalidInput("binary STL shorter than 84-byte header".to_owned()));
    }
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    let expected = count
        .checked_mul(50)
        .and_then(|n| n.checked_add(84))
        .ok_or_else(|| SlsError::InvalidInput("binary STL triangle count overflow".to_owned()))?;
    if bytes.len() < expected {
        return Err(SlsError::InvalidInput("truncated binary STL payload".to_owned()));
    }
    if count == 0 {
        return Err(SlsError::InvalidInput("binary STL contains 0 triangles".to_owned()));
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
        return Err(SlsError::InvalidInput("truncated STL vertex".to_owned()));
    }
    let x = f32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
    let y = f32::from_le_bytes([slice[4], slice[5], slice[6], slice[7]]);
    let z = f32::from_le_bytes([slice[8], slice[9], slice[10], slice[11]]);
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return Err(SlsError::InvalidInput("STL contains non-finite vertex".to_owned()));
    }
    Ok([x, y, z])
}

fn parse_ascii_stl(bytes: &[u8]) -> Result<TriMesh> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| SlsError::InvalidInput("ASCII STL is not valid UTF-8".to_owned()))?;
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
        return Err(SlsError::InvalidInput("ASCII STL has incomplete triangle".to_owned()));
    }

    TriMesh::from_indexed(positions, indices)
}

fn parse_f32(tok: Option<&str>) -> Result<f32> {
    let s = tok.ok_or_else(|| SlsError::InvalidInput("missing ASCII STL coordinate".to_owned()))?;
    let v: f32 = s
        .parse()
        .map_err(|_| SlsError::InvalidInput(format!("invalid ASCII STL float '{s}'")))?;
    if !v.is_finite() {
        return Err(SlsError::InvalidInput("ASCII STL contains non-finite coordinate".to_owned()));
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
        .map_err(|_| SlsError::InvalidInput("STL vertex count exceeds u32::MAX".to_owned()))?;
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
}
