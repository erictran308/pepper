//! What macOS's Now Playing shows (Control Center, notch apps): the title,
//! the channel, the length and where it is, paused or not, and the app's
//! cover. The mpv program sends them itself; inside libmpv, mpv sends none
//! of them (only as its own app, which this program can't be: see
//! `runloop.rs`), so this does, from mpv's properties. mpv still takes the
//! media keys and the rest of Now Playing's controls (`input-media-keys`).

use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::Mutex;
use std::time::Instant;

use crate::mpv;
use crate::objc::{
    Id, Sel, class, objc_autorelease, objc_release, objc_retain, sel, send, with_pool,
};

#[link(name = "MediaPlayer", kind = "framework")]
unsafe extern "C" {
    static MPMediaItemPropertyTitle: Id;
    static MPMediaItemPropertyArtist: Id;
    static MPMediaItemPropertyPlaybackDuration: Id;
    static MPMediaItemPropertyArtwork: Id;
    static MPNowPlayingInfoPropertyElapsedPlaybackTime: Id;
    static MPNowPlayingInfoPropertyPlaybackRate: Id;
    static MPNowPlayingInfoPropertyMediaType: Id;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

unsafe extern "C" {
    /// The class of a block with nothing captured (libclosure).
    static _NSConcreteGlobalBlock: [usize; 0];
}

/// MPNowPlayingInfoMediaTypeVideo.
const MEDIA_TYPE_VIDEO: usize = 2;
/// MPNowPlayingPlaybackState: playing, paused.
const STATE_PLAYING: usize = 1;
const STATE_PAUSED: usize = 2;
/// How far, in seconds, the position may be from where Now Playing has it
/// (it moves it on itself) before it's told again: a seek.
const JUMP: f64 = 1.5;

/// The mpv properties followed, with their formats.
const FOLLOWED: [(&CStr, c_int); 7] = [
    (c"media-title", mpv::FORMAT_STRING),
    (c"metadata/by-key/artist", mpv::FORMAT_STRING),
    (c"duration", mpv::FORMAT_DOUBLE),
    (c"time-pos", mpv::FORMAT_DOUBLE),
    (c"pause", mpv::FORMAT_FLAG),
    (c"speed", mpv::FORMAT_DOUBLE),
    (c"track-list/count", mpv::FORMAT_INT64),
];

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

/// A block, as the C ABI lays it out: what MPMediaItemArtwork calls for the
/// picture.
#[repr(C)]
struct Block {
    isa: *const [usize; 0],
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*const Block, CGSize) -> Id,
    descriptor: *const BlockDescriptor,
}

#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
    /// Its type, which frameworks read to call it across processes.
    signature: *const c_char,
}

/// BLOCK_IS_GLOBAL: never copied or freed. BLOCK_HAS_SIGNATURE: the
/// descriptor has its type.
const BLOCK_FLAGS: i32 = (1 << 28) | (1 << 30);

struct Descriptor(BlockDescriptor);
// SAFETY: never written to.
unsafe impl Sync for Descriptor {}

static DESCRIPTOR: Descriptor = Descriptor(BlockDescriptor {
    reserved: 0,
    size: size_of::<Block>(),
    // NSImage *(^)(CGSize): an object back; the block, then a CGSize.
    signature: c"@24@?0{CGSize=dd}8".as_ptr(),
});

/// The cover's NSImage (its address), which the block hands out: one
/// reference of ours, released once another takes its place.
static COVER: Mutex<usize> = Mutex::new(0);

unsafe extern "C" fn cover_image(_block: *const Block, _size: CGSize) -> Id {
    let cover = COVER.lock().unwrap_or_else(|e| e.into_inner());
    let image = *cover as Id;
    if image.is_null() {
        return image;
    }
    // SAFETY: a live image, held by COVER while the lock is; handed out
    // retained and autoreleased, as a block returns an object, so it
    // outlives a new cover taking its place.
    unsafe { objc_autorelease(objc_retain(image)) }
}

struct Global(Block);
// SAFETY: a global block is never written to.
unsafe impl Sync for Global {}

static COVER_BLOCK: Global = Global(Block {
    isa: &raw const _NSConcreteGlobalBlock,
    flags: BLOCK_FLAGS,
    reserved: 0,
    invoke: cover_image,
    descriptor: &raw const DESCRIPTOR.0,
});

pub struct NowPlaying {
    /// Off with `--input-media-keys=no`.
    on: bool,
    title: String,
    artist: String,
    duration: f64,
    position: f64,
    paused: bool,
    speed: f64,
    /// Whether the app's cover is shown: only for an app, which chose the
    /// file, not for one mpv found next to a video played from a shell.
    covers: bool,
    /// The cover's path, and Now Playing's artwork made of it (one
    /// reference of ours).
    cover: Option<String>,
    artwork: Id,
    /// Where Now Playing was last told the video is, and when.
    told: Option<(f64, Instant)>,
}

