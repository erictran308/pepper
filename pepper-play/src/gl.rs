//! An OpenGL context with no window (CGL), and render targets whose memory
//! is an IOSurface. libmpv draws only through OpenGL or in software, and
//! macOS 27 still has OpenGL.

use std::ffi::{c_char, c_int, c_void};

use crate::surface::Surface;

type CglContext = *mut c_void;

const CGL_PFA_COLOR_SIZE: c_int = 8;
const CGL_PFA_ALPHA_SIZE: c_int = 11;
const CGL_PFA_ACCELERATED: c_int = 73;
const CGL_PFA_ALLOW_OFFLINE_RENDERERS: c_int = 96;
const CGL_PFA_OPENGL_PROFILE: c_int = 99;
const CGL_OGLP_VERSION_3_2_CORE: c_int = 0x3200;

const GL_TEXTURE_RECTANGLE: u32 = 0x84F5;
const GL_RGBA: u32 = 0x1908;
const GL_BGRA: u32 = 0x80E1;
const GL_UNSIGNED_INT_8_8_8_8_REV: u32 = 0x8367;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
pub const GL_RGBA8: c_int = 0x8058;

#[link(name = "OpenGL", kind = "framework")]
unsafe extern "C" {
    fn CGLChoosePixelFormat(
        attribs: *const c_int,
        pix: *mut *mut c_void,
        npix: *mut c_int,
    ) -> c_int;
    fn CGLDestroyPixelFormat(pix: *mut c_void) -> c_int;
    fn CGLCreateContext(pix: *mut c_void, share: CglContext, ctx: *mut CglContext) -> c_int;
    fn CGLDestroyContext(ctx: CglContext) -> c_int;
    fn CGLSetCurrentContext(ctx: CglContext) -> c_int;
    fn CGLTexImageIOSurface2D(
        ctx: CglContext,
        target: u32,
        internal_format: u32,
        width: i32,
        height: i32,
        format: u32,
        kind: u32,
        surface: *mut c_void,
        plane: u32,
    ) -> c_int;

    fn glGenTextures(n: i32, textures: *mut u32);
    fn glDeleteTextures(n: i32, textures: *const u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glGenFramebuffers(n: i32, framebuffers: *mut u32);
    fn glDeleteFramebuffers(n: i32, framebuffers: *const u32);
    fn glBindFramebuffer(target: u32, framebuffer: u32);
    fn glFramebufferTexture2D(
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: u32,
        level: i32,
    );
    fn glCheckFramebufferStatus(target: u32) -> u32;
    fn glFinish();
}

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// libmpv's way to find OpenGL functions: they're all in OpenGL.framework,
/// which this program links.
pub unsafe extern "C" fn get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
    // SAFETY: name is a C string from libmpv.
    unsafe { dlsym(RTLD_DEFAULT, name) }
}

/// A CGL context, current on the thread that made it.
pub struct Context {
    raw: CglContext,
}

impl Context {
    pub fn new() -> Result<Context, String> {
        let attribs = [
            CGL_PFA_OPENGL_PROFILE,
            CGL_OGLP_VERSION_3_2_CORE,
            CGL_PFA_ACCELERATED,
            CGL_PFA_COLOR_SIZE,
            24,
            CGL_PFA_ALPHA_SIZE,
            8,
            CGL_PFA_ALLOW_OFFLINE_RENDERERS,
            0,
        ];
        let mut pix = std::ptr::null_mut();
        let mut count = 0;
        let mut raw = std::ptr::null_mut();
        // SAFETY: attribs ends in 0; the pixel format is destroyed once the
        // context holds its own reference.
        unsafe {
            let err = CGLChoosePixelFormat(attribs.as_ptr(), &mut pix, &mut count);
            if err != 0 || pix.is_null() {
                return Err(format!("CGLChoosePixelFormat failed: {err}"));
            }
            let err = CGLCreateContext(pix, std::ptr::null_mut(), &mut raw);
            CGLDestroyPixelFormat(pix);
            if err != 0 || raw.is_null() {
                return Err(format!("CGLCreateContext failed: {err}"));
            }
            CGLSetCurrentContext(raw);
        }
        Ok(Context { raw })
    }

    /// Waits for the graphics chip to finish what was drawn, so the frame is
    /// whole before Pepper shows it.
    pub fn finish(&self) {
        // SAFETY: the context is current on this thread.
        unsafe { glFinish() }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: we made the context; nothing uses it after this.
        unsafe {
            CGLSetCurrentContext(std::ptr::null_mut());
            CGLDestroyContext(self.raw);
        }
    }
}

/// A framebuffer that draws into an IOSurface.
pub struct Target {
    pub surface: Surface,
    texture: u32,
    pub framebuffer: u32,
}

impl Target {
    pub fn new(context: &Context, width: u32, height: u32) -> Result<Target, String> {
        let surface = Surface::new(width, height).ok_or("IOSurfaceCreate failed")?;
        let mut texture = 0;
        let mut framebuffer = 0;
        // SAFETY: the context is current on this thread; the texture's memory
        // is the surface, which the target keeps alive.
        unsafe {
            glGenTextures(1, &mut texture);
            glBindTexture(GL_TEXTURE_RECTANGLE, texture);
            let err = CGLTexImageIOSurface2D(
                context.raw,
                GL_TEXTURE_RECTANGLE,
                GL_RGBA,
                width as i32,
                height as i32,
                GL_BGRA,
                GL_UNSIGNED_INT_8_8_8_8_REV,
                surface.raw(),
                0,
            );
            glBindTexture(GL_TEXTURE_RECTANGLE, 0);
            if err != 0 {
                glDeleteTextures(1, &texture);
                return Err(format!("CGLTexImageIOSurface2D failed: {err}"));
            }

            glGenFramebuffers(1, &mut framebuffer);
            glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
            glFramebufferTexture2D(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_RECTANGLE,
                texture,
                0,
            );
            let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            if status != GL_FRAMEBUFFER_COMPLETE {
                glDeleteFramebuffers(1, &framebuffer);
                glDeleteTextures(1, &texture);
                return Err(format!("framebuffer incomplete: {status:#x}"));
            }
        }
        Ok(Target {
            surface,
            texture,
            framebuffer,
        })
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        // SAFETY: the context that made them is still current (targets are
        // dropped before it).
        unsafe {
            glDeleteFramebuffers(1, &self.framebuffer);
            glDeleteTextures(1, &self.texture);
        }
    }
}
