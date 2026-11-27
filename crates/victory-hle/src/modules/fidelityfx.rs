//! FidelityFX dispatch interposition adapted from readmet3xt-lab/foxhound (GPL-3.0-or-later).
//! The description layouts are the FidelityFX SDK's API (MIT) as DexNom's fork
//! reads them (DexNom/VIctory, `crates/victory-hle/src/modules/fidelityfx.rs`,
//! GPL-3.0-or-later), from which they are ported.
#![allow(non_snake_case)]
// Title code calls these through its import table.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use victory_gpu::object::Object;
use victory_gpu::streamline::{self, Upscale, plan::Camera};
use victory_loader::{ImportResolver, Provenance, Resolution};
use victory_pe::ImportSymbol;

/// The DLL the title imports the API from (it loads the upscaler itself).
pub const LOADER_DLL: &str = "amd_fidelityfx_loader_xbox.dll";

// Return codes.
const OK: u32 = 0;
const PARAMETER: u32 = 6;

// Description types.
const CREATE_UPSCALE: u64 = 0x1_0000;
const DISPATCH_UPSCALE: u64 = 0x1_0001;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Header {
    kind: u64,
    next: *mut Header,
}

/// An upscaler context: flags, largest render and output sizes, a message
/// callback.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CreateUpscale {
    header: Header,
    flags: u32,
    max_render: [u32; 2],
    max_output: [u32; 2],
    message: usize,
}

/// A resource, its description (type, format, sizes, mips, flags, usage)
/// and the state it is in.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Resource {
    resource: *mut c_void,
    description: [u32; 8],
    state: u32,
}

/// One upscaling: the command list, then colour, depth, motion vectors,
/// exposure, reactive mask, transparency mask and output; jitter,
/// motion-vector scale, sizes, sharpening, frame time (ms), pre-exposure,
/// reset, camera and flags. (The C `bool`s are read as bytes.)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct DispatchUpscale {
    header: Header,
    list: *mut c_void,
    resources: [Resource; 7],
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    render_size: [u32; 2],
    upscale_size: [u32; 2],
    enable_sharpening: u8,
    sharpness: f32,
    frame_time_delta: f32,
    pre_exposure: f32,
    reset: u8,
    camera_near: f32,
    camera_far: f32,
    camera_fov_angle_vertical: f32,
    view_space_to_meters_factor: f32,
    flags: u32,
}

const _: () = assert!(size_of::<CreateUpscale>() == 0x30);
const _: () = assert!(size_of::<Resource>() == 0x30);
const _: () = assert!(size_of::<DispatchUpscale>() == 0x1b0);

type Create = unsafe extern "C" fn(*mut *mut c_void, *mut Header, *const c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut *mut c_void, *const c_void) -> u32;
/// Configure, query and dispatch.
type Call = unsafe extern "C" fn(*mut *mut c_void, *mut Header) -> u32;

/// The title's own functions, by [`NAMES`].
static ORIGINALS: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
const NAMES: [&str; 3] = ["ffxCreateContext", "ffxDestroyContext", "ffxDispatch"];

/// An upscaler context the title made: its creation flags and largest
/// sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Created {
    flags: u32,
    max_render: [u32; 2],
    max_output: [u32; 2],
}

static CONTEXTS: Mutex<Option<HashMap<usize, Created>>> = Mutex::new(None);

macro_rules! once {
    ($level:ident, $($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::$level!($($arg)*));
    }};
}

/// Binds the title's imports of [`LOADER_DLL`] to the functions here when
/// DLSS is on, keeping what `inner` binds them to (the
/// title's own DLL) for them to call; otherwise binds as `inner` does.
pub struct Interpose<'a> {
    pub inner: &'a dyn ImportResolver,
}

impl ImportResolver for Interpose<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let found = self.inner.resolve(dll, symbol);
        if !dll.eq_ignore_ascii_case(LOADER_DLL) || !streamline::serving() {
            return found;
        }
        match (&found, symbol) {
            (&Resolution::Found { address, .. }, ImportSymbol::Name { name, .. }) => {
                match interpose(name, address) {
                    Some(ours) => Resolution::Found {
                        address: ours,
                        provenance: Provenance::Hle,
                    },
                    None => found,
                }
            }
            _ => found,
        }
    }
}

/// Keeps `original` as the title's `name` and gives ours, if we have one.
fn interpose(name: &str, original: usize) -> Option<usize> {
    let i = NAMES.iter().position(|n| *n == name)?;
    ORIGINALS[i].store(original, Relaxed);
    let ours = [
        ffxCreateContext as *const () as usize,
        ffxDestroyContext as *const () as usize,
        ffxDispatch as *const () as usize,
    ][i];
    tracing::info!(
        name,
        original = format_args!("{original:#x}"),
        "FidelityFX: the title's import goes through ours (DLSS Quality)"
    );
    Some(ours)
}

fn original(i: usize) -> Option<*const ()> {
    let address = ORIGINALS[i].load(Relaxed);
    (address != 0).then_some(address as *const ())
}

fn contexts<T>(f: impl FnOnce(&mut HashMap<usize, Created>) -> T) -> T {
    let mut contexts = CONTEXTS.lock().unwrap_or_else(|e| e.into_inner());
    f(contexts.get_or_insert_with(HashMap::new))
}

