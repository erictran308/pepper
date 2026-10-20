import AppKit
import Combine
import GhosttyKit
import SwiftUI

/// Where Pepper's tab bar shows, and the room it takes.
enum PepperTabs {
    static let barHeight: CGFloat = 38

    /// Posted by the hidden titlebar's window when it takes the terminal's
    /// colors again (a config reload, a theme, OSC 11, another split
    /// focused): its bar does too.
    static let appearanceDidChange = Notification.Name("PepperTabsAppearanceDidChange")

    /// Whether the controller's window shows the bar: the hidden titlebar's
    /// window (`macos-titlebar-style = hidden`, not `window-decoration =
    /// none`'s plain one), on macOS 15 or later.
    static func showsBar(_ controller: TerminalController) -> Bool {
        guard #available(macOS 15.0, *) else { return false }
        return controller.window is HiddenTitlebarTerminalWindow
    }

    /// The window's content size for a terminal of `size`: the bar's height
    /// more where it shows, so the terminal keeps its rows.
    static func contentSize(_ size: NSSize?, _ controller: TerminalController) -> NSSize? {
        guard let size, showsBar(controller) else { return size }
        return NSSize(width: size.width, height: size.height + barHeight)
    }
}

/// A window's content with Pepper's tab bar over it where it shows; any
/// other window shows the content alone.
struct PepperTabbed<Content: View>: View {
    let controller: TerminalController
    let content: Content

    init(controller: TerminalController, @ViewBuilder content: () -> Content) {
        self.controller = controller
        self.content = content()
    }

    var body: some View {
        if PepperTabs.showsBar(controller) {
            if #available(macOS 15.0, *) {
                VStack(spacing: 0) {
                    PepperTabBar(controller: controller)
                    content
                }
                .ignoresSafeArea(.container, edges: .top)
            } else {
                content
            }
        } else {
            content
        }
    }
}

/// Pepper's tab bar. The tabs stay macOS's: each is a window of the tab
/// group, so goto_tab, move_tab, restoring and AppleScript work as before;
/// only the bar is Pepper's, where the hidden titlebar was. Each tab shows
/// what goes on in it: video Pepper shows, a progress report (OSC 9;4), a
/// bell.
///
/// Every window of a group has a bar, but only the selected tab's window is
/// on screen: only its bar keeps up, the others wait until theirs is.
@available(macOS 15.0, *)
struct PepperTabBar: View {
    @StateObject private var model: Model

    init(controller: TerminalController) {
        _model = StateObject(wrappedValue: Model(controller: controller))
    }

