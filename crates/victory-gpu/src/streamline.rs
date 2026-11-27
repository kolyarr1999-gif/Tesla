//! DLSS Super Resolution on the title's Series S FidelityFX dispatch.
//! Ported from readmet3xt-lab/foxhound (GPL-3.0-or-later).

pub(crate) mod abi;
pub mod plan;

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use crate::object::Object;

const INTERPOSER: &str = "sl.interposer.dll";
const PROJECT_ID: &CStr = c"6f0d7b2e-3a41-4c1e-9b57-2d8e4f1a9c30";
const ENGINE_VERSION: &CStr = c"VIctory 0.1";
const VIEWPORT_DLSS: u32 = 1;

/// One upscaling the title asked for: a FidelityFX `ffxDispatch` of an
/// upscale, in its own terms (`victory_hle::modules::fidelityfx` reads it).
#[derive(Clone, Copy, Debug)]
pub struct Upscale {
    /// The title's command list.
    pub list: *const Object,
    /// Colour, depth, motion vectors, exposure, reactive mask, transparency
    /// and composition mask, output: the title's resource (null if none) and
    /// the FidelityFX state it is in.
    pub resources: [(*const Object, u32); 7],
    pub camera: plan::Camera,
    pub pre_exposure: f32,
}

pub const COLOUR: usize = 0;
pub const DEPTH: usize = 1;
pub const MOTION: usize = 2;
pub const EXPOSURE: usize = 3;
pub const REACTIVE: usize = 4;
pub const OUTPUT: usize = 6;

struct Library(windows_sys::Win32::Foundation::HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: this owns the reference LoadLibraryExW took.
        unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.0) };
    }
}

// SAFETY: a module handle.
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}

/// The library's exported functions.
struct Sl {
    _library: Library,
    init: abi::SlInit,
    is_feature_supported: abi::SlIsFeatureSupported,
    evaluate_feature: abi::SlEvaluateFeature,
    free_resources: abi::SlFreeResources,
    get_feature_requirements: abi::SlGetFeatureRequirements,
    get_feature_version: abi::SlGetFeatureVersion,
    set_constants: abi::SlSetConstants,
    get_feature_function: abi::SlGetFeatureFunction,
    get_new_frame_token: abi::SlGetNewFrameToken,
    set_d3d_device: abi::SlSetD3DDevice,
}

static SL: OnceLock<Sl> = OnceLock::new();

struct Functions {
    dlss_optimal: Option<abi::SlDlssGetOptimalSettings>,
    dlss_set_options: Option<abi::SlDlssSetOptions>,
}
static FUNCTIONS: OnceLock<Functions> = OnceLock::new();
static SUPPORTED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static EVALUATED_ALL: AtomicU64 = AtomicU64::new(0);

struct QualitySizes {
    output: [u32; 2],
    optimal: [u32; 2],
    min: [u32; 2],
    max: [u32; 2],
}

struct Calls {
    settings: Option<QualitySizes>,
    last: Option<(u32, [u32; 2], [u32; 2])>,
}
static CALLS: Mutex<Calls> = Mutex::new(Calls {
    settings: None,
    last: None,
});

fn calls() -> std::sync::MutexGuard<'static, Calls> {
    CALLS.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn init() {
    if !std::env::var_os("VICTORY_DLSS").is_some_and(|value| value == "1") {
        return;
    }
    if std::env::var_os("VICTORY_THIN").is_some_and(|value| value == "1") {
        tracing::warn!("DLSS Quality requires immediate list recording");
        return;
    }
    let Some(dir) = std::env::var_os("VICTORY_STREAMLINE_DIR") else {
        tracing::warn!("DLSS Quality requires VICTORY_STREAMLINE_DIR");
        return;
    };
    if let Err(why) = load_and_init(Path::new(&dir)) {
        tracing::warn!("DLSS Quality unavailable: {why}; using the title upscaler");
    }
}

