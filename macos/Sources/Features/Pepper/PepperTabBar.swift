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
/// bell. It's always there, even with one tab, so a second tab doesn't move
/// the terminal down.
///
/// Every window of a group has a bar, but only the selected tab's window is
/// on screen: only its bar keeps up, the others catch up when theirs is.
@available(macOS 15.0, *)
struct PepperTabBar: View {
    @StateObject private var model: Model
    /// The bar's width, which says how many tabs fit.
    @State private var width: CGFloat = 0

    private static let padding: CGFloat = 10
    private static let newTabWidth: CGFloat = 26
    /// The least empty space kept for moving the window.
    private static let dragRoom: CGFloat = 40

    init(controller: TerminalController) {
        _model = StateObject(wrappedValue: Model(controller: controller))
    }

    var body: some View {
        HStack(alignment: .bottom, spacing: TabRow.spacing) {
            TabRow {
                ForEach(shownTabs) { tab in
                    Item(tab: tab, model: model)
                }
            }
            .layoutPriority(1)
            Button(action: model.newTab) {
                Image(systemName: "plus")
                    .font(.system(size: 12, weight: .medium))
                    .frame(width: Self.newTabWidth, height: Self.newTabWidth)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(model.foreground.opacity(0.6))
            .padding(.bottom, 3)
            .help("New Tab")
            Spacer(minLength: Self.dragRoom)
        }
        .padding(.horizontal, Self.padding)
        .frame(height: PepperTabs.barHeight, alignment: .bottom)
        .frame(maxWidth: .infinity, alignment: .leading)
        .coordinateSpace(.named(Model.space))
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
        .background {
            // The bar's empty space moves the window, as a titlebar does.
            Color.clear
                .contentShape(Rectangle())
                .gesture(WindowDragGesture())
                .simultaneousGesture(TapGesture().onEnded { model.refocus() })
                .allowsWindowActivationEvents(true)
        }
        .background(Color(nsColor: model.barColor))
        // The selected tab cuts the bar away under it (see Item): only
        // within the bar.
        .compositingGroup()
    }

    /// As many tabs as fit at `TabRow.minTabWidth`, the selected one among
    /// them; the others aren't drawn at all.
    private var shownTabs: ArraySlice<Model.Tab> {
        let tabs = model.tabs
        guard width > 0, !tabs.isEmpty else { return tabs[...] }
        let room = width - 2 * Self.padding - Self.newTabWidth - Self.dragRoom - 2 * TabRow.spacing
        let fitting = max(1, Int((room + TabRow.spacing) / (TabRow.minTabWidth + TabRow.spacing)))
        let count = min(tabs.count, fitting)
        let selected = tabs.firstIndex { $0.isSelected } ?? 0
        let first = min(max(0, selected - count + 1), tabs.count - count)
        return tabs[first..<first + count]
    }

    /// The tabs at one width, up to `maxTabWidth`, narrower down to
    /// `minTabWidth` to fit. It asks for no width of its own (none ideal), so
    /// tabs never widen the window, or its default size.
    private struct TabRow: Layout {
        static let spacing: CGFloat = 2
        static let maxTabWidth: CGFloat = 200
        static let minTabWidth: CGFloat = 64
        static let tabHeight: CGFloat = 31

        func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
            let count = CGFloat(subviews.count)
            let widest = max(0, count * Self.maxTabWidth + (count - 1) * Self.spacing)
            return CGSize(width: min(proposal.width ?? 0, widest), height: Self.tabHeight)
        }

        func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
            guard !subviews.isEmpty else { return }
            let count = CGFloat(subviews.count)
            let width = (bounds.width - (count - 1) * Self.spacing) / count
            let each = min(Self.maxTabWidth, max(Self.minTabWidth, width))
            var x = bounds.minX
            for subview in subviews {
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

        private var dragOffset: CGFloat? {
            model.dragging?.id == tab.id ? model.dragging?.offset : nil
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
            .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(Model.space)) } action: {
                model.frames[tab.id] = $0
            }
            .onDisappear { model.frames[tab.id] = nil }
            // Dragged within the bar only: never a system drag, which the
            // terminal under the bar (or another app) would take as text.
            .offset(x: dragOffset ?? 0)
            .zIndex(dragOffset == nil ? 0 : 1)
            .onTapGesture { model.select(tab) }
            .gesture(
                DragGesture(minimumDistance: 4, coordinateSpace: .named(Model.space))
                    .onChanged { model.drag(tab, by: $0.translation.width) }
                    .onEnded { model.drop(tab, at: $0.location.x) }
            )
            .onHover { hovering = $0 }
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
        struct Tab: Identifiable, Equatable {
            let id: ObjectIdentifier
            weak var window: NSWindow?
            let title: String
            /// The tab's title, whole, for the tooltip.
            let fullTitle: String
            let isSelected: Bool
            let isLast: Bool
            let video: Bool
            let bell: Bool
            let progress: Ghostty.Action.ProgressReport?
            let tabColor: TerminalTabColor

            static func == (a: Tab, b: Tab) -> Bool {
                a.id == b.id && a.title == b.title && a.fullTitle == b.fullTitle
                    && a.isSelected == b.isSelected && a.isLast == b.isLast
                    && a.video == b.video && a.bell == b.bell && a.tabColor == b.tabColor
                    && a.progress?.state == b.progress?.state
                    && a.progress?.progress == b.progress?.progress
            }
        }

        struct Dragging {
            let id: ObjectIdentifier
            var offset: CGFloat
        }

        /// The bar's coordinate space, where tabs are found under a drag.
        static let space = "PepperTabBar"

        @Published private(set) var tabs: [Tab] = []
        /// The terminal's background, which the selected tab is.
        @Published private(set) var background: NSColor = .windowBackgroundColor
        /// The tab being dragged, and how far.
        @Published private(set) var dragging: Dragging?
        /// Where each tab drawn is, in the bar: where a dragged one lands.
        var frames: [ObjectIdentifier: CGRect] = [:]

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
            let window = controller.window
            // This window shown again (its tab selected, made main or not):
            // at once, so it never shows an old bar.
            for name in [NSWindow.didBecomeMainNotification, NSWindow.didChangeOcclusionStateNotification] {
                notices.append(center.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                    self?.update()
                })
            }
            // Its colors changed; video in any window's terminals.
            let later: [(Notification.Name, NSWindow?)] = [
                (PepperTabs.appearanceDidChange, window),
                (PepperServer.videoDidChange, nil),
            ]
            for (name, object) in later {
                notices.append(center.addObserver(forName: name, object: object, queue: .main) { [weak self] _ in
                    self?.queueUpdate()
                })
            }
            // Filled now, so a new tab's bar never shows empty.
            update()
        }

        deinit {
            for notice in notices { NotificationCenter.default.removeObserver(notice) }
        }

        // MARK: Actions

        func select(_ tab: Tab) {
            guard let window = tab.window else { return }
            window.tabGroup?.selectedWindow = window
            window.makeKeyAndOrderFront(nil)
            refocus(window)
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
                refocus()
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

        func drag(_ tab: Tab, by offset: CGFloat) {
            dragging = Dragging(id: tab.id, offset: offset)
        }

        /// The dragged tab goes to the place of the tab it's let go over
        /// (the first or last, past the ends).
        func drop(_ tab: Tab, at x: CGFloat) {
            dragging = nil
            defer { refocus() }
            let placed = tabs.compactMap { tab in frames[tab.id].map { (tab, $0) } }
            guard let first = placed.first, let last = placed.last else { return }
            let target = placed.first { $0.1.minX <= x && x <= $0.1.maxX }?.0
                ?? (x < first.1.minX ? first.0 : last.0)
            guard target.id != tab.id,
                  let moving = tab.window,
                  let targetWindow = target.window,
                  let group = targetWindow.tabGroup,
                  let from = group.windows.firstIndex(of: moving),
                  let to = group.windows.firstIndex(of: targetWindow)
            else { return }
            let selected = group.selectedWindow
            // As TerminalController's move_tab does.
            NSAnimationContext.beginGrouping()
            NSAnimationContext.current.duration = 0
            group.removeWindow(moving)
            if !targetWindow.addTabbedWindowSafely(moving, ordered: from < to ? .above : .below) {
                // Kept in the group, at its end, rather than left a window
                // of its own.
                group.addWindow(moving)
            }
            (selected ?? moving).makeKeyAndOrderFront(nil)
            NSAnimationContext.endGrouping()
        }

        /// Gives the terminal back the keyboard after a click in the bar: the
        /// bar and the terminal share a hosting view, which a click can make
        /// first responder.
        func refocus(_ window: NSWindow? = nil) {
            let target = (window ?? controller?.window)?.windowController as? BaseTerminalController
            guard let surface = target?.focusedSurface else { return }
            Ghostty.moveFocus(to: surface)
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
            // stops watching until its window is shown again.
            guard let window = controller?.window,
                  (window.tabGroup?.selectedWindow ?? window) === window
            else {
                watching = []
                watched = []
                return
            }
            let windows = window.tabbedWindows ?? [window]
            let bells = controller?.ghostty.config.bellFeatures.contains(.title) ?? true
            let tabs = windows.enumerated().map { index, tabWindow in
                let tabController = tabWindow.windowController as? BaseTerminalController
                // The title without the bell Ghostty puts before it: the
                // tab shows its own.
                let title = tabController.map { $0.titleOverride ?? $0.focusedSurface?.title ?? tabWindow.title }
                    ?? tabWindow.title
                return Tab(
                    id: ObjectIdentifier(tabWindow),
                    window: tabWindow,
                    title: Self.shortTitle(title),
                    fullTitle: title,
                    isSelected: tabWindow === window,
                    isLast: index == windows.count - 1,
                    video: tabController?.surfaceTree.contains { PepperServer.shared.showsVideo(in: $0) } ?? false,
                    bell: bells && (tabController?.bell ?? false),
                    progress: tabController?.surfaceTree.lazy.compactMap(\.progressReport).first { $0.state != .remove },
                    tabColor: (tabWindow as? TerminalWindow)?.tabColor ?? .none
                )
            }
            if tabs != self.tabs { self.tabs = tabs }
            let background = terminalColor((window as? TerminalWindow)?.preferredBackgroundColor ?? .windowBackgroundColor)
            if background != self.background { self.background = background }
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

        /// A config value by name, as Ghostty.Config reads its enums; it has
        /// no getter for this one.
        private static func configString(_ config: ghostty_config_t?, _ key: String) -> String? {
            guard let config else { return nil }
            var v: UnsafePointer<Int8>?
            guard ghostty_config_get(config, &v, key, UInt(key.lengthOfBytes(using: .utf8))),
                  let v else { return nil }
            return String(cString: v)
        }

        /// A title that's a folder (Ghostty's fallback when the shell sets
        /// none, or zsh integration's at a prompt) shows its last part, as a
        /// prompt does: `~` for home. A command typed with its program's
        /// path (zsh integration's title while it runs) shows the program's
        /// name, then the rest: `/bin/rm -rf x` is `rm -rf x`.
        private static func shortTitle(_ title: String) -> String {
            guard !title.isEmpty else { return "Terminal" }
            let home = NSHomeDirectory()
            var path = title
            if path == home || path.hasPrefix(home + "/") {
                path = "~" + path.dropFirst(home.count)
            }
            // "…/Personal/tuitube" is zsh integration's title for a deep folder.
            guard path.hasPrefix("/") || path.hasPrefix("~") || path.hasPrefix("…/") else { return title }
            if path.contains(" "), !path.hasPrefix("…/"), !isFolder(path, home) {
                let words = path.split(separator: " ", maxSplits: 1)
                let program = (String(words[0]) as NSString).lastPathComponent
                return words.count > 1 ? "\(program) \(words[1])" : program
            }
            while path.count > 1, path.hasSuffix("/") { path.removeLast() }
            return path == "~" ? path : (path as NSString).lastPathComponent
        }

        /// Whether `path` (`~` for home) is a folder: only asked of a title
        /// with a space, a folder's or a command's.
        private static func isFolder(_ path: String, _ home: String) -> Bool {
            let full = path.hasPrefix("~") ? home + path.dropFirst() : path
            var folder: ObjCBool = false
            return FileManager.default.fileExists(atPath: full, isDirectory: &folder) && folder.boolValue
        }
    }
}