    var body: some View {
        HStack(alignment: .bottom, spacing: TabRow.spacing) {
            TabRow(selected: model.selectedIndex) {
                ForEach(model.tabs) { tab in
                    Item(tab: tab, model: model)
                }
            }
            .layoutPriority(1)
            Button(action: model.newTab) {
                Image(systemName: "plus")
                    .font(.system(size: 12, weight: .medium))
                    .frame(width: 26, height: 26)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(model.foreground.opacity(0.6))
            .padding(.bottom, 3)
            .help("New Tab")
            Spacer(minLength: 40)
        }
        .padding(.horizontal, 10)
        .frame(height: PepperTabs.barHeight, alignment: .bottom)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background {
            // The bar's empty space moves the window, as a titlebar does.
            Color.clear
                .contentShape(Rectangle())
                .gesture(WindowDragGesture())
                .allowsWindowActivationEvents(true)
        }
        .background(Color(nsColor: model.barColor))
        // The selected tab cuts the bar away under it (see Item): only
        // within the bar.
        .compositingGroup()
    }

    /// The tabs at one width, up to `maxTabWidth`, narrower to fit the bar
    /// down to `minTabWidth`. Past that, as many as fit show, the selected
    /// one among them. It needs no width of its own, so many tabs never
    /// widen the window.
    private struct TabRow: Layout {
        static let spacing: CGFloat = 2
        static let maxTabWidth: CGFloat = 200
        static let minTabWidth: CGFloat = 64
        static let tabHeight: CGFloat = 31

        let selected: Int

        func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
            let count = CGFloat(subviews.count)
            let ideal = max(0, count * Self.maxTabWidth + (count - 1) * Self.spacing)
            return CGSize(width: min(proposal.width ?? ideal, ideal), height: Self.tabHeight)
        }

        func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
            guard !subviews.isEmpty else { return }
            let fitting = max(1, Int((bounds.width + Self.spacing) / (Self.minTabWidth + Self.spacing)))
            let shown = min(subviews.count, fitting)
            let first = min(max(0, selected - shown + 1), subviews.count - shown)
            let width = (bounds.width - CGFloat(shown - 1) * Self.spacing) / CGFloat(shown)
            let each = min(Self.maxTabWidth, max(Self.minTabWidth, width))
            var x = bounds.minX
            for (index, subview) in subviews.enumerated() {
                guard (first..<first + shown).contains(index) else {
                    // Off the bar: nothing is drawn or clicked there.
                    subview.place(at: CGPoint(x: -100_000, y: bounds.minY), proposal: .zero)
                    continue
                }
                subview.place(
                    at: CGPoint(x: x, y: bounds.minY),
                    proposal: ProposedViewSize(width: each, height: bounds.height)
                )
                x += each + Self.spacing
            }
        }
    }

    private struct Item: View {
        let tab: Model.Tab
        @ObservedObject var model: Model
        @State private var hovering = false

        private var shape: UnevenRoundedRectangle {
            UnevenRoundedRectangle(topLeadingRadius: 8, topTrailingRadius: 8, style: .continuous)
        }

        private var textColor: Color {
            model.foreground.opacity(tab.isSelected ? 0.95 : 0.65)
        }

        var body: some View {
            HStack(spacing: 7) {
                Image(systemName: tab.video ? "play.rectangle.fill" : "terminal")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(iconColor)
                Text(tab.title)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Spacer(minLength: 0)
                status
                closeButton
            }
            .font(.system(size: 12, weight: tab.isSelected ? .medium : .regular))
            .foregroundStyle(textColor)
            .padding(.leading, 11)
            .padding(.trailing, 5)
            .frame(maxWidth: .infinity, minHeight: TabRow.tabHeight, maxHeight: TabRow.tabHeight)
            .overlay(alignment: .bottom) { progressLine }
            .background(shape.fill(fill))
            .background {
                // No bar under the selected tab: it's the terminal's
                // background alone, as see-through as the terminal.
                if tab.isSelected { shape.fill(.black).blendMode(.destinationOut) }
            }
            .contentShape(shape)
            .onTapGesture { model.select(tab) }
            .onHover { hovering = $0 }
            .draggable(tab.dragPayload)
            .dropDestination(for: String.self) { items, _ in
                guard let payload = items.first else { return false }
                return model.move(payload, to: tab)
            }
            .contextMenu { menu }
            .help(tab.fullTitle)
            .accessibilityAddTraits(tab.isSelected ? [.isButton, .isSelected] : .isButton)
            .accessibilityAction { model.select(tab) }
        }

        @ViewBuilder
        private var menu: some View {
            Button("Rename Tab…") { model.rename(tab) }
            Menu("Tab Color") {
                ForEach(TerminalTabColor.allCases, id: \.self) { color in
                    Button { model.setColor(color, of: tab) } label: {
                        if color == tab.tabColor {
                            Label(color.localizedName, systemImage: "checkmark")
                        } else {
                            Text(color.localizedName)
                        }
                    }
                }
            }
            Divider()
            Button("Close Tab") { model.close(tab) }
            Button("Close Other Tabs") { model.closeOthers(tab) }
                .disabled(model.tabs.count < 2)
            Button("Close Tabs to the Right") { model.closeToTheRight(tab) }
                .disabled(tab.isLast)
            Divider()
            Button("Move Tab to New Window") { model.moveToNewWindow(tab) }
                .disabled(model.tabs.count < 2)
        }

        private var fill: Color {
            if tab.isSelected { return Color(nsColor: model.background) }
            return hovering ? model.foreground.opacity(0.06) : .clear
        }

        private var iconColor: Color {
            if let color = tab.tabColor.displayColor { return Color(nsColor: color) }
            return tab.video ? Color(nsColor: .systemGreen) : textColor
        }

        @ViewBuilder
        private var status: some View {
            if tab.bell {
                Image(systemName: "bell.fill")
                    .font(.system(size: 9.5))
                    .foregroundStyle(Color(nsColor: .systemYellow))
            }
            if tab.video {
                // Still bars, not moving ones: a drawing that never stops
                // would cost more than the video.
                HStack(alignment: .bottom, spacing: 2) {
                    Capsule().frame(width: 2, height: 6)
                    Capsule().frame(width: 2, height: 10)
                    Capsule().frame(width: 2, height: 8)
                }
                .foregroundStyle(Color(nsColor: .systemGreen))
                .accessibilityLabel("Playing video")
            }
            if let report = tab.progress, let percent = report.progress {
                Text("\(min(percent, 100))%")
                    .font(.system(size: 10.5, design: .monospaced))
                    .foregroundStyle(model.foreground.opacity(0.6))
            }
        }

        @ViewBuilder
        private var progressLine: some View {
            if let report = tab.progress {
                GeometryReader { geo in
                    ZStack(alignment: .leading) {
                        Capsule().fill(model.foreground.opacity(0.12))
                        Capsule()
                            .fill(lineColor(report.state))
                            .frame(width: geo.size.width * fraction(report))
                    }
                }
                .frame(height: 2)
                .padding(.horizontal, 11)
                .padding(.bottom, 3)
            }
        }

        /// Without a number (indeterminate), the line is whole but faint.
        private func fraction(_ report: Ghostty.Action.ProgressReport) -> CGFloat {
            guard let percent = report.progress else { return 1 }
            return CGFloat(min(percent, 100)) / 100
        }

        /// SurfaceProgressBar's colors, as the terminal's own bar shows them.
        private func lineColor(_ state: Ghostty.Action.ProgressReport.State) -> Color {
            switch state {
            case .error: .red
            case .pause: .orange
            case .indeterminate: .accentColor.opacity(0.5)
            default: .accentColor
            }
        }

        private var closeButton: some View {
            let shown = tab.isSelected || hovering
            return Button { model.close(tab) } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 8.5, weight: .bold))
                    .frame(width: 18, height: 18)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .opacity(shown ? 1 : 0)
            .allowsHitTesting(shown)
            .help("Close Tab")
        }
    }

    /// The group's tabs, read again whenever something a tab shows changes,
    /// while this bar's window is the selected tab's.
    final class Model: ObservableObject {
        struct Tab: Identifiable {
            let id: ObjectIdentifier
            weak var window: NSWindow?
            let title: String
            /// The window's title, whole, for the tooltip.
            let fullTitle: String
            let isSelected: Bool
            let isLast: Bool
            let video: Bool
            let bell: Bool
            let progress: Ghostty.Action.ProgressReport?
            let tabColor: TerminalTabColor

            /// What a dragged tab carries: its window's number, looked for
            /// only among this group's windows when it's dropped.
            var dragPayload: String { "\(Model.dragPrefix)\(window?.windowNumber ?? 0)" }
        }

        fileprivate static let dragPrefix = "pepper-tab:"

        @Published private(set) var tabs: [Tab] = []
        /// The terminal's background, which the selected tab is.
        @Published private(set) var background: NSColor = .windowBackgroundColor

        var selectedIndex: Int { tabs.firstIndex { $0.isSelected } ?? 0 }

        var barColor: NSColor {
            let shaded = background.withAlphaComponent(1).blended(withFraction: 0.3, of: .black)
            return (shaded ?? background).withAlphaComponent(background.alphaComponent)
        }

        var foreground: Color {
            background.isLightColor ? .black : .white
        }

        private weak var controller: TerminalController?
        private var watching: [AnyCancellable] = []
        /// The windows `watching` is for: watched again only when they change.
        private var watched: [ObjectIdentifier] = []
        private var notices: [NSObjectProtocol] = []
        private var updateQueued = false

        init(controller: TerminalController) {
            self.controller = controller
            let center = NotificationCenter.default
            // This window shown again (its tab selected), or its colors
            // changed; video in any window's terminals.
            let names: [(Notification.Name, NSWindow?)] = [
                (NSWindow.didBecomeMainNotification, controller.window),
                (PepperTabs.appearanceDidChange, controller.window),
                (PepperServer.videoDidChange, nil),
            ]
            for (name, object) in names {
                notices.append(center.addObserver(forName: name, object: object, queue: .main) { [weak self] _ in
                    self?.queueUpdate()
                })
            }
            queueUpdate()
        }

        deinit {
            for notice in notices { NotificationCenter.default.removeObserver(notice) }
        }

        // MARK: Actions

        func select(_ tab: Tab) {
            guard let window = tab.window else { return }
            window.tabGroup?.selectedWindow = window
            window.makeKeyAndOrderFront(nil)
        }

        func newTab() {
            controller?.newTab(nil)
        }

        func close(_ tab: Tab) {
            guard let controller = tab.window?.windowController as? TerminalController else { return }
            // A tab closes at once unless it asks first; it asks on its own
            // window, which must be on screen for that.
            if controller.surfaceTree.contains(where: { $0.needsConfirmQuit }) {
                onScreen(tab) { $0.closeTab(nil) }
            } else {
                controller.closeTab(nil)
            }
        }

        func rename(_ tab: Tab) {
            onScreen(tab) { $0.promptTabTitle() }
        }

        func closeOthers(_ tab: Tab) {
            onScreen(tab) { $0.closeOtherTabs(nil) }
        }

        func closeToTheRight(_ tab: Tab) {
            onScreen(tab) { $0.closeTabsOnTheRight(nil) }
        }

        func moveToNewWindow(_ tab: Tab) {
            tab.window?.moveTabToNewWindow(nil)
        }

        func setColor(_ color: TerminalTabColor, of tab: Tab) {
            (tab.window as? TerminalWindow)?.tabColor = color
            queueUpdate()
        }

        /// Moves the dragged tab to `target`'s place, if it's a tab of this
        /// group; anything else dropped is refused.
        func move(_ payload: String, to target: Tab) -> Bool {
            guard payload.hasPrefix(Self.dragPrefix),
                  let number = Int(payload.dropFirst(Self.dragPrefix.count)),
                  let targetWindow = target.window,
                  let group = targetWindow.tabGroup,
                  let moving = group.windows.first(where: { $0.windowNumber == number }),
                  moving !== targetWindow,
                  let from = group.windows.firstIndex(of: moving),
                  let to = group.windows.firstIndex(of: targetWindow)
            else { return false }
            let selected = group.selectedWindow
            // As TerminalController's move_tab does.
            NSAnimationContext.beginGrouping()
            NSAnimationContext.current.duration = 0
            group.removeWindow(moving)
            targetWindow.addTabbedWindowSafely(moving, ordered: from < to ? .above : .below)
            (selected ?? moving).makeKeyAndOrderFront(nil)
            NSAnimationContext.endGrouping()
            return true
        }

        /// Selects the tab, then acts on its controller: its sheets show on
        /// its own window.
        private func onScreen(_ tab: Tab, _ action: @escaping (TerminalController) -> Void) {
            guard let controller = tab.window?.windowController as? TerminalController else { return }
            select(tab)
            DispatchQueue.main.async { action(controller) }
        }

        // MARK: Updating

        /// Many changes come together (a tab closing changes the group and
        /// a window's main): one update after them.
        private func queueUpdate() {
            guard !updateQueued else { return }
            updateQueued = true
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                self.updateQueued = false
                self.update()
            }
        }

        private func update() {
            // Only the selected tab's window is on screen: another's bar
            // stops watching until its window is main again.
            guard let window = controller?.window,
                  (window.tabGroup?.selectedWindow ?? window) === window
            else {
                watching = []
                watched = []
                return
            }
            let windows = window.tabbedWindows ?? [window]
            tabs = windows.enumerated().map { index, tabWindow in
                let tabController = tabWindow.windowController as? BaseTerminalController
                let title = Self.withoutBell(tabWindow.title)
                return Tab(
                    id: ObjectIdentifier(tabWindow),
                    window: tabWindow,
                    title: Self.shortTitle(title),
                    fullTitle: title,
                    isSelected: tabWindow === window,
                    isLast: index == windows.count - 1,
                    video: tabController?.surfaceTree.contains { PepperServer.shared.showsVideo(in: $0) } ?? false,
                    bell: tabController?.bell ?? false,
                    progress: tabController?.surfaceTree.lazy.compactMap(\.progressReport).first { $0.state != .remove },
                    tabColor: (tabWindow as? TerminalWindow)?.tabColor ?? .none
                )
            }
            background = terminalColor((window as? TerminalWindow)?.preferredBackgroundColor ?? .windowBackgroundColor)
            let ids = windows.map { ObjectIdentifier($0) }
            if ids != watched {
                watch(window, windows)
                watched = ids
            }
        }

        /// Watches what the tabs show: the group, each tab's title, bell and
        /// progress, the last two followed through its splits by its
        /// controller. Video comes by PepperServer's notice.
        private func watch(_ window: NSWindow, _ windows: [NSWindow]) {
            var watching: [AnyCancellable] = []
            let changed: (Any) -> Void = { [weak self] _ in self?.queueUpdate() }
            if let group = window.tabGroup {
                watching.append(group.publisher(for: \.windows).dropFirst().sink(receiveValue: changed))
            }
            for tabWindow in windows {
                watching.append(tabWindow.publisher(for: \.title).dropFirst().sink(receiveValue: changed))
                guard let controller = tabWindow.windowController as? BaseTerminalController else { continue }
                watching.append(controller.$bell.dropFirst().sink(receiveValue: changed))
                watching.append(controller.surfaceValuesPublisher(
                    valueKeyPath: \.progressReport,
                    publisherKeyPath: \.$progressReport
                ).dropFirst().sink(receiveValue: changed))
            }
            self.watching = watching
        }

        // MARK: Colors and titles

        /// With `window-colorspace = display-p3` the terminal reads its
        /// colors' numbers as Display P3, so the selected tab does too, or it
        /// would differ from the terminal under it.
        private func terminalColor(_ color: NSColor) -> NSColor {
            let config = controller?.ghostty.config.config
            guard Self.configString(config, "window-colorspace") == "display-p3",
                  let rgb = color.usingColorSpace(.sRGB)
            else { return color }
            return NSColor(
                displayP3Red: rgb.redComponent,
                green: rgb.greenComponent,
                blue: rgb.blueComponent,
                alpha: rgb.alphaComponent
            )
        }

        private static func configString(_ config: ghostty_config_t?, _ key: String) -> String? {
            guard let config else { return nil }
            var v: UnsafePointer<Int8>?
            guard ghostty_config_get(config, &v, key, UInt(key.lengthOfBytes(using: .utf8))),
                  let v else { return nil }
            return String(cString: v)
        }

        /// Ghostty puts a bell before the title (`bell-features = title`);
        /// the tab shows its own.
        private static func withoutBell(_ title: String) -> String {
            title.hasPrefix("🔔 ") ? String(title.dropFirst(2)) : title
        }

        /// A title that's a path (Ghostty's fallback when the shell sets
        /// none, or zsh integration's at a prompt) shows its last folder, as
        /// a prompt does: `~` for home.
        private static func shortTitle(_ title: String) -> String {
            guard !title.isEmpty else { return "Terminal" }
            let home = NSHomeDirectory()
            var path = title
            if path == home || path.hasPrefix(home + "/") {
                path = "~" + path.dropFirst(home.count)
            }
            // "…/Personal/tuitube" is zsh integration's title for a deep folder.
            guard path.hasPrefix("/") || path.hasPrefix("~") || path.hasPrefix("…/") else { return title }
            while path.count > 1, path.hasSuffix("/") { path.removeLast() }
            return path == "~" ? path : (path as NSString).lastPathComponent
        }
    }
}