// SAFETY: the artwork is only ever used from the thread that owns this.
unsafe impl Send for NowPlaying {}

impl NowPlaying {
    /// Follows mpv's properties for Now Playing, unless the app turned
    /// Now Playing off; with the app's cover if `covers`.
    pub fn start(handle: *mut mpv::Handle, covers: bool) -> NowPlaying {
        let on = get_string(handle, c"input-media-keys").as_deref() != Some("no");
        if on {
            for (i, (name, format)) in FOLLOWED.iter().enumerate() {
                // SAFETY: a live handle and a C string.
                unsafe { mpv::mpv_observe_property(handle, i as u64, name.as_ptr(), *format) };
            }
        }
        NowPlaying {
            on,
            title: String::new(),
            artist: String::new(),
            duration: 0.0,
            position: 0.0,
            paused: false,
            speed: 1.0,
            covers,
            cover: None,
            artwork: std::ptr::null_mut(),
            told: None,
        }
    }

    /// A property mpv says changed.
    pub fn on_property(&mut self, handle: *mut mpv::Handle, property: &mpv::EventProperty) {
        if !self.on || property.name.is_null() {
            return;
        }
        // SAFETY: mpv's property event: a C string name, and data of the
        // format it says (none when the property is unavailable).
        let name = unsafe { CStr::from_ptr(property.name) };
        let data = property.data;
        let string = || -> String {
            if property.format != mpv::FORMAT_STRING || data.is_null() {
                return String::new();
            }
            // SAFETY: a string property's data is a char*.
            unsafe { CStr::from_ptr(*data.cast::<*const c_char>()) }
                .to_string_lossy()
                .into_owned()
        };
        let double = || -> Option<f64> {
            // SAFETY: a double property's data is a double.
            (property.format == mpv::FORMAT_DOUBLE && !data.is_null())
                .then(|| unsafe { *data.cast::<f64>() })
                .filter(|v| v.is_finite())
        };
        match name.to_bytes() {
            b"media-title" => self.title = string(),
            b"metadata/by-key/artist" => self.artist = string(),
            b"duration" => self.duration = double().unwrap_or(0.0).max(0.0),
            b"speed" => self.speed = double().unwrap_or(1.0),
            b"pause" => {
                // SAFETY: a flag property's data is an int.
                self.paused = property.format == mpv::FORMAT_FLAG
                    && !data.is_null()
                    && unsafe { *data.cast::<c_int>() } != 0;
            }
            b"time-pos" => {
                self.position = double().unwrap_or(0.0).max(0.0);
                if !self.jumped() {
                    return;
                }
            }
            b"track-list/count" => {
                if !self.covers {
                    return;
                }
                // Once found, the cover stays, whatever the list does after.
                let cover = find_cover(handle);
                if cover.is_none() || cover == self.cover {
                    return;
                }
                self.set_cover(cover);
            }
            _ => return,
        }
        self.publish();
    }

    /// Says it all again: now and then, in case anything else wrote over it.
    pub fn refresh(&mut self) {
        if self.on && self.told.is_some() {
            self.publish();
        }
    }

    /// Whether the position is off from where Now Playing, moving it on
    /// itself, has it.
    fn jumped(&self) -> bool {
        let Some((at, when)) = self.told else {
            return true;
        };
        let rate = if self.paused { 0.0 } else { self.speed };
        let expected = at + when.elapsed().as_secs_f64() * rate;
        (self.position - expected).abs() > JUMP
    }

    fn set_cover(&mut self, cover: Option<String>) {
        self.cover = cover;
        let Some(path) = self.cover.as_deref().and_then(|p| CString::new(p).ok()) else {
            return;
        };
        with_pool(|| {
            // SAFETY: AppKit and MediaPlayer calls through their own
            // signatures. The image and the artwork are made here with one
            // reference each, which COVER and self.artwork hold; the ones
            // they held before are released once nothing hands them out.
            unsafe {
                let path: Id = send!(unsafe extern "C" fn(Id, Sel, *const c_char) -> Id)(
                    class(c"NSString"),
                    sel(c"stringWithUTF8String:"),
                    path.as_ptr(),
                );
                let image: Id =
                    send!(unsafe extern "C" fn(Id, Sel) -> Id)(class(c"NSImage"), sel(c"alloc"));
                let image: Id = send!(unsafe extern "C" fn(Id, Sel, Id) -> Id)(
                    image,
                    sel(c"initWithContentsOfFile:"),
                    path,
                );
                if image.is_null() {
                    return;
                }
                let old = std::mem::replace(
                    &mut *COVER.lock().unwrap_or_else(|e| e.into_inner()),
                    image as usize,
                );
                if old != 0 {
                    objc_release(old as Id);
                }
                let size: CGSize =
                    send!(unsafe extern "C" fn(Id, Sel) -> CGSize)(image, sel(c"size"));
                let artwork: Id = send!(unsafe extern "C" fn(Id, Sel) -> Id)(
                    class(c"MPMediaItemArtwork"),
                    sel(c"alloc"),
                );
                let artwork = send!(unsafe extern "C" fn(Id, Sel, CGSize, *const Block) -> Id)(
                    artwork,
                    sel(c"initWithBoundsSize:requestHandler:"),
                    size,
                    &raw const COVER_BLOCK.0,
                );
                // The info Now Playing has holds the old artwork itself.
                let old = std::mem::replace(&mut self.artwork, artwork);
                if !old.is_null() {
                    objc_release(old);
                }
            }
        });
    }