fn load_and_init(dir: &Path) -> Result<(), String> {
    let dir = std::path::absolute(dir).map_err(|e| e.to_string())?;
    let path = dir.join(INTERPOSER);
    for file in [INTERPOSER, "sl.common.dll", "sl.dlss.dll", "nvngx_dlss.dll"] {
        let file = dir.join(file);
        if !file.is_file() {
            return Err(format!("required library {} is missing", file.display()));
        }
        verify_signature(&file)?;
    }
    let sl = load(&path)?;
    let sl = SL.get_or_init(|| sl);
    let dir_wide = wide(&dir);
    let paths = [dir_wide.as_ptr()];
    let preferences = abi::Preferences {
        base: abi::BaseStructure::new(abi::PREFERENCES, 1),
        show_console: false,
        log_level: abi::LOG_DEFAULT,
        paths_to_plugins: paths.as_ptr(),
        num_paths_to_plugins: 1,
        path_to_logs_and_data: std::ptr::null(),
        allocate_callback: std::ptr::null(),
        release_callback: std::ptr::null(),
        log_message_callback: Some(log_message),
        flags: abi::DISABLE_CL_STATE_TRACKING
            | abi::DISABLE_DEBUG_TEXT
            | abi::USE_MANUAL_HOOKING
            | abi::USE_FRAME_BASED_RESOURCE_TAGGING,
        features_to_load: &abi::FEATURE_DLSS,
        num_features_to_load: 1,
        application_id: 0,
        engine: abi::ENGINE_CUSTOM,
        engine_version: ENGINE_VERSION.as_ptr(),
        project_id: PROJECT_ID.as_ptr(),
        render_api: abi::RENDER_API_D3D12,
    };
    // SAFETY: the SDK copies the preferences it keeps.
    let code = unsafe { (sl.init)(&preferences, abi::SDK_VERSION) };
    if code != 0 {
        return Err(format!("slInit: {} ({code})", abi::result_name(code)));
    }
    let (adapter, luid) = default_adapter().ok_or("no DXGI adapter")?;
    let info = abi::AdapterInfo {
        base: abi::BaseStructure::new(abi::ADAPTER_INFO, 1),
        device_luid: std::ptr::from_ref(&luid).cast(),
        device_luid_size: size_of_val(&luid) as u32,
        vk_physical_device: std::ptr::null_mut(),
    };
    // SAFETY: the adapter description lives through the call.
    let code = unsafe { (sl.is_feature_supported)(abi::FEATURE_DLSS, &info) };
    let supported = if code == 0 {
        Ok(())
    } else {
        Err(format!("{} ({code})", abi::result_name(code)))
    };
    if report("DLSS Super Resolution", abi::FEATURE_DLSS, supported, sl) {
        SUPPORTED.store(true, Release);
        tracing::info!(%adapter, mode = "Quality", "Streamline loaded");
    }
    Ok(())
}

pub fn serving() -> bool {
    SUPPORTED.load(Acquire)
}

pub(crate) fn device_made(device: &ID3D12Device) {
    if !serving() {
        return;
    }
    let Some(sl) = SL.get() else { return };
    // SAFETY: the native D3D12 device.
    let code = unsafe { (sl.set_d3d_device)(device.as_raw()) };
    if code != 0 {
        tracing::warn!(code, "Streamline: slSetD3DDevice failed");
        return;
    }
    let function = |name: &CStr| -> Option<*mut c_void> {
        let mut address = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name and an out pointer.
        let code =
            unsafe { (sl.get_feature_function)(abi::FEATURE_DLSS, name.as_ptr(), &mut address) };
        if code != 0 || address.is_null() {
            tracing::warn!(code, name = %name.to_string_lossy(), "Streamline function unavailable");
            return None;
        }
        Some(address)
    };
    let functions = Functions {
        // SAFETY: the named functions have the SDK signatures.
        dlss_optimal: function(c"slDLSSGetOptimalSettings").map(|f| unsafe {
            std::mem::transmute::<*mut c_void, abi::SlDlssGetOptimalSettings>(f)
        }),
        dlss_set_options: function(c"slDLSSSetOptions")
            .map(|f| unsafe { std::mem::transmute::<*mut c_void, abi::SlDlssSetOptions>(f) }),
    };
    if functions.dlss_optimal.is_some() && functions.dlss_set_options.is_some() {
        let _ = FUNCTIONS.set(functions);
        log_version(sl, abi::FEATURE_DLSS, "DLSS Super Resolution");
    }
}

