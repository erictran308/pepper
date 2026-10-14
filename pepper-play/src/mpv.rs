//! The part of libmpv's C API this program uses (client.h, render.h and
//! render_gl.h, client API 2.5, mpv 0.41).
//!
//! libmpv is loaded at run time ([`load`]), not linked: Pepper ships this
//! program, and the user's mpv (Homebrew's, MacPorts') brings the library.

use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_void};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::OnceLock;

#[repr(C)]
pub struct Handle {
    _private: [u8; 0],
}

#[repr(C)]
pub struct RenderContext {
    _private: [u8; 0],
}

#[repr(C)]
pub struct Event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct ClientMessage {
    pub num_args: c_int,
    pub args: *const *const c_char,
}

#[repr(C)]
pub struct EventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct LogMessage {
    pub prefix: *const c_char,
    pub level: *const c_char,
    pub text: *const c_char,
    pub log_level: c_int,
}

#[repr(C)]
pub struct EndFile {
    pub reason: c_int,
    pub error: c_int,
}

#[repr(C)]
pub struct RenderParam {
    pub kind: c_int,
    pub data: *mut c_void,
}

pub type GetProcAddress =
    unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void;

#[repr(C)]
pub struct OpenGlInitParams {
    pub get_proc_address: Option<GetProcAddress>,
    pub get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
pub struct OpenGlFbo {
    pub fbo: c_int,
    pub w: c_int,
    pub h: c_int,
    pub internal_format: c_int,
}

pub const EVENT_NONE: c_int = 0;
pub const EVENT_SHUTDOWN: c_int = 1;
pub const EVENT_LOG_MESSAGE: c_int = 2;
pub const EVENT_END_FILE: c_int = 7;
pub const EVENT_CLIENT_MESSAGE: c_int = 16;
pub const EVENT_PROPERTY_CHANGE: c_int = 22;

pub const END_FILE_ERROR: c_int = 4;

pub const FORMAT_STRING: c_int = 1;
pub const FORMAT_FLAG: c_int = 3;
pub const FORMAT_INT64: c_int = 4;
pub const FORMAT_DOUBLE: c_int = 5;

pub const RENDER_PARAM_INVALID: c_int = 0;
pub const RENDER_PARAM_API_TYPE: c_int = 1;
pub const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
pub const RENDER_PARAM_OPENGL_FBO: c_int = 3;
pub const RENDER_PARAM_FLIP_Y: c_int = 4;

pub const RENDER_UPDATE_FRAME: u64 = 1;

pub type UpdateCallback = unsafe extern "C" fn(ctx: *mut c_void);

/// Where libmpv is looked for, unless `PEPPER_LIBMPV` says: Homebrew on
/// Apple chips, MacPorts, Homebrew on Intel.
const PLACES: [&str; 3] = [
    "/opt/homebrew/lib/libmpv.2.dylib",
    "/opt/local/lib/libmpv.2.dylib",
    "/usr/local/lib/libmpv.2.dylib",
];

/// The client API's major version this program is written for (client.h's
/// MPV_CLIENT_API_VERSION >> 16).
const API_MAJOR: c_ulong = 2;

const RTLD_NOW: c_int = 0x2;
const RTLD_LOCAL: c_int = 0x4;

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn getuid() -> u32;
    fn getxattr(
        path: *const c_char,
        name: *const c_char,
        value: *mut c_void,
        size: usize,
        position: u32,
        options: c_int,
    ) -> isize;
}

/// Declares libmpv's functions: each becomes a pointer in [`Api`], found by
/// [`load`], and a function of the same name and type here that calls it.
macro_rules! api {
    ($(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
        struct Api {
            $($name: unsafe extern "C" fn($($ty),*) $(-> $ret)?,)*
        }

        fn find(lib: *mut c_void) -> Result<Api, String> {
            Ok(Api {
                $($name: {
                    let name = concat!(stringify!($name), "\0");
                    // SAFETY: lib is an open library; the name is a C string.
                    let symbol = unsafe { dlsym(lib, name.as_ptr().cast()) };
                    if symbol.is_null() {
                        return Err(format!("it has no {}", stringify!($name)));
                    }
                    // SAFETY: the symbol is libmpv's function of this type
                    // (client.h, render.h).
                    unsafe {
                        std::mem::transmute::<
                            *mut c_void,
                            unsafe extern "C" fn($($ty),*) $(-> $ret)?,
                        >(symbol)
                    }
                },)*
            })
        }

        $(
            /// libmpv's function of this name. Only after [`load`].
            pub unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
                // SAFETY: the caller keeps libmpv's rules for it.
                unsafe { (api().$name)($($arg),*) }
            }
        )*
    };
}

