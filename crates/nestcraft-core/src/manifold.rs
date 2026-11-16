//! Pure-Rust mesh validity diagnostics, manifold/winding checks, and shell splitting.

use std::collections::HashMap;

use crate::Result;
use crate::model::TriMesh;

/// Minimum triangle area (mm²) below which a triangle is considered degenerate.
const DEGENERATE_AREA_EPS: f64 = 1e-10;

/// Diagnostic summary of a [`TriMesh`]'s topological and geometric validity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifoldReport {
    /// Total number of triangles in the mesh.
    pub triangle_count: usize,
    /// Total number of vertices in the mesh.
    pub vertex_count: usize,
    /// Number of edge-connected triangle shells.
    pub shell_count: usize,
    /// Undirected edges referenced by only 1 triangle (open holes/boundaries).
    pub boundary_edge_count: usize,
    /// Undirected edges referenced by 3 or more triangles.
    pub non_manifold_edge_count: usize,
    /// Undirected edges shared by 2 triangles with inconsistent (same-direction) winding.
    pub flipped_edge_count: usize,
    /// Triangles with zero or near-zero area (or repeated vertex indices).
    pub degenerate_triangle_count: usize,
}

impl ManifoldReport {
    /// Returns `true` when the mesh is a closed, consistently wound, non-degenerate manifold.
    pub fn is_watertight_manifold(&self) -> bool {
        self.triangle_count > 0
            && self.boundary_edge_count == 0
            && self.non_manifold_edge_count == 0
            && self.flipped_edge_count == 0
            && self.degenerate_triangle_count == 0
    }
}

#[derive(Default, Clone, Copy)]
struct EdgeCounts {
    forward: usize,
    reverse: usize,
}

/// Analyze `mesh` for watertightness, winding consistency, non-manifold edges,
/// degenerate triangles, and connected shell count.
pub fn analyze_mesh(mesh: &TriMesh) -> ManifoldReport {
    let triangle_count = mesh.indices.len();
    let vertex_count = mesh.positions.len();

    let mut edges: HashMap<(u32, u32), EdgeCounts> = HashMap::with_capacity(triangle_count * 3 / 2);
    let mut edge_first_tri: HashMap<(u32, u32), usize> = HashMap::with_capacity(triangle_count * 3 / 2);
    let mut uf = UnionFind::new(triangle_count);
    let mut degenerate_triangle_count = 0_usize;

    for (tri_idx, &[i0, i1, i2]) in mesh.indices.iter().enumerate() {
        if i0 == i1 || i1 == i2 || i2 == i0 || tri_area_sq(&mesh.positions, i0, i1, i2) <= DEGENERATE_AREA_EPS * DEGENERATE_AREA_EPS {
            degenerate_triangle_count += 1;
        }

        for (a, b) in [(i0, i1), (i1, i2), (i2, i0)] {
            if a == b {
                continue;
            }
            let (key, is_forward) = if a < b { ((a, b), true) } else { ((b, a), false) };
            let entry = edges.entry(key).or_default();
            if is_forward {
                entry.forward += 1;
            } else {
                entry.reverse += 1;
            }
            if let Some(&other_tri) = edge_first_tri.get(&key) {
                uf.union(tri_idx, other_tri);
            } else {
                edge_first_tri.insert(key, tri_idx);
            }
        }
    }

    let mut boundary_edge_count = 0_usize;
    let mut non_manifold_edge_count = 0_usize;
    let mut flipped_edge_count = 0_usize;

    for counts in edges.values() {
        let total = counts.forward + counts.reverse;
        if total == 1 {
            boundary_edge_count += 1;
        } else if total > 2 {
            non_manifold_edge_count += 1;
        } else if total == 2 && (counts.forward == 2 || counts.reverse == 2) {
            flipped_edge_count += 1;
        }
    }

    let shell_count = uf.count_components();

    ManifoldReport {
        triangle_count,
        vertex_count,
        shell_count,
        boundary_edge_count,
        non_manifold_edge_count,
        flipped_edge_count,
        degenerate_triangle_count,
    }
}