pub fn upscale(up: &Upscale) -> Result<(), &'static str> {
    if FUNCTIONS.get().is_none() {
        return Err("Streamline device unavailable");
    }
    if [COLOUR, DEPTH, MOTION, OUTPUT]
        .iter()
        .any(|&i| up.resources[i].0.is_null())
    {
        return Err("missing required texture");
    }
    if up.camera.render.contains(&0) || up.camera.output.contains(&0) {
        return Err("empty render or output size");
    }
    validate(up)?;
    let id = NEXT_ID.fetch_add(1, Relaxed) + 1;
    crate::list::upscale(up.list, |host| evaluate(host, up, id))
        .then_some(())
        .ok_or("DLSS could not record its upscale")
}

/// Logs whether a feature is supported, with its requirements; whether it is.
fn report(name: &str, feature: u32, supported: Result<(), String>, sl: &Sl) -> bool {
    let mut requirements: abi::FeatureRequirements = unsafe_zeroed_requirements();
    // SAFETY: a structure to fill.
    let asked = unsafe { (sl.get_feature_requirements)(feature, &mut requirements) };
    let needs = (asked == 0).then(|| {
        format!(
            "OS {} (needs {}), driver {} (needs {}){}{}",
            requirements.os_version_detected,
            requirements.os_version_required,
            requirements.driver_version_detected,
            requirements.driver_version_required,
            if requirements.flags & abi::REQUIRES_HARDWARE_SCHEDULING != 0 {
                ", hardware GPU scheduling"
            } else {
                ""
            },
            if requirements.flags & abi::REQUIRES_VSYNC_OFF != 0 {
                ", v-sync off"
            } else {
                ""
            },
        )
    });
    match supported {
        Ok(()) => {
            tracing::info!(
                requirements = needs.as_deref().unwrap_or("?"),
                "Streamline: {name} is supported on this GPU"
            );
            true
        }
        Err(why) => {
            tracing::warn!(
                requirements = needs.as_deref().unwrap_or("?"),
                "Streamline: {name} is not supported here: {why}"
            );
            false
        }
    }
}

fn unsafe_zeroed_requirements() -> abi::FeatureRequirements {
    // SAFETY: plain data; pointers null, counts zero.
    let mut r: abi::FeatureRequirements = unsafe { std::mem::zeroed() };
    r.base = abi::BaseStructure::new(abi::FEATURE_REQUIREMENTS, 2);
    r
}

unsafe extern "C" fn log_message(kind: u32, message: *const c_char) {
    if message.is_null() {
        return;
    }
    // SAFETY: a NUL-terminated message for the call.
    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    let text = text.trim_end();
    // (Every plugin in the folder that was not asked for says so.)
    let noise = text.contains("since it is was not requested by the host");
    match kind {
        abi::LOG_ERROR => tracing::error!(target: "streamline", "{text}"),
        abi::LOG_WARN if !noise => tracing::warn!(target: "streamline", "{text}"),
        _ => tracing::debug!(target: "streamline", "{text}"),
    }
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// The adapter a device made with no adapter named is on (the first), its
/// name and LUID.
fn default_adapter() -> Option<(String, windows::Win32::Foundation::LUID)> {
    // SAFETY: plain DXGI queries.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        let len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        Some((
            String::from_utf16_lossy(&desc.Description[..len]),
            desc.AdapterLuid,
        ))
    }
}

