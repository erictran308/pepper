import AppKit

/// What PepperServer.swift uses of the app: a terminal's view (one with a
/// layer), and a screen's display.
enum Ghostty {
    final class SurfaceView: NSView {}
}

extension NSScreen {
    var displayID: UInt32? {
        deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32
    }
}

// The core's place action, as Swift imports ghostty.h's structs.
// swiftlint:disable type_name
struct ghostty_action_pepper_box_s {
    var x: UInt16
    var y: UInt16
    var width: UInt16
    var height: UInt16
}

struct ghostty_action_pepper_holes_s {
    var list: (
        ghostty_action_pepper_box_s,
        ghostty_action_pepper_box_s,
        ghostty_action_pepper_box_s,
        ghostty_action_pepper_box_s
    )
    var len: UInt8
}

struct ghostty_action_pepper_place_s {
    var token: UnsafePointer<CChar>!
    var x: UInt16
    var y: UInt16
    var width: UInt16
    var height: UInt16
    var holes: UnsafePointer<ghostty_action_pepper_holes_s>!
}
// swiftlint:enable type_name