api! {
    fn mpv_client_api_version() -> c_ulong;
    fn mpv_create() -> *mut Handle;
    fn mpv_initialize(ctx: *mut Handle) -> c_int;
    fn mpv_set_option_string(ctx: *mut Handle, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_set_property_string(ctx: *mut Handle, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_command(ctx: *mut Handle, args: *mut *const c_char) -> c_int;
    fn mpv_wait_event(ctx: *mut Handle, timeout: f64) -> *mut Event;
    fn mpv_request_log_messages(ctx: *mut Handle, min_level: *const c_char) -> c_int;
    fn mpv_observe_property(ctx: *mut Handle, reply_userdata: u64, name: *const c_char, format: c_int) -> c_int;
    fn mpv_get_property_string(ctx: *mut Handle, name: *const c_char) -> *mut c_char;
    fn mpv_free(data: *mut c_void);
    fn mpv_terminate_destroy(ctx: *mut Handle);
    fn mpv_error_string(error: c_int) -> *const c_char;

    fn mpv_render_context_create(res: *mut *mut RenderContext, mpv: *mut Handle, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_set_update_callback(ctx: *mut RenderContext, callback: Option<UpdateCallback>, callback_ctx: *mut c_void);
    fn mpv_render_context_update(ctx: *mut RenderContext) -> u64;
    fn mpv_render_context_render(ctx: *mut RenderContext, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_report_swap(ctx: *mut RenderContext);
    fn mpv_render_context_free(ctx: *mut RenderContext);
}

static API: OnceLock<Api> = OnceLock::new();

fn api() -> &'static Api {
    API.get().expect("libmpv is loaded first")
}

/// Loads libmpv: from `PEPPER_LIBMPV` if set, else the first of [`PLACES`]
/// there is. Call it once, before anything else here.
///
/// The library is code this program runs, so only a file that only the user
/// or the system could have put there: an absolute path, the file and its
/// folder owned by the user or root and not writable by others, and not
/// downloaded (no quarantine).
pub fn load() -> Result<(), String> {
    let from_env = std::env::var("PEPPER_LIBMPV")
        .ok()
        .filter(|p| !p.is_empty());
    let place = match &from_env {
        Some(path) => path.clone(),
        None => PLACES
            .iter()
            .find(|p| Path::new(p).exists())
            .ok_or_else(|| {
                format!(
                    "libmpv not found (looked in {}): install mpv with `brew install mpv`, \
                     or set PEPPER_LIBMPV to libmpv.2.dylib",
                    PLACES.join(", ")
                )
            })?
            .to_string(),
    };
    let real = trusted(&place).map_err(|e| format!("won't load libmpv from {place}: {e}"))?;
    let path = CString::new(real.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("{place} holds a NUL"))?;
    // SAFETY: a C string path; the library stays open for the program's
    // life.
    let lib = unsafe { dlopen(path.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
    if lib.is_null() {
        // SAFETY: dlerror's text is a C string, valid until the next dl call.
        let error = unsafe { dlerror() };
        let why = if error.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        return Err(format!("cannot load libmpv from {place}: {why}"));
    }
    let api = find(lib).map_err(|e| format!("{place} isn't a libmpv this program can use: {e}"))?;
    let _ = API.set(api);
    // SAFETY: libmpv is loaded; a function with no arguments.
    let version = unsafe { mpv_client_api_version() };
    if version >> 16 != API_MAJOR {
        return Err(format!(
            "{place} has client API {}.{}; this program needs {API_MAJOR}.x",
            version >> 16,
            version & 0xffff
        ));
    }
    Ok(())
}

/// The real path of a library to load, if only the user or the system
/// could have put it there.
fn trusted(place: &str) -> Result<std::path::PathBuf, String> {
    if !place.starts_with('/') {
        return Err("not an absolute path".into());
    }
    let real = std::fs::canonicalize(place).map_err(|e| e.to_string())?;
    // SAFETY: getuid has no preconditions.
    let me = unsafe { getuid() };
    let folder = real.parent().ok_or("no folder")?;
    for path in [real.as_path(), folder] {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.uid() != me && meta.uid() != 0 {
            return Err(format!("{} belongs to someone else", path.display()));
        }
        if meta.mode() & 0o002 != 0 {
            return Err(format!("anyone can write to {}", path.display()));
        }
    }
    let c_path = CString::new(real.as_os_str().as_encoded_bytes()).map_err(|_| "holds a NUL")?;
    // SAFETY: C strings; with no buffer, getxattr only says the size.
    let quarantined = unsafe {
        getxattr(
            c_path.as_ptr(),
            c"com.apple.quarantine".as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    } >= 0;
    if quarantined {
        return Err(format!("{} was downloaded (quarantined)", real.display()));
    }
    Ok(real)
}

/// mpv's text for an error code.
pub fn error(code: c_int) -> String {
    // SAFETY: mpv_error_string returns a static string for any code.
    unsafe { CStr::from_ptr(mpv_error_string(code)) }
        .to_string_lossy()
        .into_owned()
}
