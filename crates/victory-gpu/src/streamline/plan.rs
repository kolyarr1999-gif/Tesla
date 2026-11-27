//! Camera and resource state conversion, ported from readmet3xt-lab/foxhound (GPL-3.0-or-later).
use super::abi;

pub const FFX_HIGH_DYNAMIC_RANGE: u32 = 1 << 0;
pub const FFX_DISPLAY_RESOLUTION_MOTION_VECTORS: u32 = 1 << 1;
pub const FFX_MOTION_VECTORS_JITTER_CANCELLATION: u32 = 1 << 2;
pub const FFX_DEPTH_INVERTED: u32 = 1 << 3;
pub const FFX_DEPTH_INFINITE: u32 = 1 << 4;
pub const FFX_AUTO_EXPOSURE: u32 = 1 << 5;

/// A FidelityFX resource state as the host resource state it means (as the
/// PC backend reads it); `None` for generic read and indirect arguments,
/// which a texture DLSS reads cannot be in. (From DexNom's `fidelityfx.rs`.)
pub fn host_state(state: u32) -> Option<u32> {
    Some(match state {
        0x1 | 0x80 => 0,  // common, present
        0x2 => 0x8,       // unordered access
        0x4 => 0x40,      // compute read
        0x8 => 0x80,      // pixel read
        0xc => 0xc0,      // both
        0x10 => 0x800,    // copy source
        0x20 => 0x400,    // copy destination
        0x100 => 0x4,     // render target
        0x200 => 0x10,    // depth write
        _ => return None, // generic read, indirect arguments
    })
}

/// The host state DLSS is told an input is in: the one the title gave, or,
/// where that cannot be a texture's, shader resource.
pub fn input_state(ffx_state: u32) -> u32 {
    host_state(ffx_state)
        .filter(|&s| s != 0x800 && s != 0x400 && s != 0)
        .unwrap_or(0xc0)
}

/// The host state DLSS is told the output is in: the one the title gave if
/// that is unordered access, else unordered access.
pub fn output_state(ffx_state: u32) -> u32 {
    host_state(ffx_state).filter(|&s| s == 0x8).unwrap_or(0x8)
}

/// What an upscaling says of the camera, and of how to read its inputs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub jitter: [f32; 2],
    /// Multiplies the motion vectors into render pixels (FidelityFX).
    pub motion_scale: [f32; 2],
    pub render: [u32; 2],
    pub output: [u32; 2],
    pub near: f32,
    pub far: f32,
    pub fov_y: f32,
    pub reset: bool,
    /// The context's creation flags (`FFX_*` above).
    pub flags: u32,
}

impl Camera {
    pub fn depth_inverted(&self) -> bool {
        self.flags & FFX_DEPTH_INVERTED != 0
    }

    pub fn display_motion_vectors(&self) -> bool {
        self.flags & FFX_DISPLAY_RESOLUTION_MOTION_VECTORS != 0
    }

    /// The size the motion vectors cover.
    pub fn motion_size(&self) -> [u32; 2] {
        if self.display_motion_vectors() {
            self.output
        } else {
            self.render
        }
    }

    /// Streamline's motion vector scale: it multiplies by the render size
    /// itself before handing the scale to DLSS, which then gets FidelityFX's
    /// scale back.
    pub fn mvec_scale(&self, multiplier: [f32; 2]) -> [f32; 2] {
        let size = self.motion_size();
        [0, 1].map(|i| self.motion_scale[i] * multiplier[i] / size[i].max(1) as f32)
    }