/// The Authenticode signature of `path` (NVIDIA signs its DLLs), checked
/// without going online.
fn verify_signature(path: &Path) -> Result<(), String> {
    use windows_sys::Win32::Security::WinTrust::*;
    let file = wide(path);
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: file.as_ptr(),
        hFile: std::ptr::null_mut(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: filled structures that live through both calls.
    let code = unsafe { WinVerifyTrust(std::ptr::null_mut(), &mut action, (&raw mut data).cast()) };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: as above; closes the state the first call opened.
    unsafe { WinVerifyTrust(std::ptr::null_mut(), &mut action, (&raw mut data).cast()) };
    if code == 0 {
        Ok(())
    } else {
        Err(format!(
            "{} is not validly signed ({:#x})",
            path.display(),
            code as u32
        ))
    }
}

fn load(path: &Path) -> Result<Sl, String> {
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
        LoadLibraryExW,
    };
    let file = wide(path);
    // Its own folder and System32 only, never the current folder.
    // SAFETY: a NUL-terminated full path.
    let module = unsafe {
        LoadLibraryExW(
            file.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err(format!(
            "{INTERPOSER} did not load ({})",
            std::io::Error::last_os_error()
        ));
    }
    let library = Library(module);
    let find = |name: &CStr| {
        // SAFETY: a loaded module and a NUL-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .map(|f| f as *const c_void)
            .ok_or_else(|| format!("{INTERPOSER} has no {}", name.to_string_lossy()))
    };
    macro_rules! get {
        ($name:literal, $t:ty) => {
            // SAFETY: the export of that name has the header's signature.
            unsafe { std::mem::transmute::<*const c_void, $t>(find($name)?) }
        };
    }
    Ok(Sl {
        init: get!(c"slInit", abi::SlInit),
        is_feature_supported: get!(c"slIsFeatureSupported", abi::SlIsFeatureSupported),
        evaluate_feature: get!(c"slEvaluateFeature", abi::SlEvaluateFeature),
        free_resources: get!(c"slFreeResources", abi::SlFreeResources),
        get_feature_requirements: get!(c"slGetFeatureRequirements", abi::SlGetFeatureRequirements),
        get_feature_version: get!(c"slGetFeatureVersion", abi::SlGetFeatureVersion),
        set_constants: get!(c"slSetConstants", abi::SlSetConstants),
        get_feature_function: get!(c"slGetFeatureFunction", abi::SlGetFeatureFunction),
        get_new_frame_token: get!(c"slGetNewFrameToken", abi::SlGetNewFrameToken),
        set_d3d_device: get!(c"slSetD3DDevice", abi::SlSetD3DDevice),
        _library: library,
    })
}

fn log_version(sl: &Sl, feature: u32, name: &str) {
    let mut version = abi::FeatureVersion {
        base: abi::BaseStructure::new(abi::FEATURE_VERSION, 1),
        version_sl: abi::Version::default(),
        version_ngx: abi::Version::default(),
    };
    // SAFETY: a structure to fill.
    let code = unsafe { (sl.get_feature_version)(feature, &mut version) };
    if code == 0 {
        tracing::info!(streamline = %version.version_sl, ngx = %version.version_ngx, "Streamline: {name} loaded");
    } else {
        tracing::warn!(
            "Streamline: {name} has no version: {} ({code})",
            abi::result_name(code)
        );
    }
}

fn frame_token(sl: &Sl, index: u32) -> Option<*mut abi::FrameToken> {
    let mut token = std::ptr::null_mut();
    // SAFETY: an out pointer and an index.
    let code = unsafe { (sl.get_new_frame_token)(&mut token, &index) };
    (code == 0 && !token.is_null()).then_some(token)
}

fn validate(up: &Upscale) -> Result<(), &'static str> {
    for (i, &(resource, state)) in up.resources.iter().enumerate() {
        if resource.is_null() || i == 5 {
            continue;
        }
        if !crate::resource::is_resource(resource) {
            return Err("a texture of the upscaling is no resource of ours");
        }
        let Some(copy) = crate::resource::host_copy(resource) else {
            return Err("a texture of the upscaling has no host copy (a buffer?)");
        };
        let wanted = if i == OUTPUT {
            plan::output_state(state)
        } else {
            plan::input_state(state)
        };
        if crate::host::legal_state(&copy.texture, wanted) != wanted {
            return Err(if i == OUTPUT {
                "the output cannot be written by a shader (no unordered access)"
            } else {
                "an input cannot be read by a shader"
            });
        }
    }
    Ok(())
}