    fn publish(&mut self) {
        with_pool(|| {
            // SAFETY: Foundation and MediaPlayer calls through their own
            // signatures, with objects made here or kept by `self`.
            unsafe {
                let info: Id = send!(unsafe extern "C" fn(Id, Sel) -> Id)(
                    class(c"NSMutableDictionary"),
                    sel(c"dictionary"),
                );
                let set = |key: Id, value: Id| {
                    if !value.is_null() {
                        send!(unsafe extern "C" fn(Id, Sel, Id, Id))(
                            info,
                            sel(c"setObject:forKey:"),
                            value,
                            key,
                        );
                    }
                };
                let rate = if self.paused { 0.0 } else { self.speed };
                set(MPMediaItemPropertyTitle, ns_string(&self.title));
                set(MPMediaItemPropertyArtist, ns_string(&self.artist));
                set(
                    MPMediaItemPropertyPlaybackDuration,
                    ns_double(self.duration),
                );
                set(
                    MPNowPlayingInfoPropertyElapsedPlaybackTime,
                    ns_double(self.position),
                );
                set(MPNowPlayingInfoPropertyPlaybackRate, ns_double(rate));
                set(
                    MPNowPlayingInfoPropertyMediaType,
                    ns_unsigned(MEDIA_TYPE_VIDEO),
                );
                set(MPMediaItemPropertyArtwork, self.artwork);
                let center: Id = send!(unsafe extern "C" fn(Id, Sel) -> Id)(
                    class(c"MPNowPlayingInfoCenter"),
                    sel(c"defaultCenter"),
                );
                send!(unsafe extern "C" fn(Id, Sel, Id))(center, sel(c"setNowPlayingInfo:"), info);
                let state = if self.paused {
                    STATE_PAUSED
                } else {
                    STATE_PLAYING
                };
                send!(unsafe extern "C" fn(Id, Sel, usize))(
                    center,
                    sel(c"setPlaybackState:"),
                    state,
                );
            }
        });
        self.told = Some((self.position, Instant::now()));
    }
}

/// The album art the app added (`video-add FILE auto … albumart`, a file
/// of its own).
fn find_cover(handle: *mut mpv::Handle) -> Option<String> {
    let count: usize = get_string(handle, c"track-list/count")?.parse().ok()?;
    (0..count).find_map(|i| {
        let albumart = CString::new(format!("track-list/{i}/albumart")).ok()?;
        if get_string(handle, &albumart).as_deref() != Some("yes") {
            return None;
        }
        let file = CString::new(format!("track-list/{i}/external-filename")).ok()?;
        get_string(handle, &file).filter(|path| path.starts_with('/'))
    })
}

fn get_string(handle: *mut mpv::Handle, name: &CStr) -> Option<String> {
    // SAFETY: a live handle and a C string; mpv's answer is freed with
    // mpv_free once copied.
    unsafe {
        let value = mpv::mpv_get_property_string(handle, name.as_ptr());
        if value.is_null() {
            return None;
        }
        let out = CStr::from_ptr(value).to_string_lossy().into_owned();
        mpv::mpv_free(value.cast());
        Some(out)
    }
}

/// An autoreleased NSString (nil for one with a NUL).
unsafe fn ns_string(text: &str) -> Id {
    let Ok(text) = CString::new(text) else {
        return std::ptr::null_mut();
    };
    // SAFETY: in a pool (`publish`), through the method's own signature.
    unsafe {
        send!(unsafe extern "C" fn(Id, Sel, *const c_char) -> Id)(
            class(c"NSString"),
            sel(c"stringWithUTF8String:"),
            text.as_ptr(),
        )
    }
}

unsafe fn ns_double(value: f64) -> Id {
    // SAFETY: as in `ns_string`.
    unsafe {
        send!(unsafe extern "C" fn(Id, Sel, f64) -> Id)(
            class(c"NSNumber"),
            sel(c"numberWithDouble:"),
            value,
        )
    }
}

unsafe fn ns_unsigned(value: usize) -> Id {
    // SAFETY: as in `ns_string`.
    unsafe {
        send!(unsafe extern "C" fn(Id, Sel, usize) -> Id)(
            class(c"NSNumber"),
            sel(c"numberWithUnsignedInteger:"),
            value,
        )
    }
}