extern "C" fn ffxCreateContext(
    context: *mut *mut c_void,
    desc: *mut Header,
    memory: *const c_void,
) -> u32 {
    let Some(create) = original(0) else {
        return PARAMETER;
    };
    // SAFETY: the title's own function, with its arguments.
    let call = |desc: *mut Header| unsafe {
        std::mem::transmute::<*const (), Create>(create)(context, desc, memory)
    };
    // SAFETY: the title passes a chain of descriptions.
    let upscaler = !desc.is_null() && unsafe { (*desc).kind } == CREATE_UPSCALE;
    if !upscaler {
        return call(desc);
    }
    // SAFETY: an upscaler description, by its type.
    let asked = unsafe { desc.cast::<CreateUpscale>().read() };
    let code = call(desc);
    if code != OK || context.is_null() {
        return code;
    }
    // SAFETY: the context was made.
    let handle = unsafe { *context } as usize;
    let created = Created {
        flags: asked.flags,
        max_render: asked.max_render,
        max_output: asked.max_output,
    };
    tracing::info!(
        context = format_args!("{handle:#x}"),
        flags = format_args!("{:#x}", created.flags),
        max_render = ?created.max_render,
        max_output = ?created.max_output,
        "FidelityFX: the title made its upscaler"
    );
    contexts(|c| c.insert(handle, created));
    code
}

extern "C" fn ffxDestroyContext(context: *mut *mut c_void, memory: *const c_void) -> u32 {
    let Some(destroy) = original(1) else {
        return PARAMETER;
    };
    if !context.is_null() {
        // SAFETY: the title passes its context.
        let handle = unsafe { *context } as usize;
        if contexts(|c| c.remove(&handle)).is_some() {
            streamline::upscaler_destroyed();
        }
    }
    // SAFETY: the title's own function, with its arguments.
    unsafe { std::mem::transmute::<*const (), Destroy>(destroy)(context, memory) }
}

extern "C" fn ffxDispatch(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    let Some(dispatch) = original(2) else {
        return PARAMETER;
    };
    // SAFETY: the title's own function, with its arguments.
    let own = || unsafe { std::mem::transmute::<*const (), Call>(dispatch)(context, desc) };
    if context.is_null() || desc.is_null() {
        return own();
    }
    // SAFETY: the title passes a description and its context.
    let (kind, handle) = unsafe { ((*desc).kind, *context as usize) };
    if kind != DISPATCH_UPSCALE {
        return own();
    }
    let Some(created) = contexts(|c| c.get(&handle).copied()) else {
        once!(
            warn,
            "FidelityFX: an upscale dispatch on a context we did not see made; the title's own runs"
        );
        return own();
    };
    // SAFETY: an upscaling, by its type.
    let d = unsafe { desc.cast::<DispatchUpscale>().read() };
    first_dispatch(&d, &created);
    match streamline::upscale(&upscale(&d, &created)) {
        Ok(()) => OK,
        Err(why) => {
            refused(why);
            own()
        }
    }
}

/// Each reason the title's own upscaling ran instead, once.
fn refused(why: &'static str) {
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&why) {
        seen.push(why);
        tracing::warn!("FidelityFX: the title's own upscaling runs: {why}");
    }
}

/// The first upscale dispatch, all of it.
fn first_dispatch(d: &DispatchUpscale, created: &Created) {
    once!(
        info,
        render = ?d.render_size,
        output = ?d.upscale_size,
        max_render = ?created.max_render,
        max_output = ?created.max_output,
        create_flags = format_args!("{:#x}", created.flags),
        flags = format_args!("{:#x}", d.flags),
        jitter = ?d.jitter,
        motion_vector_scale = ?d.motion_scale,
        near = d.camera_near,
        far = d.camera_far,
        fov = d.camera_fov_angle_vertical,
        frame_ms = d.frame_time_delta,
        pre_exposure = d.pre_exposure,
        reset = d.reset != 0,
        sharpening = d.enable_sharpening != 0,
        resources = ?d.resources.iter().map(|r| (r.resource as usize != 0, r.description[1], r.description[2], r.description[3], r.state)).collect::<Vec<_>>(),
        "FidelityFX: the title's first upscale dispatch (resources: present, format, width, height, state)"
    );
}

/// An upscaling in `victory_gpu::streamline`'s terms. The output size is the
/// context's largest when the dispatch names none (as FidelityFX reads it).
fn upscale(d: &DispatchUpscale, created: &Created) -> Upscale {
    let output = if d.upscale_size == [0, 0] {
        created.max_output
    } else {
        d.upscale_size
    };
    Upscale {
        list: d.list.cast::<Object>().cast_const(),
        resources: d
            .resources
            .map(|r| (r.resource.cast::<Object>().cast_const(), r.state)),
        camera: Camera {
            jitter: d.jitter,
            motion_scale: d.motion_scale,
            render: d.render_size,
            output,
            near: d.camera_near,
            far: d.camera_far,
            fov_y: d.camera_fov_angle_vertical,
            reset: d.reset != 0,
            flags: created.flags,
        },
        pre_exposure: d.pre_exposure,
    }
}