fn evaluate(host: &crate::host::HostList, up: &Upscale, id: u64) -> bool {
    let camera = up.camera;
    let fail = |why: &str| {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Relaxed) {
            tracing::warn!(
                id,
                "DLSS: the upscaling was not recorded: {why}; using the title upscaler"
            );
        }
        false
    };
    // The inputs as a compute shader's would be (aliased views, uploads of
    // title-written textures, depth twins), then in the states DLSS is told.
    let used = |i: usize| {
        !up.resources[i].0.is_null()
            && (i != EXPOSURE || camera.flags & plan::FFX_AUTO_EXPOSURE == 0)
    };
    let mut textures: Textures = Default::default();
    for i in [COLOUR, DEPTH, MOTION, EXPOSURE, REACTIVE, OUTPUT] {
        if !used(i) {
            continue;
        }
        let (resource, state) = up.resources[i];
        if i != OUTPUT && crate::external::prepare_input(host, resource).is_none() {
            return fail("an input could not be prepared");
        }
        let wanted = if i == OUTPUT {
            plan::output_state(state)
        } else {
            plan::input_state(state)
        };
        let Some((texture, legal)) = crate::external::host_texture(host, resource, wanted) else {
            return fail("a texture has no host copy");
        };
        if legal != wanted {
            return fail("a texture cannot be in the state DLSS needs");
        }
        textures[i] = Some((texture, legal));
    }
    let recorded = match record(host.list.as_raw(), &textures, &camera, up.pre_exposure, id) {
        Ok(recorded) => recorded,
        Err(why) => return fail(&why),
    };
    crate::external::output_written(up.resources[OUTPUT].0);
    let all = EVALUATED_ALL.fetch_add(1, Relaxed) + 1;
    if all == 1 || recorded.changed {
        tracing::info!(
            render = ?camera.render,
            output = ?camera.output,
            mode = "Quality",
            optimal = ?recorded.optimal,
            hdr = camera.flags & plan::FFX_HIGH_DYNAMIC_RANGE != 0,
            depth_inverted = camera.depth_inverted(),
            auto_exposure = textures[EXPOSURE].is_none(),
            reactive = textures[REACTIVE].is_some(),
            display_motion_vectors = camera.display_motion_vectors(),
            jitter = ?recorded.jitter,
            mvec_scale = ?recorded.mvec_scale,
            near = camera.near,
            far = camera.far,
            fov = camera.fov_y,
            formats = ?formats(&textures),
            evaluations = all,
            "DLSS: upscaling recorded in place of the title's FidelityFX"
        );
    }
    true
}

/// Colour, depth, motion vectors, exposure, reactive mask, (transparency),
/// output: the host textures DLSS is given and the states they are in.
type Textures = [Option<(ID3D12Resource, u32)>; 7];

fn formats(textures: &Textures) -> Vec<i32> {
    textures
        .iter()
        .flatten()
        .map(|(t, _)| {
            // SAFETY: a live texture.
            unsafe { t.GetDesc() }.Format.0
        })
        .collect()
}

/// What [`record`] recorded.
struct Recorded {
    optimal: Option<[u32; 2]>,
    /// Another mode or size than the last time.
    changed: bool,
    jitter: [f32; 2],
    mvec_scale: [f32; 2],
}

