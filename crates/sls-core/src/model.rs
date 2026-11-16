//! Core SLS data model: triangle meshes, machine profiles, part instances, and build jobs.

use std::sync::Arc;

use crate::{Result, SlsError};

/// Axis-aligned bounding box `[min, max]` in millimetres.
pub type Aabb = [[f32; 3]; 2];

/// Row-major 3x4 rigid/affine transform matrix (`[row][col]`).
pub type Transform3x4 = [[f32; 4]; 3];

/// Identity 3x4 transform.
pub const IDENTITY_TRANSFORM: Transform3x4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

/// Unique identifier for a part prototype in a [`BuildJob`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PartId(pub u32);

/// Unique identifier for a placed part instance in a [`BuildJob`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstanceId(pub u32);

/// Per-part orientation constraint for nesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotationConstraint {
    /// Any 3D orientation is allowed.
    #[default]
    Free,
    /// Only rotation around the Z (build) axis is allowed (preserves XY strength / surface finish).
    ZOnly,
    /// Orientation is locked.
    Fixed,
}

/// Target SLS machine build-volume profile.
#[derive(Debug, Clone, PartialEq)]
pub enum MachineProfile {
    /// EOS Formiga P110 (`160 x 210 x 330 mm`).
    FormigaP110,
    /// Farsoon HT403P (`400 x 400 x 450 mm`).
    FarsoonHT403P,
    /// Custom rectangular build volume in millimetres.
    Custom { name: String, size_mm: [f32; 3] },
}

impl MachineProfile {
    /// Human-readable machine name.
    pub fn name(&self) -> &str {
        match self {
            Self::FormigaP110 => "EOS Formiga P110",
            Self::FarsoonHT403P => "Farsoon HT403P",
            Self::Custom { name, .. } => name.as_str(),
        }
    }

    /// Build envelope dimensions `[width_x, depth_y, height_z]` in millimetres.
    pub fn size_mm(&self) -> [f32; 3] {
        match self {
            Self::FormigaP110 => [160.0, 210.0, 330.0],
            Self::FarsoonHT403P => [400.0, 400.0, 450.0],
            Self::Custom { size_mm, .. } => *size_mm,
        }
    }

    /// Total build-chamber volume in cubic millimetres.
    pub fn volume_mm3(&self) -> f64 {
        let [x, y, z] = self.size_mm();
        f64::from(x) * f64::from(y) * f64::from(z)
    }
}

/// Pure-Rust indexed triangle mesh in part-local coordinates (millimetres).
#[derive(Debug, Clone)]
pub struct TriMesh {
    /// Vertex positions `(x, y, z)`.
    pub positions: Vec<[f32; 3]>,
    /// Triangle vertex indices.
    pub indices: Vec<[u32; 3]>,
    /// Unit face normals (one per triangle).
    pub tri_normals: Vec<[f32; 3]>,
    /// Local axis-aligned bounding box `[min, max]`.
    pub bounds: Aabb,
    /// Signed mesh volume in cubic millimetres (positive for outward-wound closed meshes).
    pub volume_mm3: f64,
    /// Total triangle surface area in square millimetres.
    pub surface_area_mm2: f64,
}

impl TriMesh {
    /// Construct a [`TriMesh`] from positions and triangle indices, validating finiteness and bounds.
    pub fn from_indexed(positions: Vec<[f32; 3]>, indices: Vec<[u32; 3]>) -> Result<Self> {
        if positions.is_empty() || indices.is_empty() {
            return Err(SlsError::InvalidInput("mesh has no triangles".to_owned()));
        }

        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for p in &positions {
            if !p.iter().all(|v| v.is_finite()) {
                return Err(SlsError::InvalidInput("mesh contains non-finite vertex coordinate".to_owned()));
            }
            for k in 0..3 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
        }

        let n_verts = positions.len();
        let mut tri_normals = Vec::with_capacity(indices.len());
        let mut signed_vol6 = 0.0_f64;
        let mut surface_area_mm2 = 0.0_f64;

        for tri in &indices {
            let [i0, i1, i2] = *tri;
            if i0 as usize >= n_verts || i1 as usize >= n_verts || i2 as usize >= n_verts {
                return Err(SlsError::InvalidInput("triangle index out of range".to_owned()));
            }
            let a = positions[i0 as usize];
            let b = positions[i1 as usize];
            let c = positions[i2 as usize];
            tri_normals.push(compute_face_normal(a, b, c));
            signed_vol6 += signed_tetra_vol6(a, b, c);
            surface_area_mm2 += triangle_area(a, b, c);
        }

        Ok(Self {
            positions,
            indices,
            tri_normals,
            bounds: [min, max],
            volume_mm3: (signed_vol6 / 6.0).abs(),
            surface_area_mm2,
        })
    }

