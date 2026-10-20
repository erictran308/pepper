import AppKit

class HiddenTitlebarTerminalWindow: TerminalWindow {
    // No titlebar, we don't support accessories.
    override var supportsUpdateAccessory: Bool { false }

    override func awakeFromNib() {
        super.awakeFromNib()

        // Setup our initial style
        reapplyHiddenStyle()

        // Notifications
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(fullscreenDidExit(_:)),
            name: .fullscreenDidExit,
            object: nil)
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
    }

    private static let hiddenStyleMask: NSWindow.StyleMask = [
        // We need `titled` in the mask to get the normal window frame
        .titled,

        // Full size content view so we can extend
        // content in to the hidden titlebar's area
        .fullSizeContentView,

        .resizable,
        .closable,
        .miniaturizable,
    ]

    /// Apply the hidden titlebar style.
    private func reapplyHiddenStyle() {
        // If our window is fullscreen then we don't reapply the hidden style because
        // it can result in messing up non-native fullscreen. See:
        // https://github.com/ghostty-org/ghostty/issues/8415
        if terminalController?.fullscreenStyle?.isFullscreen ?? false {
            return
        }

        // Apply our style mask while preserving the .fullScreen option
        if styleMask.contains(.fullScreen) {
            styleMask = Self.hiddenStyleMask.union([.fullScreen])
        } else {
            styleMask = Self.hiddenStyleMask
        }

        // Hide the title
        titleVisibility = .hidden
        titlebarAppearsTransparent = true

        // Hide the traffic lights (window control buttons)
        standardWindowButton(.closeButton)?.isHidden = true
        standardWindowButton(.miniaturizeButton)?.isHidden = true
        standardWindowButton(.zoomButton)?.isHidden = true

        // pepper: tabs stay with the titlebar hidden: Pepper draws their bar
        // (PepperTabBar.swift), from macOS 15. Before, as Ghostty: no tabs,
        // since their bar is hidden with the titlebar.
        if #unavailable(macOS 15.0) { tabbingMode = .disallowed }

        // Nuke it from orbit -- hide the titlebar container entirely, just in case. There are
        // some operations that appear to bring back the titlebar visibility so this ensures
        // it is gone forever.
        if let themeFrame = contentView?.superview,
           let titleBarContainer = themeFrame.firstDescendant(withClassName: "NSTitlebarContainerView") {
            titleBarContainer.isHidden = true
        }
    }

    // MARK: NSWindow

    // pepper: macOS's tab bar takes no room under Pepper's. And no titlebar
    // shows, so neither do its accessories: Ghostty's reset-zoom one (on the
    // right) is left out, which must not sit beside a tab bar (see
    // TerminalWindow.tabBarDidAppear).
    override func addTitlebarAccessoryViewController(_ childViewController: NSTitlebarAccessoryViewController) {
        if childViewController.layoutAttribute == .right && !isTabBar(childViewController) { return }
        super.addTitlebarAccessoryViewController(childViewController)
        if isTabBar(childViewController) { childViewController.isHidden = true }
    }

    // pepper: Pepper's tab bar takes the terminal's colors again too.
    override func syncAppearance(_ surfaceConfig: Ghostty.SurfaceView.DerivedConfig) {
        super.syncAppearance(surfaceConfig)
        NotificationCenter.default.post(name: PepperTabs.appearanceDidChange, object: self)
    }

    override var title: String {
        didSet {
            // Updating the title text as above automatically reveals the
            // native title view in macOS 15.0 and above. Since we're using
            // a custom view instead, we need to re-hide it.
            reapplyHiddenStyle()
        }
    }

    // We override this so that with the hidden titlebar style the titlebar
    // area is not draggable.
    override var contentLayoutRect: CGRect {
        var rect = super.contentLayoutRect
        rect.origin.y = 0
        rect.size.height = self.frame.height
        return rect
    }

    // MARK: Notifications

    @objc private func fullscreenDidExit(_ notification: Notification) {
        // Make sure they're talking about our window
        guard let fullscreen = notification.object as? FullscreenBase else { return }
        guard fullscreen.window == self else { return }

        // On exit we need to reapply the style because macOS breaks it usually.
        // This is safe to call repeatedly so if its not broken its still safe.
        reapplyHiddenStyle()
    }
}
