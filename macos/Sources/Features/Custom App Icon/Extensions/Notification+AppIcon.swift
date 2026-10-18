import AppKit

extension Notification.Name {
    /// Distributed Notification for DockTilePlugin to update icon
    ///
    /// Ghostty -> DockTilePlugin
    ///
    /// pepper: Pepper's own, so Ghostty's dock tile doesn't follow Pepper's
    /// icon changes, nor Pepper's Ghostty's.
    static let ghosttyIconDidChange = Notification.Name("io.github.erictran308.pepper.iconDidChange")
}
