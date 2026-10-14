//! IOSurfaces: the frames' memory on the graphics chip, which Pepper shows
//! without copying.

use std::ffi::c_void;

pub type SurfaceRef = *mut c_void;

#[repr(C)]
struct CallBacks {
    _private: [u8; 0],
}

const CF_NUMBER_SINT32: isize = 3;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CallBacks;
    static kCFTypeDictionaryValueCallBacks: CallBacks;
    fn CFNumberCreate(alloc: *const c_void, kind: isize, value: *const c_void) -> *const c_void;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const CallBacks,
        value_callbacks: *const CallBacks,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    static kIOSurfaceWidth: *const c_void;
    static kIOSurfaceHeight: *const c_void;
    static kIOSurfaceBytesPerElement: *const c_void;
    static kIOSurfacePixelFormat: *const c_void;
    fn IOSurfaceCreate(properties: *const c_void) -> SurfaceRef;
    fn IOSurfaceCreateMachPort(surface: SurfaceRef) -> u32;
    fn IOSurfaceIsInUse(surface: SurfaceRef) -> u8;
}

/// A BGRA IOSurface.
pub struct Surface {
    raw: SurfaceRef,
    pub width: u32,
    pub height: u32,
}

impl Surface {
    pub fn new(width: u32, height: u32) -> Option<Surface> {
        let values: [i32; 4] = [width as i32, height as i32, 4, i32::from_be_bytes(*b"BGRA")];
        // SAFETY: plain CoreFoundation calls; every object made here is
        // released before returning, and the dictionary retains what it holds.
        unsafe {
            let numbers = values
                .map(|v| CFNumberCreate(std::ptr::null(), CF_NUMBER_SINT32, (&raw const v).cast()));
            let keys = [
                kIOSurfaceWidth,
                kIOSurfaceHeight,
                kIOSurfaceBytesPerElement,
                kIOSurfacePixelFormat,
            ];
            let properties = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                numbers.as_ptr(),
                keys.len() as isize,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            let raw = IOSurfaceCreate(properties);
            CFRelease(properties);
            numbers.iter().for_each(|n| CFRelease(*n));
            (!raw.is_null()).then_some(Surface { raw, width, height })
        }
    }

    pub fn raw(&self) -> SurfaceRef {
        self.raw
    }

    /// A send right to the surface, for handing it to Pepper.
    pub fn mach_port(&self) -> u32 {
        // SAFETY: self.raw is a live IOSurface.
        unsafe { IOSurfaceCreateMachPort(self.raw) }
    }

    /// Whether the window server is still showing it.
    pub fn in_use(&self) -> bool {
        // SAFETY: self.raw is a live IOSurface.
        unsafe { IOSurfaceIsInUse(self.raw) != 0 }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: we own one reference, from IOSurfaceCreate.
        unsafe { CFRelease(self.raw) }
    }
}