/// Split a multi-shell [`TriMesh`] into separate edge-connected [`TriMesh`] parts.
pub fn split_shells(mesh: &TriMesh) -> Result<Vec<TriMesh>> {
    let n_tris = mesh.indices.len();
    let mut edge_first_tri: HashMap<(u32, u32), usize> = HashMap::with_capacity(n_tris * 3 / 2);
    let mut uf = UnionFind::new(n_tris);

    for (tri_idx, &[i0, i1, i2]) in mesh.indices.iter().enumerate() {
        for (a, b) in [(i0, i1), (i1, i2), (i2, i0)] {
            if a == b {
                continue;
            }
            let key = if a < b { (a, b) } else { (b, a) };
            if let Some(&other_tri) = edge_first_tri.get(&key) {
                uf.union(tri_idx, other_tri);
            } else {
                edge_first_tri.insert(key, tri_idx);
            }
        }
    }

    let mut root_to_group: HashMap<usize, usize> = HashMap::new();
    let mut groups: Vec<Vec<[u32; 3]>> = Vec::new();

    for (tri_idx, &tri) in mesh.indices.iter().enumerate() {
        let root = uf.find(tri_idx);
        let group_idx = match root_to_group.get(&root) {
            Some(&idx) => idx,
            None => {
                let idx = groups.len();
                root_to_group.insert(root, idx);
                groups.push(Vec::new());
                idx
            }
        };
        groups[group_idx].push(tri);
    }

    let mut shells = Vec::with_capacity(groups.len());
    for tris in groups {
        let mut vert_map: HashMap<u32, u32> = HashMap::new();
        let mut positions: Vec<[f32; 3]> = Vec::new();
        let mut indices: Vec<[u32; 3]> = Vec::with_capacity(tris.len());

        for [i0, i1, i2] in tris {
            let m0 = remap_vertex(i0, &mesh.positions, &mut vert_map, &mut positions);
            let m1 = remap_vertex(i1, &mesh.positions, &mut vert_map, &mut positions);
            let m2 = remap_vertex(i2, &mesh.positions, &mut vert_map, &mut positions);
            indices.push([m0, m1, m2]);
        }
        shells.push(TriMesh::from_indexed(positions, indices)?);
    }

    Ok(shells)
}

fn remap_vertex(
    old_idx: u32,
    src_positions: &[[f32; 3]],
    vert_map: &mut HashMap<u32, u32>,
    dst_positions: &mut Vec<[f32; 3]>,
) -> u32 {
    if let Some(&new_idx) = vert_map.get(&old_idx) {
        return new_idx;
    }
    let new_idx = dst_positions.len() as u32;
    let pos = src_positions.get(old_idx as usize).copied().unwrap_or([0.0, 0.0, 0.0]);
    dst_positions.push(pos);
    vert_map.insert(old_idx, new_idx);
    new_idx
}