/// The DLSS evaluation of `textures` recorded into the open command list
/// `list`: the mode whose render size holds the title's, the options, the
/// frame's constants (frame `id` of the upscaling's viewport), the textures
/// as local tags. The textures are left in the states given.
fn record(
    list: *mut c_void,
    textures: &Textures,
    camera: &plan::Camera,
    pre_exposure: f32,
    id: u64,
) -> Result<Recorded, String> {
    let (Some(sl), Some(functions)) = (SL.get(), FUNCTIONS.get()) else {
        return Err("Streamline is not ready".into());
    };
    let (Some(optimal_of), Some(set_options)) =
        (functions.dlss_optimal, functions.dlss_set_options)
    else {
        return Err("no DLSS functions".into());
    };
    let mut calls = calls();
    let output = camera.output;
    let mode = abi::DLSS_MAX_QUALITY;
    let mut options = abi::DlssOptions::new(mode, output, 0);
    if calls
        .settings
        .as_ref()
        .is_none_or(|sizes| sizes.output != output)
    {
        let mut settings = abi::DlssOptimalSettings::default();
        // SAFETY: filled options and a structure to fill.
        let code = unsafe { optimal_of(&options, &mut settings) };
        if code != 0 {
            return Err(format!(
                "slDLSSGetOptimalSettings: {}",
                abi::result_name(code)
            ));
        }
        calls.settings = Some(QualitySizes {
            output,
            optimal: [
                settings.optimal_render_width,
                settings.optimal_render_height,
            ],
            min: [settings.render_width_min, settings.render_height_min],
            max: [settings.render_width_max, settings.render_height_max],
        });
    }
    let &QualitySizes {
        optimal, min, max, ..
    } = calls.settings.as_ref().expect("settings were queried");
    if (0..2).any(|i| camera.render[i] < min[i] || camera.render[i] > max[i])
        && camera.render != optimal
    {
        return Err(format!(
            "render size {:?} outside the DLSS Quality range {min:?}–{max:?}",
            camera.render
        ));
    }
    options.pre_exposure = if pre_exposure > 0.0 {
        pre_exposure
    } else {
        1.0
    };
    options.color_buffers_hdr = abi::boolean(camera.flags & plan::FFX_HIGH_DYNAMIC_RANGE != 0);
    options.use_auto_exposure = abi::boolean(textures[EXPOSURE].is_none());
    let viewport = abi::ViewportHandle::new(VIEWPORT_DLSS);
    // SAFETY: filled options for our viewport.
    let code = unsafe { set_options(&viewport, &options) };
    if code != 0 {
        return Err(format!(
            "slDLSSSetOptions: {} ({code})",
            abi::result_name(code)
        ));
    }
    let changed = calls.last != Some((mode, camera.render, output));
    calls.last = Some((mode, camera.render, output));
    let token = frame_token(sl, id as u32).ok_or("no frame token")?;
    let constants = camera.constants([1.0; 2], [1.0; 2]);
    // SAFETY: constants for this frame and viewport.
    let code = unsafe { (sl.set_constants)(&constants, token, &viewport) };
    if code != 0 {
        return Err(format!(
            "slSetConstants: {} ({code})",
            abi::result_name(code)
        ));
    }
    let kinds = [
        (COLOUR, abi::BUFFER_SCALING_INPUT_COLOR, camera.render),
        (DEPTH, abi::BUFFER_DEPTH, camera.render),
        (MOTION, abi::BUFFER_MOTION_VECTORS, camera.motion_size()),
        (EXPOSURE, abi::BUFFER_EXPOSURE, [1, 1]),
        (REACTIVE, abi::BUFFER_BIAS_CURRENT_COLOR_HINT, camera.render),
        (OUTPUT, abi::BUFFER_SCALING_OUTPUT_COLOR, output),
    ];
    let mut resources: Vec<(abi::Resource, u32, [u32; 2])> = kinds
        .iter()
        .filter_map(|&(i, kind, size)| {
            textures[i]
                .as_ref()
                .map(|(t, state)| (abi::Resource::texture(t.as_raw(), *state), kind, size))
        })
        .collect();
    let mut tags: Vec<abi::ResourceTag> = resources
        .iter_mut()
        .map(|(r, kind, size)| {
            abi::ResourceTag::new(
                r,
                *kind,
                abi::VALID_UNTIL_EVALUATE,
                abi::Extent::sized(*size),
            )
        })
        .collect();
    let mut inputs: Vec<*const abi::BaseStructure> = vec![std::ptr::from_ref(&viewport.base)];
    inputs.extend(tags.iter_mut().map(|t| std::ptr::from_ref(&t.base)));
    // SAFETY: an open command list; inputs that live through the call.
    let code = unsafe {
        (sl.evaluate_feature)(
            abi::FEATURE_DLSS,
            token,
            inputs.as_ptr(),
            inputs.len() as u32,
            list,
        )
    };
    if code != 0 {
        return Err(format!(
            "slEvaluateFeature: {} ({code})",
            abi::result_name(code)
        ));
    }
    Ok(Recorded {
        optimal: Some(optimal),
        changed,
        jitter: constants.jitter_offset,
        mvec_scale: constants.mvec_scale,
    })
}

/// The title's FidelityFX upscaler `context` is destroyed: what DLSS made
/// for it goes once submitted work has run.
pub fn upscaler_destroyed() {
    let (Some(sl), true) = (SL.get(), FUNCTIONS.get().is_some()) else {
        return;
    };
    crate::external::finish_submitted();
    let viewport = abi::ViewportHandle::new(VIEWPORT_DLSS);
    let mut calls = calls();
    // SAFETY: our viewport; nothing that names it is in flight.
    let code = unsafe { (sl.free_resources)(abi::FEATURE_DLSS, &viewport) };
    calls.last = None;
    tracing::info!(
        code,
        result = abi::result_name(code),
        "DLSS: the title's upscaler was destroyed; DLSS let go of its resources"
    );
}