    /// The common constants of a frame. The title names no camera matrices:
    /// the projection is made of the field of view, the aspect and the depth
    /// range it gives; camera motion is in the motion vectors, so the
    /// clip-to-previous-clip matrices stay the identity and the camera stands
    /// at the origin looking down +z.
    pub fn constants(
        &self,
        jitter_multiplier: [f32; 2],
        mvec_multiplier: [f32; 2],
    ) -> abi::Constants {
        let aspect = self.render[0].max(1) as f32 / self.render[1].max(1) as f32;
        let view_to_clip = projection(
            self.near,
            self.far,
            self.fov_y,
            aspect,
            self.depth_inverted(),
        );
        let clip_to_view = invert(&view_to_clip).unwrap_or(IDENTITY);
        abi::Constants {
            base: abi::BaseStructure::new(abi::CONSTANTS, 2),
            camera_view_to_clip: view_to_clip,
            clip_to_camera_view: clip_to_view,
            clip_to_lens_clip: IDENTITY,
            clip_to_prev_clip: IDENTITY,
            prev_clip_to_clip: IDENTITY,
            jitter_offset: [
                self.jitter[0] * jitter_multiplier[0],
                self.jitter[1] * jitter_multiplier[1],
            ],
            mvec_scale: self.mvec_scale(mvec_multiplier),
            camera_pinhole_offset: [0.0, 0.0],
            camera_pos: [0.0, 0.0, 0.0],
            camera_up: [0.0, 1.0, 0.0],
            camera_right: [1.0, 0.0, 0.0],
            camera_fwd: [0.0, 0.0, 1.0],
            camera_near: self.near,
            camera_far: self.far,
            camera_fov: self.fov_y,
            camera_aspect_ratio: aspect,
            motion_vectors_invalid_value: 0.0,
            depth_inverted: abi::boolean(self.depth_inverted()),
            camera_motion_included: abi::TRUE,
            motion_vectors_3d: abi::FALSE,
            reset: abi::boolean(self.reset),
            orthographic_projection: abi::FALSE,
            motion_vectors_dilated: abi::FALSE,
            motion_vectors_jittered: abi::boolean(
                self.flags & FFX_MOTION_VECTORS_JITTER_CANCELLATION != 0,
            ),
            min_relative_linear_depth_object_separation: 40.0,
        }
    }
}

pub const IDENTITY: abi::Float4x4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// A left-handed perspective projection in Streamline's layout (row vectors,
/// rows as stored): depth 0 at `near` and 1 at `far`, or reversed; a far
/// plane past 1e30 (or not finite) is at infinity. Degenerate inputs give
/// the identity.
pub fn projection(near: f32, far: f32, fov_y: f32, aspect: f32, inverted: bool) -> abi::Float4x4 {
    if !(near > 0.0 && fov_y > 0.0 && fov_y < std::f32::consts::PI && aspect > 0.0) || near.is_nan()
    {
        return IDENTITY;
    }
    let y = 1.0 / (fov_y * 0.5).tan();
    let x = y / aspect;
    let infinite = !far.is_finite() || far > 1e30 || far <= near;
    let (a, b) = match (infinite, inverted) {
        (false, false) => (far / (far - near), -near * far / (far - near)),
        (false, true) => (near / (near - far), far * near / (far - near)),
        (true, false) => (1.0, -near),
        (true, true) => (0.0, near),
    };
    [
        [x, 0.0, 0.0, 0.0],
        [0.0, y, 0.0, 0.0],
        [0.0, 0.0, a, 1.0],
        [0.0, 0.0, b, 0.0],
    ]
}

