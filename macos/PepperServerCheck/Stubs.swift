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