fn tri_area_sq(positions: &[[f32; 3]], i0: u32, i1: u32, i2: u32) -> f64 {
    let a = positions[i0 as usize];
    let b = positions[i1 as usize];
    let c = positions[i2 as usize];
    let ab = [
        f64::from(b[0]) - f64::from(a[0]),
        f64::from(b[1]) - f64::from(a[1]),
        f64::from(b[2]) - f64::from(a[2]),
    ];
    let ac = [
        f64::from(c[0]) - f64::from(a[0]),
        f64::from(c[1]) - f64::from(a[1]),
        f64::from(c[2]) - f64::from(a[2]),
    ];
    let cx = ab[1] * ac[2] - ab[2] * ac[1];
    let cy = ab[2] * ac[0] - ab[0] * ac[2];
    let cz = ab[0] * ac[1] - ab[1] * ac[0];
    0.25 * (cx * cx + cy * cy + cz * cz)
}

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        if self.rank[ra] < self.rank[rb] {
            self.parent[ra] = rb;
        } else if self.rank[ra] > self.rank[rb] {
            self.parent[rb] = ra;
        } else {
            self.parent[rb] = ra;
            self.rank[ra] = self.rank[ra].saturating_add(1);
        }
    }

    fn count_components(&mut self) -> usize {
        let mut count = 0_usize;
        for i in 0..self.parent.len() {
            if self.find(i) == i {
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed_tetra(offset: [f32; 3]) -> TriMesh {
        let [ox, oy, oz] = offset;
        TriMesh::from_indexed(
            vec![
                [ox, oy, oz],
                [ox + 10.0, oy, oz],
                [ox, oy + 10.0, oz],
                [ox, oy, oz + 10.0],
            ],
            vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
        )
        .expect("valid tetra")
    }

    #[test]
    fn closed_tetra_is_watertight_manifold() {
        let tet = closed_tetra([0.0, 0.0, 0.0]);
        let report = analyze_mesh(&tet);
        assert!(report.is_watertight_manifold());
        assert_eq!(report.shell_count, 1);
        assert_eq!(report.boundary_edge_count, 0);
        assert_eq!(report.non_manifold_edge_count, 0);
        assert_eq!(report.flipped_edge_count, 0);
        assert_eq!(report.degenerate_triangle_count, 0);
    }

    #[test]
    fn open_mesh_reports_boundary_edges() {
        let mesh = TriMesh::from_indexed(
            vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
            vec![[0, 2, 1], [0, 1, 3], [1, 2, 3]],
        )
        .expect("open tet");
        let report = analyze_mesh(&mesh);
        assert!(!report.is_watertight_manifold());
        assert_eq!(report.boundary_edge_count, 3);
        assert_eq!(report.shell_count, 1);
    }

    #[test]
    fn flipped_face_is_detected() {
        let mesh = TriMesh::from_indexed(
            vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
            // Face 0 is flipped relative to faces 1..3
            vec![[0, 1, 2], [0, 1, 3], [1, 2, 3], [2, 0, 3]],
        )
        .expect("flipped tet");
        let report = analyze_mesh(&mesh);
        assert!(!report.is_watertight_manifold());
        assert_eq!(report.flipped_edge_count, 3);
    }

    #[test]
    fn non_manifold_and_degenerate_triangles_are_detected() {
        // 3 triangles sharing edge (0, 1) plus 1 degenerate triangle
        let mesh = TriMesh::from_indexed(
            vec![
                [0.0, 0.0, 0.0],
                [10.0, 0.0, 0.0],
                [0.0, 10.0, 0.0],
                [0.0, -10.0, 0.0],
                [0.0, 0.0, 10.0],
                [5.0, 0.0, 0.0],
            ],
            vec![[0, 1, 2], [0, 1, 3], [0, 1, 4], [0, 5, 1]],
        )
        .expect("non-manifold");
        let report = analyze_mesh(&mesh);
        assert!(!report.is_watertight_manifold());
        assert_eq!(report.non_manifold_edge_count, 1);
        assert_eq!(report.degenerate_triangle_count, 1);
    }

    #[test]
    fn splits_two_shells_into_separate_meshes() {
        let a = closed_tetra([0.0, 0.0, 0.0]);
        let b = closed_tetra([50.0, 0.0, 0.0]);
        let mut positions = a.positions.clone();
        let offset = positions.len() as u32;
        positions.extend_from_slice(&b.positions);
        let mut indices = a.indices.clone();
        indices.extend(b.indices.iter().map(|&[i0, i1, i2]| [i0 + offset, i1 + offset, i2 + offset]));

        let combined = TriMesh::from_indexed(positions, indices).expect("combined");
        let report = analyze_mesh(&combined);
        assert_eq!(report.shell_count, 2);
        assert!(report.is_watertight_manifold());

        let shells = split_shells(&combined).expect("split");
        assert_eq!(shells.len(), 2);
        assert_eq!(shells[0].indices.len(), 4);
        assert_eq!(shells[1].indices.len(), 4);
        assert!(analyze_mesh(&shells[0]).is_watertight_manifold());
        assert!(analyze_mesh(&shells[1]).is_watertight_manifold());
    }
}