/// A 4x4 inverse by cofactors; `None` if singular.
pub fn invert(m: &abi::Float4x4) -> Option<abi::Float4x4> {
    let a: [f64; 16] = std::array::from_fn(|i| f64::from(m[i / 4][i % 4]));
    let mut inv = [0f64; 16];
    inv[0] = a[5] * a[10] * a[15] - a[5] * a[11] * a[14] - a[9] * a[6] * a[15]
        + a[9] * a[7] * a[14]
        + a[13] * a[6] * a[11]
        - a[13] * a[7] * a[10];
    inv[4] = -a[4] * a[10] * a[15] + a[4] * a[11] * a[14] + a[8] * a[6] * a[15]
        - a[8] * a[7] * a[14]
        - a[12] * a[6] * a[11]
        + a[12] * a[7] * a[10];
    inv[8] = a[4] * a[9] * a[15] - a[4] * a[11] * a[13] - a[8] * a[5] * a[15]
        + a[8] * a[7] * a[13]
        + a[12] * a[5] * a[11]
        - a[12] * a[7] * a[9];
    inv[12] = -a[4] * a[9] * a[14] + a[4] * a[10] * a[13] + a[8] * a[5] * a[14]
        - a[8] * a[6] * a[13]
        - a[12] * a[5] * a[10]
        + a[12] * a[6] * a[9];
    inv[1] = -a[1] * a[10] * a[15] + a[1] * a[11] * a[14] + a[9] * a[2] * a[15]
        - a[9] * a[3] * a[14]
        - a[13] * a[2] * a[11]
        + a[13] * a[3] * a[10];
    inv[5] = a[0] * a[10] * a[15] - a[0] * a[11] * a[14] - a[8] * a[2] * a[15]
        + a[8] * a[3] * a[14]
        + a[12] * a[2] * a[11]
        - a[12] * a[3] * a[10];
    inv[9] = -a[0] * a[9] * a[15] + a[0] * a[11] * a[13] + a[8] * a[1] * a[15]
        - a[8] * a[3] * a[13]
        - a[12] * a[1] * a[11]
        + a[12] * a[3] * a[9];
    inv[13] = a[0] * a[9] * a[14] - a[0] * a[10] * a[13] - a[8] * a[1] * a[14]
        + a[8] * a[2] * a[13]
        + a[12] * a[1] * a[10]
        - a[12] * a[2] * a[9];
    inv[2] = a[1] * a[6] * a[15] - a[1] * a[7] * a[14] - a[5] * a[2] * a[15]
        + a[5] * a[3] * a[14]
        + a[13] * a[2] * a[7]
        - a[13] * a[3] * a[6];
    inv[6] = -a[0] * a[6] * a[15] + a[0] * a[7] * a[14] + a[4] * a[2] * a[15]
        - a[4] * a[3] * a[14]
        - a[12] * a[2] * a[7]
        + a[12] * a[3] * a[6];
    inv[10] = a[0] * a[5] * a[15] - a[0] * a[7] * a[13] - a[4] * a[1] * a[15]
        + a[4] * a[3] * a[13]
        + a[12] * a[1] * a[7]
        - a[12] * a[3] * a[5];
    inv[14] = -a[0] * a[5] * a[14] + a[0] * a[6] * a[13] + a[4] * a[1] * a[14]
        - a[4] * a[2] * a[13]
        - a[12] * a[1] * a[6]
        + a[12] * a[2] * a[5];
    inv[3] = -a[1] * a[6] * a[11] + a[1] * a[7] * a[10] + a[5] * a[2] * a[11]
        - a[5] * a[3] * a[10]
        - a[9] * a[2] * a[7]
        + a[9] * a[3] * a[6];
    inv[7] = a[0] * a[6] * a[11] - a[0] * a[7] * a[10] - a[4] * a[2] * a[11]
        + a[4] * a[3] * a[10]
        + a[8] * a[2] * a[7]
        - a[8] * a[3] * a[6];
    inv[11] = -a[0] * a[5] * a[11] + a[0] * a[7] * a[9] + a[4] * a[1] * a[11]
        - a[4] * a[3] * a[9]
        - a[8] * a[1] * a[7]
        + a[8] * a[3] * a[5];
    inv[15] = a[0] * a[5] * a[10] - a[0] * a[6] * a[9] - a[4] * a[1] * a[10]
        + a[4] * a[2] * a[9]
        + a[8] * a[1] * a[6]
        - a[8] * a[2] * a[5];
    let det = a[0] * inv[0] + a[1] * inv[4] + a[2] * inv[8] + a[3] * inv[12];
    if det.abs() < 1e-30 || !det.is_finite() {
        return None;
    }
    Some(std::array::from_fn(|r| {
        std::array::from_fn(|c| (inv[r * 4 + c] / det) as f32)
    }))
}