    /// Bounding box dimensions `[dx, dy, dz]` in millimetres.
    pub fn dimensions(&self) -> [f32; 3] {
        [
            self.bounds[1][0] - self.bounds[0][0],
            self.bounds[1][1] - self.bounds[0][1],
            self.bounds[1][2] - self.bounds[0][2],
        ]
    }
}

fn compute_face_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let nx = ab[1] * ac[2] - ab[2] * ac[1];
    let ny = ab[2] * ac[0] - ab[0] * ac[2];
    let nz = ab[0] * ac[1] - ab[1] * ac[0];
    let len = (nx * nx + ny * ny + nz * nz).sqrt();
    if len > 1e-12 {
        [nx / len, ny / len, nz / len]
    } else {
        [0.0, 0.0, 1.0]
    }
}

fn signed_tetra_vol6(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> f64 {
    let (ax, ay, az) = (f64::from(a[0]), f64::from(a[1]), f64::from(a[2]));
    let (bx, by, bz) = (f64::from(b[0]), f64::from(b[1]), f64::from(b[2]));
    let (cx, cy, cz) = (f64::from(c[0]), f64::from(c[1]), f64::from(c[2]));
    ax * (by * cz - bz * cy) + ay * (bz * cx - bx * cz) + az * (bx * cy - by * cx)
}

fn triangle_area(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> f64 {
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
    0.5 * (cx * cx + cy * cy + cz * cz).sqrt()
}

/// A unique imported part mesh shared across one or more placed instances.
#[derive(Debug, Clone)]
pub struct PartPrototype {
    /// Prototype identifier.
    pub id: PartId,
    /// Display name (typically derived from filename or 3MF object).
    pub name: String,
    /// Shared triangle mesh.
    pub mesh: Arc<TriMesh>,
    /// Default orientation constraint for new instances.
    pub default_constraint: RotationConstraint,
}

/// One placed instance of a [`PartPrototype`] in the build chamber.
#[derive(Debug, Clone)]
pub struct PartInstance {
    /// Instance identifier.
    pub id: InstanceId,
    /// Referenced prototype.
    pub part_id: PartId,
    /// Rigid 3x4 row-major placement transform in build-chamber coordinates (mm).
    pub transform: Transform3x4,
    /// Orientation constraint.
    pub constraint: RotationConstraint,
    /// Whether manual lock prevents the packer from moving this instance.
    pub locked: bool,
}

impl PartInstance {
    /// Return the `[x, y, z]` translation component of `transform`.
    pub fn translation(&self) -> [f32; 3] {
        [self.transform[0][3], self.transform[1][3], self.transform[2][3]]
    }

    /// Set the `[x, y, z]` translation component of `transform`.
    pub fn set_translation(&mut self, xyz: [f32; 3]) {
        self.transform[0][3] = xyz[0];
        self.transform[1][3] = xyz[1];
        self.transform[2][3] = xyz[2];
    }

    /// Extract `[rx, ry, rz]` Euler angles (degrees) from `transform` (`R = Rz * Ry * Rx`).
    pub fn euler_deg(&self) -> [f32; 3] {
        let m = &self.transform;
        let sy = (-m[2][0]).clamp(-1.0, 1.0);
        let ry = sy.asin();
        let (rx, rz) = if ry.cos().abs() > 1e-5 {
            (m[2][1].atan2(m[2][2]), m[1][0].atan2(m[0][0]))
        } else {
            (m[1][2].atan2(m[1][1]), 0.0)
        };
        [rx.to_degrees(), ry.to_degrees(), rz.to_degrees()]
    }

    /// Compute the transformed world-space AABB for a local bounding box.
    pub fn world_bounds(&self, local: Aabb) -> Aabb {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for ix in 0..2 {
            for iy in 0..2 {
                for iz in 0..2 {
                    let p = [local[ix][0], local[iy][1], local[iz][2]];
                    let w = transform_point(&self.transform, p);
                    for k in 0..3 {
                        min[k] = min[k].min(w[k]);
                        max[k] = max[k].max(w[k]);
                    }
                }
            }
        }
        [min, max]
    }
}

/// Apply a row-major 3x4 transform to a 3D point.
pub fn transform_point(m: &Transform3x4, p: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * p[0] + m[0][1] * p[1] + m[0][2] * p[2] + m[0][3],
        m[1][0] * p[0] + m[1][1] * p[1] + m[1][2] * p[2] + m[1][3],
        m[2][0] * p[0] + m[2][1] * p[1] + m[2][2] * p[2] + m[2][3],
    ]
}

/// Build a rigid 3x4 transform from translation (mm) and XYZ Euler angles (degrees).
pub fn transform_from_translation_euler_deg(translation: [f32; 3], euler_deg: [f32; 3]) -> Transform3x4 {
    let (rx, ry, rz) = (
        euler_deg[0].to_radians(),
        euler_deg[1].to_radians(),
        euler_deg[2].to_radians(),
    );
    let (sx, cx) = rx.sin_cos();
    let (sy, cy) = ry.sin_cos();
    let (sz, cz) = rz.sin_cos();

    // R = Rz * Ry * Rx
    [
        [cy * cz, cz * sx * sy - cx * sz, cx * cz * sy + sx * sz, translation[0]],
        [cy * sz, cx * cz + sx * sy * sz, cx * sy * sz - cz * sx, translation[1]],
        [-sy, cy * sx, cx * cy, translation[2]],
    ]
}

/// Complete SLS build job state.
#[derive(Debug, Clone)]
pub struct BuildJob {
    /// Target machine build volume.
    pub machine: MachineProfile,
    /// Minimum required part-to-part clearance in millimetres.
    pub clearance_mm: f32,
    /// Minimum required margin from X/Y chamber walls and top Z in millimetres.
    pub wall_margin_mm: f32,
    /// Unique part prototypes.
    pub parts: Vec<PartPrototype>,
    /// Placed part instances.
    pub instances: Vec<PartInstance>,
    next_part_id: u32,
    next_instance_id: u32,
}

impl Default for BuildJob {
    fn default() -> Self {
        Self::new(MachineProfile::FormigaP110)
    }
}

impl BuildJob {
    /// Create an empty build job for `machine` with `2.5 mm` clearance and `5.0 mm` wall margin.
    pub fn new(machine: MachineProfile) -> Self {
        Self {
            machine,
            clearance_mm: 2.5,
            wall_margin_mm: 5.0,
            parts: Vec::new(),
            instances: Vec::new(),
            next_part_id: 1,
            next_instance_id: 1,
        }
    }

    /// Add a new part prototype and return its [`PartId`].
    pub fn add_part(&mut self, name: impl Into<String>, mesh: Arc<TriMesh>) -> PartId {
        let id = PartId(self.next_part_id);
        self.next_part_id += 1;
        self.parts.push(PartPrototype {
            id,
            name: name.into(),
            mesh,
            default_constraint: RotationConstraint::Free,
        });
        id
    }

    /// Look up a prototype by [`PartId`].
    pub fn part(&self, id: PartId) -> Option<&PartPrototype> {
        self.parts.iter().find(|p| p.id == id)
    }

    /// Count placed instances referencing `part_id`.
    pub fn instance_count_for_part(&self, part_id: PartId) -> usize {
        self.instances.iter().filter(|i| i.part_id == part_id).count()
    }

    /// Remove an instance by [`InstanceId`], returning `true` if found and removed.
    pub fn remove_instance(&mut self, id: InstanceId) -> bool {
        let before = self.instances.len();
        self.instances.retain(|i| i.id != id);
        self.instances.len() < before
    }

    /// Place a new instance of `part_id` with `transform`.
    pub fn add_instance(&mut self, part_id: PartId, transform: Transform3x4) -> Result<InstanceId> {
        let constraint = self
            .part(part_id)
            .ok_or_else(|| SlsError::InvalidInput(format!("unknown part id {}", part_id.0)))?
            .default_constraint;
        let id = InstanceId(self.next_instance_id);
        self.next_instance_id += 1;
        self.instances.push(PartInstance {
            id,
            part_id,
            transform,
            constraint,
            locked: false,
        });
        Ok(id)
    }

    /// Place a new instance centered in XY at `z = 0` (bottom-aligned to build floor).
    pub fn add_instance_centered_on_floor(&mut self, part_id: PartId) -> Result<InstanceId> {
        let mesh = self
            .part(part_id)
            .ok_or_else(|| SlsError::InvalidInput(format!("unknown part id {}", part_id.0)))?
            .mesh
            .clone();
        let [lo, hi] = mesh.bounds;
        let [bx, by, _] = self.machine.size_mm();
        let tx = bx * 0.5 - (lo[0] + hi[0]) * 0.5;
        let ty = by * 0.5 - (lo[1] + hi[1]) * 0.5;
        let tz = -lo[2];
        let mut transform = IDENTITY_TRANSFORM;
        transform[0][3] = tx;
        transform[1][3] = ty;
        transform[2][3] = tz;
        self.add_instance(part_id, transform)
    }

    /// Duplicate an existing instance `count` times with a small X-step offset.
    pub fn duplicate_instance(&mut self, source_id: InstanceId, count: usize) -> Result<Vec<InstanceId>> {
        let src = self
            .instances
            .iter()
            .find(|i| i.id == source_id)
            .cloned()
            .ok_or_else(|| SlsError::InvalidInput(format!("unknown instance id {}", source_id.0)))?;
        let dims = self
            .part(src.part_id)
            .map(|p| p.mesh.dimensions())
            .unwrap_or([10.0, 10.0, 10.0]);
        let step_x = dims[0] + self.clearance_mm;
        let mut created = Vec::with_capacity(count);
        for k in 1..=count {
            let mut transform = src.transform;
            transform[0][3] += step_x * k as f32;
            let id = self.add_instance(src.part_id, transform)?;
            created.push(id);
        }
        Ok(created)
    }

    /// Highest Z coordinate (mm) reached by any placed instance.
    pub fn max_z_mm(&self) -> f32 {
        let mut max_z = 0.0_f32;
        for inst in &self.instances {
            if let Some(proto) = self.part(inst.part_id) {
                let wb = inst.world_bounds(proto.mesh.bounds);
                max_z = max_z.max(wb[1][2]);
            }
        }
        max_z
    }

    /// Total sintered volume of all placed instances in cubic millimetres.
    pub fn total_part_volume_mm3(&self) -> f64 {
        self.instances
            .iter()
            .filter_map(|inst| self.part(inst.part_id).map(|p| p.mesh.volume_mm3))
            .sum()
    }

    /// Sintered packing density (%) relative to the occupied build height (`W * D * max_Z`).
    pub fn packing_density_pct(&self) -> f64 {
        let max_z = f64::from(self.max_z_mm());
        if max_z <= 1e-6 || self.instances.is_empty() {
            return 0.0;
        }
        let [wx, wy, _] = self.machine.size_mm();
        let used_box_vol = f64::from(wx) * f64::from(wy) * max_z;
        (self.total_part_volume_mm3() / used_box_vol) * 100.0
    }

    /// Check whether an instance's world AABB lies inside the machine envelope with `wall_margin_mm`.
    pub fn is_instance_in_bounds(&self, id: InstanceId) -> bool {
        let Some(inst) = self.instances.iter().find(|i| i.id == id) else {
            return false;
        };
        let Some(proto) = self.part(inst.part_id) else {
            return false;
        };
        let [lo, hi] = inst.world_bounds(proto.mesh.bounds);
        let [wx, wy, wz] = self.machine.size_mm();
        let m = self.wall_margin_mm.max(0.0);
        const EPS: f32 = 1e-3;
        lo[0] >= m - EPS
            && lo[1] >= m - EPS
            && lo[2] >= -EPS
            && hi[0] <= wx - m + EPS
            && hi[1] <= wy - m + EPS
            && hi[2] <= wz - m + EPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_cube() -> Arc<TriMesh> {
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
        Arc::new(TriMesh::from_indexed(positions, indices).expect("valid cube"))
    }

    #[test]
    fn rejects_non_finite_or_empty_mesh() {
        assert!(TriMesh::from_indexed(vec![], vec![]).is_err());
        assert!(TriMesh::from_indexed(vec![[f32::NAN, 0.0, 0.0]], vec![[0, 0, 0]]).is_err());
        assert!(TriMesh::from_indexed(vec![[0.0, 0.0, 0.0]], vec![[0, 1, 0]]).is_err());
    }

    #[test]
    fn computes_cube_volume_and_job_metrics() {
        let cube = unit_cube();
        assert!((cube.volume_mm3 - 1000.0).abs() < 1e-3);
        assert!((cube.surface_area_mm2 - 600.0).abs() < 1e-3);
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("cube", cube);
        let iid = job.add_instance_centered_on_floor(pid).expect("placed");
        assert!(job.is_instance_in_bounds(iid));
        assert!((job.max_z_mm() - 10.0).abs() < 1e-4);
        assert!((job.total_part_volume_mm3() - 1000.0).abs() < 1e-3);
        assert!(job.packing_density_pct() > 0.0);
        assert_eq!(job.instance_count_for_part(pid), 1);

        // Moving inside wall margin triggers out-of-bounds.
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == iid) {
            inst.set_translation([1.0, 10.0, 0.0]);
        }
        assert!(!job.is_instance_in_bounds(iid));
        assert!(job.remove_instance(iid));
        assert_eq!(job.instance_count_for_part(pid), 0);
    }

    #[test]
    fn euler_deg_roundtrips_transform() {
        let pos = [12.5, 34.0, 56.25];
        let deg = [15.0, -25.0, 40.0];
        let tf = transform_from_translation_euler_deg(pos, deg);
        let inst = PartInstance {
            id: InstanceId(1),
            part_id: PartId(1),
            transform: tf,
            constraint: RotationConstraint::Free,
            locked: false,
        };
        let out = inst.euler_deg();
        for k in 0..3 {
            assert!((out[k] - deg[k]).abs() < 1e-3, "axis {k}: {} vs {}", out[k], deg[k]);
        }
    }
}
