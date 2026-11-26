//! Core NestCraft data model: triangle meshes, machine profiles, part instances, and build jobs.

use std::sync::Arc;

use crate::{NestCraftError, Result};

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

/// Unique identifier for a hierarchical project tree group in a [`BuildJob`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(pub u32);

/// Named group organizing part instances in the project tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartGroup {
    /// Group identifier.
    pub id: GroupId,
    /// Display name.
    pub name: String,
    /// Whether all instances in this group are hidden from the viewport.
    pub hidden: bool,
}

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

/// Where world `(0, 0, 0)` sits on the machine bed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BedOrigin {
    /// `(0, 0, 0)` is at the front-left corner (`X in [0, W]`, `Y in [0, D]`).
    #[default]
    FrontLeft,
    /// `(0, 0, 0)` is at the bed center (`X in [-W/2, W/2]`, `Y in [-D/2, D/2]`).
    Center,
}

/// Target machine build-volume profile.
#[derive(Debug, Clone, PartialEq)]
pub enum MachineProfile {
    /// EOS Formiga P110 (`200 x 250 x 330 mm`).
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
            Self::FormigaP110 => [200.0, 250.0, 330.0],
            Self::FarsoonHT403P => [400.0, 400.0, 450.0],
            Self::Custom { size_mm, .. } => *size_mm,
        }
    }

    /// Total build-chamber volume in cubic millimetres.
    pub fn volume_mm3(&self) -> f64 {
        let [x, y, z] = self.size_mm();
        f64::from(x) * f64::from(y) * f64::from(z)
    }

    /// Construct a validated custom machine profile with dimensions clamped to at least `10 mm`.
    pub fn custom(name: impl Into<String>, size_mm: [f32; 3]) -> Self {
        let raw_name = name.into();
        let trimmed = raw_name.trim();
        let clean_name = if trimmed.is_empty() {
            "Custom Chamber".to_owned()
        } else {
            trimmed.to_owned()
        };
        let clamped = [
            size_mm[0].clamp(10.0, 2000.0),
            size_mm[1].clamp(10.0, 2000.0),
            size_mm[2].clamp(10.0, 2000.0),
        ];
        Self::Custom {
            name: clean_name,
            size_mm: clamped,
        }
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
            return Err(NestCraftError::InvalidInput("mesh has no triangles".to_owned()));
        }

        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for p in &positions {
            if !p.iter().all(|v| v.is_finite()) {
                return Err(NestCraftError::InvalidInput("mesh contains non-finite vertex coordinate".to_owned()));
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
                return Err(NestCraftError::InvalidInput("triangle index out of range".to_owned()));
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

/// Return `true` when `a` and `b` have identical vertex bit patterns and triangle index lists.
pub fn meshes_bit_equal(a: &TriMesh, b: &TriMesh) -> bool {
    a.positions.len() == b.positions.len()
        && a.indices.len() == b.indices.len()
        && a.positions.iter().zip(&b.positions).all(|(pa, pb)| {
            pa[0].to_bits() == pb[0].to_bits()
                && pa[1].to_bits() == pb[1].to_bits()
                && pa[2].to_bits() == pb[2].to_bits()
        })
        && a.indices == b.indices
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
    /// Whether this instance is hidden in the 3D viewport.
    pub hidden: bool,
    /// Whether this instance is rendered 50% transparent in the 3D viewport.
    pub transparent: bool,
    /// Optional custom RGB display color (`None` uses the active theme color).
    pub color: Option<[u8; 3]>,
    /// Optional project-tree group membership.
    pub group_id: Option<GroupId>,
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

/// User-configured STL keep-out / no-build zone in build-chamber coordinates.
#[derive(Debug, Clone)]
pub struct NoBuildZone {
    /// Display name (typically derived from the imported STL file name).
    pub name: String,
    /// Keep-out volume mesh in local coordinates (millimetres).
    pub mesh: Arc<TriMesh>,
    /// Translation `[x, y, z]` in millimetres.
    pub position: [f32; 3],
    /// Euler rotation `[rx, ry, rz]` in degrees.
    pub rotation_deg: [f32; 3],
    /// Extra safety clearance around the no-build zone in millimetres.
    pub clearance_mm: f32,
    /// Whether parts are checked against this zone.
    pub enabled: bool,
    /// Whether the zone mesh is rendered in the 3D viewport.
    pub visible: bool,
    /// Whether viewport dragging of the no-build zone is locked.
    pub locked: bool,
}

impl NoBuildZone {
    /// Construct a new enabled & visible no-build zone at the origin.
    pub fn new(name: impl Into<String>, mesh: Arc<TriMesh>) -> Self {
        Self {
            name: name.into(),
            mesh,
            position: [0.0, 0.0, 0.0],
            rotation_deg: [0.0, 0.0, 0.0],
            clearance_mm: 0.0,
            enabled: true,
            visible: true,
            locked: true,
        }
    }

    /// Rigid 3x4 world transform for this no-build zone.
    pub fn transform(&self) -> Transform3x4 {
        transform_from_translation_euler_deg(self.position, self.rotation_deg)
    }

    /// World-space axis-aligned bounding box of the transformed no-build zone.
    pub fn world_bounds(&self) -> Aabb {
        transform_aabb(self.mesh.bounds, &self.transform())
    }
}

fn transform_aabb(bounds: Aabb, m: &Transform3x4) -> Aabb {
    let [lo, hi] = bounds;
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for sx in [lo[0], hi[0]] {
        for sy in [lo[1], hi[1]] {
            for sz in [lo[2], hi[2]] {
                let p = transform_point(m, [sx, sy, sz]);
                for k in 0..3 {
                    min[k] = min[k].min(p[k]);
                    max[k] = max[k].max(p[k]);
                }
            }
        }
    }
    [min, max]
}

/// Complete NestCraft build job state.
#[derive(Debug, Clone)]
pub struct BuildJob {
    /// Target machine build volume.
    pub machine: MachineProfile,
    /// Minimum required part-to-part clearance in millimetres.
    pub clearance_mm: f32,
    /// Minimum required margin from X/Y chamber walls and top Z in millimetres.
    pub wall_margin_mm: f32,
    /// Minimum allowed Z coordinate (floor height) in millimetres (default `0.0`).
    pub min_z_mm: f32,
    /// Where world `(0, 0, 0)` sits on the machine bed.
    pub bed_origin: BedOrigin,
    /// Selected SLS material identifier (see `report::SLS_MATERIALS`).
    pub material_id: String,
    /// Unique part prototypes.
    pub parts: Vec<PartPrototype>,
    /// Placed part instances.
    pub instances: Vec<PartInstance>,
    /// Project-tree groups.
    pub groups: Vec<PartGroup>,
    /// Optional STL keep-out / no-build zone.
    pub no_build_zone: Option<NoBuildZone>,
    pub(crate) next_part_id: u32,
    pub(crate) next_instance_id: u32,
    pub(crate) next_group_id: u32,
}

impl Default for BuildJob {
    fn default() -> Self {
        Self::new(MachineProfile::FormigaP110)
    }
}

impl BuildJob {
    /// Create an empty build job for `machine` with `2.5 mm` clearance and `5.0 mm` wall margin.
    pub fn new(machine: MachineProfile) -> Self {
        let bed_origin = match machine {
            MachineProfile::FarsoonHT403P => BedOrigin::Center,
            _ => BedOrigin::FrontLeft,
        };
        Self {
            machine,
            clearance_mm: 2.5,
            wall_margin_mm: 5.0,
            min_z_mm: 0.0,
            bed_origin,
            material_id: "pa2200".to_owned(),
            parts: Vec::new(),
            instances: Vec::new(),
            groups: Vec::new(),
            no_build_zone: None,
            next_part_id: 1,
            next_instance_id: 1,
            next_group_id: 1,
        }
    }

    /// Reference bed XY bounds `([min_x, min_y], [max_x, max_y])` in world coordinates.
    pub fn bed_xy_bounds(&self) -> ([f32; 2], [f32; 2]) {
        let [w, d, _] = self.machine.size_mm();
        match self.bed_origin {
            BedOrigin::FrontLeft => ([0.0, 0.0], [w, d]),
            BedOrigin::Center => ([-w * 0.5, -d * 0.5], [w * 0.5, d * 0.5]),
        }
    }

    /// World XY coordinates of the bed center.
    pub fn bed_center_xy(&self) -> [f32; 2] {
        let [w, d, _] = self.machine.size_mm();
        match self.bed_origin {
            BedOrigin::FrontLeft => [w * 0.5, d * 0.5],
            BedOrigin::Center => [0.0, 0.0],
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

    /// Reuse an existing prototype with matching `name` and bit-identical geometry, or
    /// create a new prototype sharing the existing `Arc<TriMesh>` if only geometry matches.
    ///
    /// Returns `(part_id, created_new_prototype)`.
    pub fn add_or_reuse_part(&mut self, name: impl Into<String>, mesh: Arc<TriMesh>) -> (PartId, bool) {
        let name = name.into();
        if let Some(existing) = self.parts.iter().find(|p| {
            p.name == name && (Arc::ptr_eq(&p.mesh, &mesh) || meshes_bit_equal(&p.mesh, &mesh))
        }) {
            return (existing.id, false);
        }
        let shared_mesh = self
            .parts
            .iter()
            .find(|p| Arc::ptr_eq(&p.mesh, &mesh) || meshes_bit_equal(&p.mesh, &mesh))
            .map_or(mesh, |p| Arc::clone(&p.mesh));
        let pid = self.add_part(name, shared_mesh);
        (pid, true)
    }

    /// Remove any [`PartPrototype`] no longer referenced by any [`PartInstance`], returning their IDs.
    pub fn prune_unused_parts(&mut self) -> Vec<PartId> {
        let referenced: std::collections::HashSet<PartId> =
            self.instances.iter().map(|i| i.part_id).collect();
        let mut removed = Vec::new();
        self.parts.retain(|p| {
            if referenced.contains(&p.id) {
                true
            } else {
                removed.push(p.id);
                false
            }
        });
        removed
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
        if self.instances.len() < before {
            self.prune_unused_parts();
            true
        } else {
            false
        }
    }

    /// Place a new instance of `part_id` with `transform`.
    pub fn add_instance(&mut self, part_id: PartId, transform: Transform3x4) -> Result<InstanceId> {
        let constraint = self
            .part(part_id)
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown part id {}", part_id.0)))?
            .default_constraint;
        let id = InstanceId(self.next_instance_id);
        self.next_instance_id += 1;
        self.instances.push(PartInstance {
            id,
            part_id,
            transform,
            constraint,
            locked: false,
            hidden: false,
            transparent: false,
            color: None,
            group_id: None,
        });
        Ok(id)
    }

    /// Place a new instance centered in XY at `z = 0` (bottom-aligned to build floor).
    pub fn add_instance_centered_on_floor(&mut self, part_id: PartId) -> Result<InstanceId> {
        let mesh = self
            .part(part_id)
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown part id {}", part_id.0)))?
            .mesh
            .clone();
        let [lo, hi] = mesh.bounds;
        let [cx, cy] = self.bed_center_xy();
        let tx = cx - (lo[0] + hi[0]) * 0.5;
        let ty = cy - (lo[1] + hi[1]) * 0.5;
        let tz = self.min_z_mm.max(0.0) - lo[2];
        let mut transform = IDENTITY_TRANSFORM;
        transform[0][3] = tx;
        transform[1][3] = ty;
        transform[2][3] = tz;
        self.add_instance(part_id, transform)
    }

    /// Place a new instance on `Z = 0` side-by-side with existing parts so it does not overlap.
    ///
    /// When no instances exist yet, this places the part centered on the reference bed at `Z = 0`.
    /// Otherwise it places the part to the right (`+X`) of the rightmost existing instance with
    /// `self.clearance_mm` spacing, without restricting placement to the reference bed size.
    pub fn add_instance_on_floor_next(&mut self, part_id: PartId) -> Result<InstanceId> {
        if self.instances.is_empty() {
            return self.add_instance_centered_on_floor(part_id);
        }
        let mesh = self
            .part(part_id)
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown part id {}", part_id.0)))?
            .mesh
            .clone();
        let [lo, hi] = mesh.bounds;
        let [cx, cy] = self.bed_center_xy();

        let mut max_x = f32::NEG_INFINITY;
        for inst in &self.instances {
            if let Some(proto) = self.part(inst.part_id) {
                let wb = inst.world_bounds(proto.mesh.bounds);
                max_x = max_x.max(wb[1][0]);
            }
        }

        let tx = if max_x.is_finite() {
            max_x + self.clearance_mm.max(0.0) - lo[0]
        } else {
            cx - (lo[0] + hi[0]) * 0.5
        };
        let ty = cy - (lo[1] + hi[1]) * 0.5;
        let tz = self.min_z_mm.max(0.0) - lo[2];
        let mut transform = IDENTITY_TRANSFORM;
        transform[0][3] = tx;
        transform[1][3] = ty;
        transform[2][3] = tz;
        self.add_instance(part_id, transform)
    }

    /// Duplicate an existing instance `count` times with a small X-step offset.
    pub fn duplicate_instance(&mut self, source_id: InstanceId, count: usize) -> Result<Vec<InstanceId>> {
        self.duplicate_instance_grid(source_id, [count + 1, 1, 1], [self.clearance_mm; 3], None)
    }

    /// Duplicate an existing instance into a compact 3D grid of `counts = [nx, ny, nz]`
    /// total cells (skipping `(0, 0, 0)` which is the source instance), spaced by
    /// `world_dims[axis] + gap_mm[axis]`.
    ///
    /// When `max_new` is `Some(n)`, stops after creating `n` new instances.
    pub fn duplicate_instance_grid(
        &mut self,
        source_id: InstanceId,
        counts: [usize; 3],
        gap_mm: [f32; 3],
        max_new: Option<usize>,
    ) -> Result<Vec<InstanceId>> {
        let src = self
            .instances
            .iter()
            .find(|i| i.id == source_id)
            .cloned()
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown instance id {}", source_id.0)))?;
        let local_bounds = self
            .part(src.part_id)
            .map(|p| p.mesh.bounds)
            .unwrap_or([[0.0; 3], [10.0; 3]]);
        let wb = src.world_bounds(local_bounds);
        let dims = [
            (wb[1][0] - wb[0][0]).max(0.1),
            (wb[1][1] - wb[0][1]).max(0.1),
            (wb[1][2] - wb[0][2]).max(0.1),
        ];
        let steps = [
            dims[0] + gap_mm[0].max(0.0),
            dims[1] + gap_mm[1].max(0.0),
            dims[2] + gap_mm[2].max(0.0),
        ];
        let nx = counts[0].max(1);
        let ny = counts[1].max(1);
        let nz = counts[2].max(1);
        let target_new = max_new.unwrap_or_else(|| nx.saturating_mul(ny).saturating_mul(nz).saturating_sub(1));
        let mut created = Vec::with_capacity(target_new);

        'outer: for iz in 0..nz {
            for iy in 0..ny {
                for ix in 0..nx {
                    if ix == 0 && iy == 0 && iz == 0 {
                        continue;
                    }
                    if created.len() >= target_new {
                        break 'outer;
                    }
                    let mut transform = src.transform;
                    transform[0][3] += steps[0] * ix as f32;
                    transform[1][3] += steps[1] * iy as f32;
                    transform[2][3] += steps[2] * iz as f32;
                    let id = self.add_instance(src.part_id, transform)?;
                    self.clamp_instance_above_floor(id);
                    created.push(id);
                }
            }
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
        let ([x0, y0], [x1, y1]) = self.bed_xy_bounds();
        let [_, _, wz] = self.machine.size_mm();
        let m = self.wall_margin_mm.max(0.0);
        const EPS: f32 = 1e-3;
        lo[0] >= x0 + m - EPS
            && lo[1] >= y0 + m - EPS
            && lo[2] >= self.min_z_mm.max(0.0) - EPS
            && hi[0] <= x1 - m + EPS
            && hi[1] <= y1 - m + EPS
            && hi[2] <= wz - m + EPS
    }

    /// Check whether an instance's lowest world Z is on or above `self.min_z_mm` (default `0.0`).
    pub fn is_instance_above_floor(&self, id: InstanceId) -> bool {
        let Some(inst) = self.instances.iter().find(|i| i.id == id) else {
            return false;
        };
        let Some(proto) = self.part(inst.part_id) else {
            return false;
        };
        self.world_bounds_above_floor(inst.world_bounds(proto.mesh.bounds))
    }

    /// Check whether a world-space bounding box rests on or above `self.min_z_mm` (default `0.0`).
    pub fn world_bounds_above_floor(&self, wb: Aabb) -> bool {
        const EPS: f32 = 1e-3;
        wb[0][2] >= self.min_z_mm.max(0.0) - EPS
    }

    /// Lift an instance vertically if its lowest world Z is below `self.min_z_mm`.
    ///
    /// Returns `true` when the instance was adjusted.
    pub fn clamp_instance_above_floor(&mut self, id: InstanceId) -> bool {
        let Some(part_id) = self.instances.iter().find(|i| i.id == id).map(|i| i.part_id) else {
            return false;
        };
        let Some(local_bounds) = self.part(part_id).map(|p| p.mesh.bounds) else {
            return false;
        };
        let floor_z = self.min_z_mm.max(0.0);
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        let wb = inst.world_bounds(local_bounds);
        if wb[0][2] < floor_z {
            inst.transform[2][3] += floor_z - wb[0][2];
            true
        } else {
            false
        }
    }

    /// Return the lowest world-space Z coordinate of `id`, if found.
    pub fn instance_min_z(&self, id: InstanceId) -> Option<f32> {
        let inst = self.instances.iter().find(|i| i.id == id)?;
        let proto = self.part(inst.part_id)?;
        Some(inst.world_bounds(proto.mesh.bounds)[0][2])
    }

    /// Shift `id` vertically so its lowest world-space Z is `0.0`, keeping X and Y unchanged.
    ///
    /// Returns `true` when the instance was found and adjusted.
    pub fn reset_instance_z_to_zero(&mut self, id: InstanceId) -> bool {
        let Some(part_id) = self.instances.iter().find(|i| i.id == id).map(|i| i.part_id) else {
            return false;
        };
        let Some(local_bounds) = self.part(part_id).map(|p| p.mesh.bounds) else {
            return false;
        };
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        let min_z = inst.world_bounds(local_bounds)[0][2];
        if min_z.abs() > 1e-6 {
            inst.transform[2][3] -= min_z;
            true
        } else {
            false
        }
    }

    /// Return `true` when `id` intersects the active [`NoBuildZone`] (including its `clearance_mm`).
    pub fn is_instance_in_no_build_zone(&self, id: InstanceId) -> bool {
        let Some(inst) = self.instances.iter().find(|i| i.id == id) else {
            return false;
        };
        self.instance_in_no_build_zone(inst)
    }

    /// Return `true` when `inst`'s exact triangle mesh intersects or comes within `clearance_mm`
    /// of the active [`NoBuildZone`] mesh (never using the part's AABB alone as a false positive).
    pub fn instance_in_no_build_zone(&self, inst: &PartInstance) -> bool {
        let Some(zone) = &self.no_build_zone else {
            return false;
        };
        if !zone.enabled {
            return false;
        }
        let Some(proto) = self.part(inst.part_id) else {
            return false;
        };
        let pad = zone.clearance_mm.max(0.0);
        let pad_sq = pad * pad;
        let wb = inst.world_bounds(proto.mesh.bounds);
        let zb = zone.world_bounds();
        let wb_pad = [
            [wb[0][0] - pad, wb[0][1] - pad, wb[0][2] - pad],
            [wb[1][0] + pad, wb[1][1] + pad, wb[1][2] + pad],
        ];
        if !aabbs_overlap(wb_pad, zb) {
            return false;
        }

        let zb_pad = [
            [zb[0][0] - pad, zb[0][1] - pad, zb[0][2] - pad],
            [zb[1][0] + pad, zb[1][1] + pad, zb[1][2] + pad],
        ];
        let p_tf = inst.transform;
        let mut p_tris: Vec<([[f32; 3]; 3], Aabb)> = Vec::new();
        for tri in &proto.mesh.indices {
            let a = transform_point(&p_tf, proto.mesh.positions[tri[0] as usize]);
            let b = transform_point(&p_tf, proto.mesh.positions[tri[1] as usize]);
            let c = transform_point(&p_tf, proto.mesh.positions[tri[2] as usize]);
            let tb = tri_aabb(a, b, c);
            if aabbs_overlap(tb, zb_pad) {
                p_tris.push(([a, b, c], tb));
            }
        }

        let z_tf = zone.transform();
        let mut z_tris: Vec<([[f32; 3]; 3], Aabb)> = Vec::new();
        for tri in &zone.mesh.indices {
            let a = transform_point(&z_tf, zone.mesh.positions[tri[0] as usize]);
            let b = transform_point(&z_tf, zone.mesh.positions[tri[1] as usize]);
            let c = transform_point(&z_tf, zone.mesh.positions[tri[2] as usize]);
            let tb = tri_aabb(a, b, c);
            if aabbs_overlap(wb_pad, tb) {
                z_tris.push(([a, b, c], tb));
            }
        }

        for &(pt, pb) in &p_tris {
            let pb_pad = [
                [pb[0][0] - pad, pb[0][1] - pad, pb[0][2] - pad],
                [pb[1][0] + pad, pb[1][1] + pad, pb[1][2] + pad],
            ];
            for &(zt, zb_tri) in &z_tris {
                if !aabbs_overlap(pb_pad, zb_tri) {
                    continue;
                }
                if tri_tri_dist_sq(pt, zt) <= pad_sq + 1e-6 {
                    return true;
                }
            }
        }

        // Handle a part wholly contained inside a closed No-Build Zone.
        if let Some(first_local) = proto.mesh.positions.first() {
            let probe = transform_point(&p_tf, *first_local);
            if aabbs_overlap([probe, probe], zb) && point_inside_mesh_x_ray(probe, &zone.mesh, &z_tf) {
                return true;
            }
        }
        false
    }

    /// Return `true` when `wb` intersects the active [`NoBuildZone`] (including its `clearance_mm`).
    pub fn world_bounds_in_no_build_zone(&self, wb: Aabb) -> bool {
        let Some(zone) = &self.no_build_zone else {
            return false;
        };
        if !zone.enabled {
            return false;
        }
        let pad = zone.clearance_mm.max(0.0);
        let zb = zone.world_bounds();
        let wb_pad = [
            [wb[0][0] - pad, wb[0][1] - pad, wb[0][2] - pad],
            [wb[1][0] + pad, wb[1][1] + pad, wb[1][2] + pad],
        ];
        if !aabbs_overlap(wb_pad, zb) {
            return false;
        }
        let z_tf = zone.transform();
        for tri in &zone.mesh.indices {
            let p0 = transform_point(&z_tf, zone.mesh.positions[tri[0] as usize]);
            let p1 = transform_point(&z_tf, zone.mesh.positions[tri[1] as usize]);
            let p2 = transform_point(&z_tf, zone.mesh.positions[tri[2] as usize]);
            let tri_box = tri_aabb(p0, p1, p2);
            if aabbs_overlap(wb_pad, tri_box) {
                return true;
            }
        }
        false
    }

    /// Drop an instance vertically so its lowest world Z rests at `self.min_z_mm`.
    pub fn drop_instance_to_floor(&mut self, id: InstanceId) -> bool {
        let Some(part_id) = self.instances.iter().find(|i| i.id == id).map(|i| i.part_id) else {
            return false;
        };
        let Some(local_bounds) = self.part(part_id).map(|p| p.mesh.bounds) else {
            return false;
        };
        let floor_z = self.min_z_mm.max(0.0);
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        let wb = inst.world_bounds(local_bounds);
        inst.transform[2][3] += floor_z - wb[0][2];
        true
    }

    /// Center an instance horizontally at the bed center without changing Z or rotation.
    pub fn center_instance_xy(&mut self, id: InstanceId) -> bool {
        let Some(part_id) = self.instances.iter().find(|i| i.id == id).map(|i| i.part_id) else {
            return false;
        };
        let Some(local_bounds) = self.part(part_id).map(|p| p.mesh.bounds) else {
            return false;
        };
        let [tx_center, ty_center] = self.bed_center_xy();
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        let wb = inst.world_bounds(local_bounds);
        let cx = (wb[0][0] + wb[1][0]) * 0.5;
        let cy = (wb[0][1] + wb[1][1]) * 0.5;
        inst.transform[0][3] += tx_center - cx;
        inst.transform[1][3] += ty_center - cy;
        true
    }

    /// Create a new project-tree group and return its [`GroupId`].
    pub fn create_group(&mut self, name: impl Into<String>) -> GroupId {
        let id = GroupId(self.next_group_id);
        self.next_group_id += 1;
        let raw = name.into();
        let trimmed = raw.trim();
        let clean = if trimmed.is_empty() {
            format!("Group {}", id.0)
        } else {
            trimmed.to_owned()
        };
        self.groups.push(PartGroup {
            id,
            name: clean,
            hidden: false,
        });
        id
    }

    /// Remove a group by [`GroupId`], ungrouping any member instances.
    pub fn remove_group(&mut self, id: GroupId) -> bool {
        let before = self.groups.len();
        self.groups.retain(|g| g.id != id);
        if self.groups.len() == before {
            return false;
        }
        for inst in &mut self.instances {
            if inst.group_id == Some(id) {
                inst.group_id = None;
            }
        }
        true
    }

    /// Remove all groups that currently contain zero instances. Returns the number removed.
    pub fn remove_empty_groups(&mut self) -> usize {
        let used: std::collections::HashSet<GroupId> = self.instances.iter().filter_map(|i| i.group_id).collect();
        let before = self.groups.len();
        self.groups.retain(|g| used.contains(&g.id));
        before - self.groups.len()
    }

    /// Toggle or set a group's `hidden` flag.
    pub fn set_group_hidden(&mut self, id: GroupId, hidden: bool) -> bool {
        let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
            return false;
        };
        group.hidden = hidden;
        true
    }

    /// Assign or clear an instance's group membership.
    pub fn set_instance_group(&mut self, inst_id: InstanceId, group_id: Option<GroupId>) -> bool {
        if let Some(gid) = group_id
            && !self.groups.iter().any(|g| g.id == gid)
        {
            return false;
        }
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == inst_id) else {
            return false;
        };
        inst.group_id = group_id;
        true
    }

    /// Set `hidden` on a single instance.
    pub fn set_instance_hidden(&mut self, inst_id: InstanceId, hidden: bool) -> bool {
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == inst_id) else {
            return false;
        };
        inst.hidden = hidden;
        true
    }

    /// Set `transparent` (50% opacity) on a single instance.
    pub fn set_instance_transparent(&mut self, inst_id: InstanceId, transparent: bool) -> bool {
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == inst_id) else {
            return false;
        };
        inst.transparent = transparent;
        true
    }

    /// Set or clear custom RGB display `color` on a single instance.
    pub fn set_instance_color(&mut self, inst_id: InstanceId, color: Option<[u8; 3]>) -> bool {
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == inst_id) else {
            return false;
        };
        inst.color = color;
        true
    }

    /// Rotate `inst_id` to `euler_deg` while keeping its local bounding-box center fixed in world space.
    pub fn set_instance_euler_around_center(&mut self, inst_id: InstanceId, euler_deg: [f32; 3]) -> bool {
        let Some(part_id) = self.instances.iter().find(|i| i.id == inst_id).map(|i| i.part_id) else {
            return false;
        };
        let Some(bounds) = self.part(part_id).map(|p| p.mesh.bounds) else {
            return false;
        };
        let local_center = [
            (bounds[0][0] + bounds[1][0]) * 0.5,
            (bounds[0][1] + bounds[1][1]) * 0.5,
            (bounds[0][2] + bounds[1][2]) * 0.5,
        ];
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == inst_id) else {
            return false;
        };
        let center_before = transform_point(&inst.transform, local_center);
        let pos = inst.translation();
        inst.transform = transform_from_translation_euler_deg(pos, euler_deg);
        let center_after = transform_point(&inst.transform, local_center);
        inst.transform[0][3] += center_before[0] - center_after[0];
        inst.transform[1][3] += center_before[1] - center_after[1];
        inst.transform[2][3] += center_before[2] - center_after[2];
        true
    }

    /// Rotate `inst_id` so the face with `local_normal` points along `-Z` (`[0, 0, -1]`),
    /// keeping the instance's local bounding-box center fixed in world space.
    pub fn align_instance_face_to_neg_z(&mut self, inst_id: InstanceId, local_normal: [f32; 3]) -> bool {
        let Some(part_id) = self.instances.iter().find(|i| i.id == inst_id).map(|i| i.part_id) else {
            return false;
        };
        let Some(bounds) = self.part(part_id).map(|p| p.mesh.bounds) else {
            return false;
        };
        let local_center = [
            (bounds[0][0] + bounds[1][0]) * 0.5,
            (bounds[0][1] + bounds[1][1]) * 0.5,
            (bounds[0][2] + bounds[1][2]) * 0.5,
        ];
        let Some(inst) = self.instances.iter_mut().find(|i| i.id == inst_id) else {
            return false;
        };
        let m = &inst.transform;
        let wx = m[0][0] * local_normal[0] + m[0][1] * local_normal[1] + m[0][2] * local_normal[2];
        let wy = m[1][0] * local_normal[0] + m[1][1] * local_normal[1] + m[1][2] * local_normal[2];
        let wz = m[2][0] * local_normal[0] + m[2][1] * local_normal[1] + m[2][2] * local_normal[2];
        let len = (wx * wx + wy * wy + wz * wz).sqrt();
        if len < 1e-6 {
            return false;
        }
        let wn = [wx / len, wy / len, wz / len];
        let target = [0.0_f32, 0.0, -1.0];
        let c = wn[0] * target[0] + wn[1] * target[1] + wn[2] * target[2];

        let q: [[f32; 3]; 3] = if c > 0.999_999 {
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        } else if c < -0.999_999 {
            // 180 deg around X
            [[1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, -1.0]]
        } else {
            // Rodrigues rotation from wn to target: v = wn x target
            let vx = wn[1] * target[2] - wn[2] * target[1];
            let vy = wn[2] * target[0] - wn[0] * target[2];
            let vz = wn[0] * target[1] - wn[1] * target[0];
            let k = 1.0 / (1.0 + c);
            [
                [1.0 - (vy * vy + vz * vz) * k, vx * vy * k - vz, vx * vz * k + vy],
                [vx * vy * k + vz, 1.0 - (vx * vx + vz * vz) * k, vy * vz * k - vx],
                [vx * vz * k - vy, vy * vz * k + vx, 1.0 - (vx * vx + vy * vy) * k],
            ]
        };

        let center_before = transform_point(&inst.transform, local_center);
        let r_old = [
            [m[0][0], m[0][1], m[0][2]],
            [m[1][0], m[1][1], m[1][2]],
            [m[2][0], m[2][1], m[2][2]],
        ];
        let mut next = IDENTITY_TRANSFORM;
        for row in 0..3 {
            for col in 0..3 {
                next[row][col] =
                    q[row][0] * r_old[0][col] + q[row][1] * r_old[1][col] + q[row][2] * r_old[2][col];
            }
        }
        inst.transform = next;
        let center_after = transform_point(&inst.transform, local_center);
        inst.transform[0][3] += center_before[0] - center_after[0];
        inst.transform[1][3] += center_before[1] - center_after[1];
        inst.transform[2][3] += center_before[2] - center_after[2];
        true
    }

    /// Scale `inst_id`'s mesh by `factors = [sx, sy, sz]` (> 0) around its local bounding-box center.
    ///
    /// If other instances share the same `PartId`, a new `PartProto` is created for `inst_id`
    /// so un-selected siblings stay untouched. Returns the `PartId` holding the scaled mesh.
    pub fn scale_instance(&mut self, inst_id: InstanceId, factors: [f32; 3]) -> Result<PartId> {
        if factors.iter().any(|f| !f.is_finite() || *f <= 1e-6) {
            return Err(NestCraftError::InvalidInput(
                "Scale factors must be finite positive numbers".to_owned(),
            ));
        }
        let old_part_id = self
            .instances
            .iter()
            .find(|i| i.id == inst_id)
            .map(|i| i.part_id)
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown instance id {}", inst_id.0)))?;
        let proto = self
            .part(old_part_id)
            .cloned()
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown part id {}", old_part_id.0)))?;

        let b = proto.mesh.bounds;
        let lc = [
            (b[0][0] + b[1][0]) * 0.5,
            (b[0][1] + b[1][1]) * 0.5,
            (b[0][2] + b[1][2]) * 0.5,
        ];
        let scaled_positions: Vec<[f32; 3]> = proto
            .mesh
            .positions
            .iter()
            .map(|p| {
                [
                    lc[0] + (p[0] - lc[0]) * factors[0],
                    lc[1] + (p[1] - lc[1]) * factors[1],
                    lc[2] + (p[2] - lc[2]) * factors[2],
                ]
            })
            .collect();
        let scaled_mesh = Arc::new(TriMesh::from_indexed(scaled_positions, proto.mesh.indices.clone())?);
        let target_part_id = self.add_part(proto.name, scaled_mesh);
        if let Some(inst) = self.instances.iter_mut().find(|i| i.id == inst_id) {
            inst.part_id = target_part_id;
        }
        self.prune_unused_parts();
        self.clamp_instance_above_floor(inst_id);
        Ok(target_part_id)
    }

    /// Split a multi-shell instance into separate parts/instances at the same transform.
    ///
    /// Returns the newly created instance IDs, or an empty `Vec` if the mesh has <= 1 shell.
    pub fn split_instance_shells(&mut self, inst_id: InstanceId) -> Result<Vec<InstanceId>> {
        self.split_instance_shells_with_options(inst_id, false)
    }

    /// Split disconnected shells of `inst_id` into separate parts/instances, optionally keeping the source.
    pub fn split_instance_shells_with_options(
        &mut self,
        inst_id: InstanceId,
        keep_source: bool,
    ) -> Result<Vec<InstanceId>> {
        let src = self
            .instances
            .iter()
            .find(|i| i.id == inst_id)
            .cloned()
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown instance id {}", inst_id.0)))?;
        let proto = self
            .part(src.part_id)
            .cloned()
            .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown part id {}", src.part_id.0)))?;
        let shells = crate::manifold::split_shells(&proto.mesh)?;
        if shells.len() <= 1 {
            return Ok(Vec::new());
        }
        if !keep_source {
            self.remove_instance(inst_id);
        }
        let mut created = Vec::with_capacity(shells.len());
        for (idx, shell_mesh) in shells.into_iter().enumerate() {
            let name = format!("{} [Shell {}]", proto.name, idx + 1);
            let pid = self.add_part(name, Arc::new(shell_mesh));
            let new_id = self.add_instance(pid, src.transform)?;
            if let Some(new_inst) = self.instances.iter_mut().find(|i| i.id == new_id) {
                new_inst.constraint = src.constraint;
                new_inst.locked = src.locked;
                new_inst.hidden = src.hidden;
                new_inst.transparent = src.transparent;
                new_inst.color = src.color;
                new_inst.group_id = src.group_id;
            }
            created.push(new_id);
        }
        Ok(created)
    }

    /// Merge 2 or more instances into a single combined part/instance in world coordinates.
    ///
    /// Removes the source instances and returns the newly created merged instance ID.
    pub fn merge_instances(&mut self, ids: &[InstanceId]) -> Result<InstanceId> {
        self.merge_instances_with_options(ids, false)
    }

    /// Merge 2 or more instances into a single combined part/instance in world coordinates,
    /// optionally keeping the source instances.
    pub fn merge_instances_with_options(&mut self, ids: &[InstanceId], keep_sources: bool) -> Result<InstanceId> {
        let id_set: std::collections::HashSet<InstanceId> = ids.iter().copied().collect();
        let sources: Vec<PartInstance> = self
            .instances
            .iter()
            .filter(|i| id_set.contains(&i.id))
            .cloned()
            .collect();
        if sources.len() < 2 {
            return Err(NestCraftError::InvalidInput(
                "Select at least 2 parts to merge".to_owned(),
            ));
        }

        let mut positions = Vec::new();
        let mut indices = Vec::new();
        let mut first_name = None;
        for src in &sources {
            let proto = self
                .part(src.part_id)
                .ok_or_else(|| NestCraftError::InvalidInput(format!("unknown part id {}", src.part_id.0)))?;
            if first_name.is_none() {
                first_name = Some(proto.name.clone());
            }
            let base = u32::try_from(positions.len())
                .map_err(|_| NestCraftError::InvalidInput("merged mesh vertex count overflow".to_owned()))?;
            for &p in &proto.mesh.positions {
                positions.push(transform_point(&src.transform, p));
            }
            for &[a, b, c] in &proto.mesh.indices {
                indices.push([a + base, b + base, c + base]);
            }
        }

        let merged_mesh = TriMesh::from_indexed(positions, indices)?;
        let name = format!("{} (Merged)", first_name.unwrap_or_else(|| "Part".to_owned()));
        let pid = self.add_part(name, Arc::new(merged_mesh));
        let new_id = self.add_instance(pid, IDENTITY_TRANSFORM)?;

        let shared_group = sources.first().and_then(|s| s.group_id).filter(|gid| {
            sources.iter().all(|s| s.group_id == Some(*gid))
        });
        let shared_color = sources.first().and_then(|s| s.color).filter(|col| {
            sources.iter().all(|s| s.color == Some(*col))
        });
        let all_transparent = sources.iter().all(|s| s.transparent);
        if let Some(new_inst) = self.instances.iter_mut().find(|i| i.id == new_id) {
            new_inst.group_id = shared_group;
            new_inst.color = shared_color;
            new_inst.transparent = all_transparent;
        }
        if !keep_sources {
            for src in &sources {
                self.remove_instance(src.id);
            }
        }
        Ok(new_id)
    }

    /// Return `true` when `inst` is visible (neither itself nor its group is hidden).
    pub fn is_instance_visible(&self, inst: &PartInstance) -> bool {
        if inst.hidden {
            return false;
        }
        if let Some(gid) = inst.group_id
            && let Some(group) = self.groups.iter().find(|g| g.id == gid)
        {
            return !group.hidden;
        }
        true
    }
}

