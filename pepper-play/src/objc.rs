//! The little of the Objective-C runtime this program calls into (AppKit's
//! app, MediaPlayer's Now Playing), without a binding crate.

use std::ffi::{CStr, c_char, c_void};

pub type Id = *mut c_void;
pub type Sel = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    pub fn objc_getClass(name: *const c_char) -> Id;
    pub fn sel_registerName(name: *const c_char) -> Sel;
    pub fn objc_msgSend();
    pub fn objc_autoreleasePoolPush() -> *mut c_void;
    pub fn objc_autoreleasePoolPop(pool: *mut c_void);
}

/// `objc_msgSend` as a function of the given type: it must be called
/// through the method's exact signature.
macro_rules! send {
    ($ty:ty) => {
        std::mem::transmute::<unsafe extern "C" fn(), $ty>($crate::objc::objc_msgSend)
    };
}
pub(crate) use send;

/// The class named `name` (nil if there's none).
pub fn class(name: &CStr) -> Id {
    // SAFETY: a lookup by a C string.
    unsafe { objc_getClass(name.as_ptr()) }
}

pub fn sel(name: &CStr) -> Sel {
    // SAFETY: a lookup by a C string.
    unsafe { sel_registerName(name.as_ptr()) }
}

/// Runs `f` with an autorelease pool of its own: on a thread that isn't the
/// app's, nothing else would release what it makes.
pub fn with_pool<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: push and pop pair up around `f`.
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let out = f();
        objc_autoreleasePoolPop(pool);
        out
    }
}