pub(crate) fn aabbs_overlap(a: Aabb, b: Aabb) -> bool {
    a[0][0] <= b[1][0]
        && a[1][0] >= b[0][0]
        && a[0][1] <= b[1][1]
        && a[1][1] >= b[0][1]
        && a[0][2] <= b[1][2]
        && a[1][2] >= b[0][2]
}

pub(crate) fn tri_aabb(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> Aabb {
    [
        [a[0].min(b[0]).min(c[0]), a[1].min(b[1]).min(c[1]), a[2].min(b[2]).min(c[2])],
        [a[0].max(b[0]).max(c[0]), a[1].max(b[1]).max(c[1]), a[2].max(b[2]).max(c[2])],
    ]
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub(crate) fn point_tri_dist_sq(p: [f32; 3], tri: [[f32; 3]; 3]) -> f32 {
    point_tri_closest(p, tri).1
}

pub(crate) fn point_tri_closest(p: [f32; 3], tri: [[f32; 3]; 3]) -> ([f32; 3], f32) {
    let [a, b, c] = tri;
    let ab = sub3(b, a);
    let ac = sub3(c, a);
    let ap = sub3(p, a);
    let d1 = dot3(ab, ap);
    let d2 = dot3(ac, ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return (a, dot3(ap, ap));
    }
    let bp = sub3(p, b);
    let d3 = dot3(ab, bp);
    let d4 = dot3(ac, bp);
    if d3 >= 0.0 && d4 <= d3 {
        return (b, dot3(bp, bp));
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        let proj = [a[0] + v * ab[0], a[1] + v * ab[1], a[2] + v * ab[2]];
        let d = sub3(p, proj);
        return (proj, dot3(d, d));
    }
    let cp = sub3(p, c);
    let d5 = dot3(ab, cp);
    let d6 = dot3(ac, cp);
    if d6 >= 0.0 && d5 <= d6 {
        return (c, dot3(cp, cp));
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        let proj = [a[0] + w * ac[0], a[1] + w * ac[1], a[2] + w * ac[2]];
        let d = sub3(p, proj);
        return (proj, dot3(d, d));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        let bc = sub3(c, b);
        let proj = [b[0] + w * bc[0], b[1] + w * bc[1], b[2] + w * bc[2]];
        let d = sub3(p, proj);
        return (proj, dot3(d, d));
    }
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    let proj = [
        a[0] + ab[0] * v + ac[0] * w,
        a[1] + ab[1] * v + ac[1] * w,
        a[2] + ab[2] * v + ac[2] * w,
    ];
    let d = sub3(p, proj);
    (proj, dot3(d, d))
}

fn seg_seg_closest(p1: [f32; 3], q1: [f32; 3], p2: [f32; 3], q2: [f32; 3]) -> ([f32; 3], [f32; 3], f32) {
    let d1 = sub3(q1, p1);
    let d2 = sub3(q2, p2);
    let r = sub3(p1, p2);
    let a = dot3(d1, d1);
    let e = dot3(d2, d2);
    let f = dot3(d2, r);
    let (s, t) = if a <= 1e-12 && e <= 1e-12 {
        (0.0, 0.0)
    } else if a <= 1e-12 {
        (0.0, (f / e).clamp(0.0, 1.0))
    } else {
        let c = dot3(d1, r);
        if e <= 1e-12 {
            ((-c / a).clamp(0.0, 1.0), 0.0)
        } else {
            let b = dot3(d1, d2);
            let denom = a * e - b * b;
            let mut s = if denom != 0.0 {
                ((b * f - c * e) / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let tnom = b * s + f;
            let mut t = tnom / e;
            if t < 0.0 {
                t = 0.0;
                s = (-c / a).clamp(0.0, 1.0);
            } else if t > 1.0 {
                t = 1.0;
                s = ((b - c) / a).clamp(0.0, 1.0);
            }
            (s, t)
        }
    };
    let c1 = [p1[0] + d1[0] * s, p1[1] + d1[1] * s, p1[2] + d1[2] * s];
    let c2 = [p2[0] + d2[0] * t, p2[1] + d2[1] * t, p2[2] + d2[2] * t];
    let diff = sub3(c1, c2);
    (c1, c2, dot3(diff, diff))
}

pub(crate) fn seg_intersects_tri_point(p0: [f32; 3], p1: [f32; 3], tri: [[f32; 3]; 3]) -> Option<[f32; 3]> {
    const EPS: f32 = 1e-5;
    let dir = sub3(p1, p0);
    let e1 = sub3(tri[1], tri[0]);
    let e2 = sub3(tri[2], tri[0]);
    let h = cross3(dir, e2);
    let a = dot3(e1, h);
    if a.abs() < 1e-8 {
        return None;
    }
    let f = 1.0 / a;
    let s = sub3(p0, tri[0]);
    let u = f * dot3(s, h);
    if !(-EPS..=1.0 + EPS).contains(&u) {
        return None;
    }
    let q = cross3(s, e1);
    let v = f * dot3(dir, q);
    if v < -EPS || u + v > 1.0 + EPS {
        return None;
    }
    let t = f * dot3(e2, q);
    if (-EPS..=1.0 + EPS).contains(&t) {
        let tc = t.clamp(0.0, 1.0);
        Some([p0[0] + tc * dir[0], p0[1] + tc * dir[1], p0[2] + tc * dir[2]])
    } else {
        None
    }
}

pub(crate) fn tri_tri_closest_points(t1: [[f32; 3]; 3], t2: [[f32; 3]; 3]) -> ([f32; 3], [f32; 3], f32) {
    for i in 0..3 {
        if let Some(p) = seg_intersects_tri_point(t1[i], t1[(i + 1) % 3], t2) {
            return (p, p, 0.0);
        }
        if let Some(p) = seg_intersects_tri_point(t2[i], t2[(i + 1) % 3], t1) {
            return (p, p, 0.0);
        }
    }
    let mut best_p1 = t1[0];
    let mut best_p2 = t2[0];
    let mut best_d2 = f32::INFINITY;
    for &p in &t1 {
        let (q, d2) = point_tri_closest(p, t2);
        if d2 < best_d2 {
            best_d2 = d2;
            best_p1 = p;
            best_p2 = q;
        }
    }
    for &q in &t2 {
        let (p, d2) = point_tri_closest(q, t1);
        if d2 < best_d2 {
            best_d2 = d2;
            best_p1 = p;
            best_p2 = q;
        }
    }
    for i in 0..3 {
        for j in 0..3 {
            let (c1, c2, d2) = seg_seg_closest(t1[i], t1[(i + 1) % 3], t2[j], t2[(j + 1) % 3]);
            if d2 < best_d2 {
                best_d2 = d2;
                best_p1 = c1;
                best_p2 = c2;
            }
        }
    }
    (best_p1, best_p2, best_d2)
}

pub(crate) fn tri_tri_intersect_segment(t1: [[f32; 3]; 3], t2: [[f32; 3]; 3]) -> Option<([f32; 3], [f32; 3])> {
    let mut pts = [[0.0_f32; 3]; 2];
    let mut count = 0usize;
    let mut push_pt = |p: [f32; 3]| -> Option<([f32; 3], [f32; 3])> {
        if count == 0 {
            pts[0] = p;
            count = 1;
        } else if count == 1 {
            let d = sub3(p, pts[0]);
            if dot3(d, d) > 1e-6 {
                pts[1] = p;
                count = 2;
                return Some((pts[0], pts[1]));
            }
        }
        None
    };
    for i in 0..3 {
        if let Some(p) = seg_intersects_tri_point(t1[i], t1[(i + 1) % 3], t2)
            && let Some(seg) = push_pt(p)
        {
            return Some(seg);
        }
    }
    for i in 0..3 {
        if let Some(p) = seg_intersects_tri_point(t2[i], t2[(i + 1) % 3], t1)
            && let Some(seg) = push_pt(p)
        {
            return Some(seg);
        }
    }
    None
}

pub(crate) fn tri_tri_dist_sq(t1: [[f32; 3]; 3], t2: [[f32; 3]; 3]) -> f32 {
    tri_tri_closest_points(t1, t2).2
}

pub(crate) fn point_inside_mesh_x_ray(p: [f32; 3], mesh: &TriMesh, tf: &Transform3x4) -> bool {
    let mut hits = 0usize;
    for tri in &mesh.indices {
        let a = transform_point(tf, mesh.positions[tri[0] as usize]);
        let b = transform_point(tf, mesh.positions[tri[1] as usize]);
        let c = transform_point(tf, mesh.positions[tri[2] as usize]);
        let e1 = sub3(b, a);
        let e2 = sub3(c, a);
        let h = [0.0, e2[2], -e2[1]]; // cross([1,0,0], e2)
        let det = dot3(e1, h);
        if det.abs() < 1e-8 {
            continue;
        }
        let inv = 1.0 / det;
        let s = sub3(p, a);
        let u = inv * dot3(s, h);
        if !(0.0..=1.0).contains(&u) {
            continue;
        }
        let q = cross3(s, e1);
        let v = inv * q[0];
        if v < 0.0 || u + v > 1.0 {
            continue;
        }
        let t = inv * dot3(e2, q);
        if t > 1e-6 {
            hits += 1;
        }
    }
    hits % 2 == 1
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
            hidden: false,
            transparent: false,
            color: None,
            group_id: None,
        };
        let out = inst.euler_deg();
        for k in 0..3 {
            assert!((out[k] - deg[k]).abs() < 1e-3, "axis {k}: {} vs {}", out[k], deg[k]);
        }
    }

    #[test]
    fn drop_to_floor_and_center_xy_align_bounds() {
        let cube = unit_cube();
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("cube", cube);
        let tf = transform_from_translation_euler_deg([10.0, 20.0, 35.0], [90.0, 0.0, 45.0]);
        let iid = job.add_instance(pid, tf).expect("added");

        assert!(job.drop_instance_to_floor(iid));
        assert!(job.center_instance_xy(iid));

        let inst = job.instances.iter().find(|i| i.id == iid).expect("inst");
        let wb = inst.world_bounds(job.part(pid).expect("proto").mesh.bounds);
        let [wx, wy, _] = job.machine.size_mm();
        assert!(wb[0][2].abs() < 1e-4, "bottom Z should be 0, got {}", wb[0][2]);
        assert!(((wb[0][0] + wb[1][0]) * 0.5 - wx * 0.5).abs() < 1e-3);
        assert!(((wb[0][1] + wb[1][1]) * 0.5 - wy * 0.5).abs() < 1e-3);
    }

    #[test]
    fn groups_visibility_and_custom_machine_work() {
        let cube = unit_cube();
        let mut job = BuildJob::new(MachineProfile::custom("  My SLS  ", [5.0, 250.0, 300.0]));
        assert_eq!(job.machine.name(), "My SLS");
        assert_eq!(job.machine.size_mm(), [10.0, 250.0, 300.0]);

        let pid = job.add_part("cube", cube);
        let iid = job.add_instance_centered_on_floor(pid).expect("placed");
        let gid = job.create_group("Batch A");
        assert!(job.set_instance_group(iid, Some(gid)));
        assert!(job.is_instance_visible(&job.instances[0]));

        assert!(job.set_group_hidden(gid, true));
        assert!(!job.is_instance_visible(&job.instances[0]));

        assert!(job.set_group_hidden(gid, false));
        assert!(job.set_instance_hidden(iid, true));
        assert!(!job.is_instance_visible(&job.instances[0]));

        assert!(job.set_instance_hidden(iid, false));
        assert!(job.remove_group(gid));
        assert_eq!(job.instances[0].group_id, None);
        assert!(job.is_instance_visible(&job.instances[0]));
    }

    #[test]
    fn free_floor_placement_and_z_zero_clamp_work() {
        let cube = unit_cube();
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let p1 = job.add_part("c1", cube.clone());
        let p2 = job.add_part("c2", cube);

        let i1 = job.add_instance_on_floor_next(p1).expect("i1");
        let i2 = job.add_instance_on_floor_next(p2).expect("i2");
        let wb1 = job.instances[0].world_bounds(job.part(p1).expect("p1").mesh.bounds);
        let wb2 = job.instances[1].world_bounds(job.part(p2).expect("p2").mesh.bounds);
        assert!(wb1[0][2].abs() < 1e-4);
        assert!(wb2[0][2].abs() < 1e-4);
        assert!(wb2[0][0] >= wb1[1][0] + job.clearance_mm - 1e-3);

        // Moving far outside XY bed is still valid above Z = 0.
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == i1) {
            inst.set_translation([-500.0, 900.0, 5.0]);
        }
        assert!(job.is_instance_above_floor(i1));

        // Moving below Z = 0 fails floor check and is lifted back by clamp.
        if let Some(inst) = job.instances.iter_mut().find(|i| i.id == i2) {
            inst.set_translation([0.0, 0.0, -4.0]);
        }
        assert!(!job.is_instance_above_floor(i2));
        assert!(job.clamp_instance_above_floor(i2));
        assert!(job.is_instance_above_floor(i2));
    }

    #[test]
    fn duplicate_instance_grid_places_3d_array_without_overlap() {
        let cube = unit_cube();
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("cube", cube);
        let iid = job.add_instance_centered_on_floor(pid).expect("placed");

        let created = job
            .duplicate_instance_grid(iid, [3, 2, 2], [2.0, 3.0, 4.0], None)
            .expect("grid");
        assert_eq!(created.len(), 11);
        assert_eq!(job.instances.len(), 12);
        for inst in &job.instances {
            assert!(job.is_instance_above_floor(inst.id));
        }

        // Respects max_new cap for non-rectangular total counts.
        let capped = job
            .duplicate_instance_grid(iid, [3, 3, 1], [2.0, 2.0, 2.0], Some(6))
            .expect("capped");
        assert_eq!(capped.len(), 6);
    }

    #[test]
    fn no_build_zone_detects_overlap_with_clearance() {
        let cube = unit_cube();
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("cube", cube.clone());
        let iid = job.add_instance(pid, IDENTITY_TRANSFORM).expect("placed");

        let mut zone = NoBuildZone::new("Keep-Out", cube);
        zone.position = [12.0, 0.0, 0.0];
        zone.clearance_mm = 1.0;
        job.no_build_zone = Some(zone);
        assert!(!job.is_instance_in_no_build_zone(iid));

        if let Some(z) = &mut job.no_build_zone {
            z.clearance_mm = 3.0;
        }
        assert!(job.is_instance_in_no_build_zone(iid));

        if let Some(z) = &mut job.no_build_zone {
            z.enabled = false;
        }
        assert!(!job.is_instance_in_no_build_zone(iid));
    }

    #[test]
    fn bed_origin_sets_bounds_and_center_per_machine() {
        let p110 = BuildJob::new(MachineProfile::FormigaP110);
        assert_eq!(p110.bed_origin, BedOrigin::FrontLeft);
        assert_eq!(p110.bed_xy_bounds(), ([0.0, 0.0], [200.0, 250.0]));
        assert_eq!(p110.bed_center_xy(), [100.0, 125.0]);

        let farsoon = BuildJob::new(MachineProfile::FarsoonHT403P);
        assert_eq!(farsoon.bed_origin, BedOrigin::Center);
        assert_eq!(farsoon.bed_xy_bounds(), ([-200.0, -200.0], [200.0, 200.0]));
        assert_eq!(farsoon.bed_center_xy(), [0.0, 0.0]);
    }

    #[test]
    fn no_build_zone_uses_exact_mesh_not_part_bbox() {
        // L-shaped part: occupies [0..30, 0..10] and [0..10, 0..30], so [15..30, 15..30] is empty!
        let l_positions = vec![
            [0.0, 0.0, 0.0], [30.0, 0.0, 0.0], [30.0, 10.0, 0.0], [10.0, 10.0, 0.0], [10.0, 30.0, 0.0], [0.0, 30.0, 0.0],
            [0.0, 0.0, 5.0], [30.0, 0.0, 5.0], [30.0, 10.0, 5.0], [10.0, 10.0, 5.0], [10.0, 30.0, 5.0], [0.0, 30.0, 5.0],
        ];
        let l_indices = vec![
            [0, 2, 1], [0, 3, 2], [0, 4, 3], [0, 5, 4],
            [6, 7, 8], [6, 8, 9], [6, 9, 10], [6, 10, 11],
            [0, 1, 7], [0, 7, 6], [1, 2, 8], [1, 8, 7],
            [2, 3, 9], [2, 9, 8], [3, 4, 10], [3, 10, 9],
            [4, 5, 11], [4, 11, 10], [5, 0, 6], [5, 6, 11],
        ];
        let l_mesh = Arc::new(TriMesh::from_indexed(l_positions, l_indices).expect("L mesh"));
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid = job.add_part("LPart", l_mesh);
        let iid = job.add_instance(pid, IDENTITY_TRANSFORM).expect("placed");

        // Place a 10x10x5 No-Build Zone inside the empty L pocket at [15..25, 15..25, 0..5]
        let mut zone = NoBuildZone::new("PocketZone", unit_cube());
        zone.position = [15.0, 15.0, 0.0];
        zone.clearance_mm = 1.0;
        job.no_build_zone = Some(zone);

        // Part AABB [0..30, 0..30] overlaps the zone, but actual L triangles are >= 5 mm away!
        assert!(!job.is_instance_in_no_build_zone(iid));
    }

    #[test]
    fn remove_instance_and_scale_instance_prune_unused_parts() {
        let cube_a = unit_cube();
        let cube_b = Arc::new(
            TriMesh::from_indexed(cube_a.positions.clone(), cube_a.indices.clone()).expect("valid cube"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let pid_a = job.add_part("CubeA", cube_a);
        let i1 = job.add_instance_centered_on_floor(pid_a).expect("i1");
        let i2 = job.add_instance_on_floor_next(pid_a).expect("i2");
        let pid_b = job.add_part("CubeB", cube_b);
        let i3 = job.add_instance_on_floor_next(pid_b).expect("i3");

        // Removing one of two instances sharing CubeA keeps CubeA.
        assert!(job.remove_instance(i1));
        assert!(job.part(pid_a).is_some());

        // Removing the remaining instance of CubeA prunes CubeA.
        assert!(job.remove_instance(i2));
        assert!(job.part(pid_a).is_none());

        // Scaling the sole instance of CubeB prunes the old unscaled CubeB prototype.
        let scaled_pid = job.scale_instance(i3, [1.5, 1.0, 1.0]).expect("scaled");
        assert_ne!(scaled_pid, pid_b);
        assert!(job.part(pid_b).is_none());
        assert!(job.part(scaled_pid).is_some());
    }

    #[test]
    fn add_or_reuse_part_deduplicates_name_and_shares_identical_geometry() {
        let cube_a = unit_cube();
        let cube_a_copy = Arc::new(
            TriMesh::from_indexed(cube_a.positions.clone(), cube_a.indices.clone()).expect("valid cube"),
        );
        let cube_b_copy = Arc::new(
            TriMesh::from_indexed(cube_a.positions.clone(), cube_a.indices.clone()).expect("valid cube"),
        );
        let mut job = BuildJob::new(MachineProfile::FormigaP110);
        let (p1, created1) = job.add_or_reuse_part("Bracket", cube_a);
        assert!(created1);

        // Same name + bit-identical mesh reuses the existing PartId.
        let (p1_again, created2) = job.add_or_reuse_part("Bracket", cube_a_copy);
        assert!(!created2);
        assert_eq!(p1_again, p1);
        assert_eq!(job.parts.len(), 1);

        // Different name + bit-identical mesh keeps a separate PartPrototype but shares Arc<TriMesh>.
        let (p2, created3) = job.add_or_reuse_part("Bracket_Alt", cube_b_copy);
        assert!(created3);
        assert_ne!(p2, p1);
        assert_eq!(job.parts.len(), 2);
        assert!(Arc::ptr_eq(
            &job.part(p1).expect("p1").mesh,
            &job.part(p2).expect("p2").mesh
        ));
    }
}
