import AppKit
import IOSurface
import OSLog
import QuartzCore
#if canImport(GhosttyKit)
// The core's action. PepperServerCheck has it from ghostty.h itself.
import GhosttyKit
#endif

/// pepper: shows video frames that another process hands over as IOSurfaces,
/// in a layer over a box of a terminal surface, so macOS composites them with
/// no copies.
///
/// Two halves meet by a token:
/// - Placing: a program in a tab prints `OSC 777;pepper;place;TOKEN;COL;ROW;COLS;ROWS`
///   (or `…;remove;TOKEN`). The core turns the cells into pixels and calls
///   `place`, so only a program that can write to that tab places video there.
///   Each view (tab or split) holds at most `maxPlacementsPerView` boxes, so
///   no tab's output can use up another's, and a box never shows outside its
///   view. A place can end in up to 4 holes, `;COL,ROW,COLS,ROWS` each: cells
///   (from the top left of the screen, as the box) where frames don't show,
///   so what the terminal draws there does (a popup over the video). The
///   core gives them to `place` in pixels too.
/// - Streaming: a process sends IOSurfaces over the Mach port named in
///   `PEPPER_PORT`, with the same token. Frames show only where that token
///   has been placed.
///
/// Messages, by msgh_id; each inline part starts with the token, NUL padded
/// to 64 bytes:
/// - attach: 1–4 BGRA IOSurfaces (at most `maxSide` pixels a side) as port
///   descriptors, then the token. The reply port (msgh_local_port, made a
///   send right) is the client: when it dies, its streams and their boxes go.
///   The stream is the attaching process's: only it can present, detach or
///   attach it again, and it ends when that process exits or runs another
///   program, whatever its client port does. A process has at most
///   `maxStreamsPerOwner` streams.
/// - present: the token, then a `UInt32` slot; shows that surface.
/// - detach: the token; ends the stream.
/// - hello: any token, with a reply port (a send-once right). Answered at
///   once, so the client learns from the kernel which process holds this
///   port's name.
///
/// Pepper answers on the client's port, never waiting for it:
/// - size: the token, then the box's width and height in pixels, whether
///   it's placed, and the display it's on (a CGDirectDisplayID, 0 if not
///   known), as `UInt32`s. Sent when a token's stream and placement both
///   exist, and when the placement moves, resizes or goes, or its window
///   goes to another display, so the client draws its frames at the box's
///   size and that display's refreshes.
/// - refused: the token, when an attach isn't taken (too many streams, a
///   surface it can't show, a token another process streams to), so the
///   client can offer its surfaces again later.
/// - hello's answer: the token.
///
/// The kernel's notices that a client died come on a port of their own, which
/// no other process can send to.
///
/// Every shell gets the port's name as `PEPPER_PORT`, pepper-play's path
/// (the player in `Contents/Helpers`) as `PEPPER_PLAY`, and what this Pepper
/// takes beyond the first `OSC 777;pepper` as `PEPPER_FEATURES` (comma
/// separated; `holes`), so a program knows before it sends them.
final class PepperServer {
    static let shared = PepperServer()

    enum MessageID: mach_msg_id_t {
        case attach = 0x7070_0001
        case present = 0x7070_0002
        case detach = 0x7070_0003
        case hello = 0x7070_0004
    }

    /// The size message, to a client.
    private static let sizeID: mach_msg_id_t = 0x7070_0101
    /// To a client, an attach not taken.
    private static let refusedID: mach_msg_id_t = 0x7070_0102
    /// To whoever said hello.
    private static let helloAnswerID: mach_msg_id_t = 0x7070_0103

    /// The kernel's dead-name notification (mach/notify.h).
    private static let notifyDeadName: mach_msg_id_t = 0o110

    /// MACH_PORT_TYPE_* values, macros Swift doesn't import.
    private static let portTypeSend: mach_port_type_t = 1 << (MACH_PORT_RIGHT_SEND + 16)
    private static let portTypeDeadName: mach_port_type_t = 1 << (MACH_PORT_RIGHT_DEAD_NAME + 16)
    private static let portTypeDeadNameRequest: mach_port_type_t = 0x8000_0000

    /// MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT) in a format-0
    /// trailer: the sender's audit token after each message received. The
    /// trailer's size, and where its audit token starts.
    private static let receiveAudit: mach_msg_option_t = 3 << 24
    private static let auditTrailerSize = 52
    private static let auditTokenOffset = 20

    private static let logger = Logger(
        subsystem: Bundle.main.bundleIdentifier!,
        category: "pepper"
    )

    private static let tokenSize = 64
    private static let maxSurfaces = 4
    private static let maxSide = 8192
    /// A surface's memory, at most: maxSide² BGRA, plus room for row padding.
    private static let maxSurfaceBytes = 4 * maxSide * maxSide + (1 << 20)
    /// How long a box keeps the replaced stream's last frame (see `attach`)
    /// if the new stream presents nothing.
    private static let keptFrameLimit: TimeInterval = 2
    private static let maxStreams = 16
    /// The most streams one process has: pepper-agent's viewer shows at most
    /// 4 pages, and each pepper-play has one. Others can still attach while
    /// one process has all it may.
    private static let maxStreamsPerOwner = 4
    private static let maxPlacementsPerView = 4
    private static let maxPlacements = 64
    private static let bufferSize = 1024
    private static let noticeSize = 256
    /// The most messages handled before the rest of the main queue runs.
    private static let drainBatch = 64
    /// The most messages waiting on the port (the kernel's default is 5):
    /// room for every stream's frames through a moment's stall of the main
    /// thread.
    private static let queueLimit: mach_port_msgcount_t = 256
    /// The layer each view's boxes go in.
    private static let containerName = "pepper"
    /// What shells get as `PEPPER_FEATURES`.
    private static let features = "holes"

    /// bootstrap_check_in isn't in any Swift module, so it's looked up.
    private typealias BootstrapCheckIn = @convention(c) (
        mach_port_t,
        UnsafePointer<CChar>,
        UnsafeMutablePointer<mach_port_t>
    ) -> kern_return_t

    private static let bootstrapCheckIn: BootstrapCheckIn? = {
        let rtldDefault = UnsafeMutableRawPointer(bitPattern: -2)
        guard let sym = dlsym(rtldDefault, "bootstrap_check_in") else { return nil }
        return unsafeBitCast(sym, to: BootstrapCheckIn.self)
    }()

    /// A process, by its audit token: its pid, and which program that pid
    /// ran.
    private struct Sender: Hashable {
        var pid: UInt32
        var version: UInt32
    }

    /// Frames handed over by one client.
    private struct Stream {
        var surfaces: [IOSurface]
        /// Our send right to the client's port; one user reference per stream.
        var client: mach_port_t
        /// The process that attached it.
        var owner: Sender
        var slot: Int?
        /// What the client was last told of its box.
        var reported: BoxSize?
        /// Which attach made it, to tell it from a later one on the token.
        var serial = 0
    }

    /// Where a token's frames show.
    private struct Placement {
        weak var view: Ghostty.SurfaceView?
        /// The box. Its frames show in it, or, when it has holes, in
        /// `pictures`.
        let layer: CALayer
        /// When it was made, to find a view's oldest.
        let order: Int
        /// The box's size in pixels.
        var pixels: CGSize = .zero
        /// With holes, a picture of the whole box in each piece of it that
        /// shows, clipped to its piece (none when the holes cover it all);
        /// nil without.
        var pictures: [CALayer]?
        /// What the layers were last cut for, so the same place again
        /// keeps them.
        var cut: Cut?
        /// The frame its layers show, or keep showing.
        var shown: IOSurface?
    }

    /// The pieces of a box that show, from its top left in pixels, at its
    /// size and scale.
    private struct Cut: Equatable {
        var size: CGSize
        var pieces: [CGRect]
        var scale: CGFloat
    }

    private struct BoxSize: Equatable {
        var width: UInt32
        var height: UInt32
        var visible: Bool
        var display: UInt32
    }

    private var port = mach_port_t(MACH_PORT_NULL)
    /// Where dead-name notices come: there's no send right to it, so only the
    /// kernel's send-once rights reach it.
    private var notifyPort = mach_port_t(MACH_PORT_NULL)
    private var source: DispatchSourceMachReceive?
    private var notifySource: DispatchSourceMachReceive?
    private let header: UnsafeMutablePointer<mach_msg_header_t> = UnsafeMutableRawPointer
        .allocate(byteCount: bufferSize, alignment: 8)
        .bindMemory(to: mach_msg_header_t.self, capacity: 1)
    private let notice: UnsafeMutablePointer<mach_msg_header_t> = UnsafeMutableRawPointer
        .allocate(byteCount: noticeSize, alignment: 8)
        .bindMemory(to: mach_msg_header_t.self, capacity: 1)

    private var streams: [String: Stream] = [:]
    private var placements: [String: Placement] = [:]
    private var placementsMade = 0
    private var streamsMade = 0
    /// The processes that have streams, each watched, so that its streams end
    /// when it exits or runs another program.
    private var owners: [Sender: DispatchSourceProcess] = [:]
    /// The view of each box that shows its stream, by token, as
    /// `videoDidChange` last told.
    private var videoBoxes: [String: ObjectIdentifier] = [:]

    /// Posted when the boxes that show a stream change, or their views: their
    /// tabs show it.
    static let videoDidChange = Notification.Name("PepperVideoDidChange")

    /// Whether a stream shows in `view`.
    func showsVideo(in view: Ghostty.SurfaceView) -> Bool {
        videoBoxes.contains { token, id in
            id == ObjectIdentifier(view) && placements[token]?.view === view
        }
    }

    /// Whether the token's box shows its stream: a frame in it, or the last
    /// one, kept while the stream attaches again at a new size. A box its
    /// holes cover all of still does: its video plays on under them.
    private func showsVideo(_ token: String, _ placement: Placement) -> Bool {
        placement.view != nil && streams[token] != nil && placement.shown != nil
    }

    /// Posts `videoDidChange` if the boxes that show a stream changed, or
    /// the views they're in. Called on the way out of every way in
    /// (messages, notices, placing, an owner's end, a window's move, a kept
    /// frame's end), so after every frame: it compares without allocating,
    /// and lists them again only when they changed.
    private func noteVideo() {
        var showing = 0
        for (token, placement) in placements where showsVideo(token, placement) {
            guard let view = placement.view, videoBoxes[token] == ObjectIdentifier(view) else {
                showing = -1
                break
            }
            showing += 1
        }
        guard showing != videoBoxes.count else { return }
        videoBoxes = [:]
        for (token, placement) in placements where showsVideo(token, placement) {
            if let view = placement.view { videoBoxes[token] = ObjectIdentifier(view) }
        }
        NotificationCenter.default.post(name: Self.videoDidChange, object: self)
    }

    /// Registers the port and listens on the main queue. Call it before
    /// ghostty_init: the core keeps a copy of the environment, taken then,
    /// that every shell's is made from, so the variables set here must be
    /// in it. Set later, they'd be missing from shells, and the copy would
    /// point into an `environ` array that unsetenv shifted (a null where
    /// it expects a variable) or setenv moved.
    func start() {
        guard source == nil else { return }
        // What Pepper was started with isn't its own: no shell gets it, even
        // if checking in fails.
        unsetenv("PEPPER_PORT")
        unsetenv("PEPPER_PLAY")
        unsetenv("PEPPER_FEATURES")
        guard let checkIn = Self.bootstrapCheckIn else {
            Self.logger.error("bootstrap_check_in not found")
            return
        }
        let allocated = mach_port_allocate(
            mach_task_self_,
            mach_port_right_t(MACH_PORT_RIGHT_RECEIVE),
            &notifyPort
        )
        guard allocated == KERN_SUCCESS else {
            Self.logger.error("mach_port_allocate failed: \(allocated)")
            return
        }

        let name = "\(Bundle.main.bundleIdentifier ?? "pepper").\(getpid())"
        let kr = checkIn(bootstrap_port, name, &port)
        guard kr == KERN_SUCCESS else {
            Self.logger.error("bootstrap_check_in(\(name, privacy: .public)) failed: \(kr)")
            return
        }
        var limits = mach_port_limits_t(mpl_qlimit: Self.queueLimit)
        let limited = withUnsafeMutablePointer(to: &limits) {
            $0.withMemoryRebound(to: integer_t.self, capacity: 1) {
                mach_port_set_attributes(
                    mach_task_self_,
                    port,
                    MACH_PORT_LIMITS_INFO,
                    $0,
                    mach_msg_type_number_t(
                        MemoryLayout<mach_port_limits_t>.size / MemoryLayout<natural_t>.size
                    )
                )
            }
        }
        if limited != KERN_SUCCESS {
            Self.logger.warning("the port's queue stays at the default: \(limited)")
        }
        setenv("PEPPER_PORT", name, 1)
        setenv("PEPPER_FEATURES", Self.features, 1)
        let play = Bundle.main.bundleURL
            .appendingPathComponent("Contents/Helpers/Pepper.app/Contents/MacOS/pepper-play")
        if FileManager.default.isExecutableFile(atPath: play.path) {
            setenv("PEPPER_PLAY", play.path, 1)
        }

        let source = DispatchSource.makeMachReceiveSource(port: port, queue: .main)
        source.setEventHandler { [weak self] in self?.drain() }
        source.resume()
        self.source = source
        let notifySource = DispatchSource.makeMachReceiveSource(port: notifyPort, queue: .main)
        notifySource.setEventHandler { [weak self] in self?.drainNotices() }
        notifySource.resume()
        self.notifySource = notifySource
        // A window going to another display, of another scale: its boxes
        // are other sizes in pixels, and their clients are told.
        for name in [NSWindow.didChangeBackingPropertiesNotification, NSWindow.didChangeScreenNotification] {
            NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] note in
                guard let window = note.object as? NSWindow else { return }
                self?.windowMoved(window)
            }
        }
        Self.logger.info("listening on \(name, privacy: .public)")
    }

    // MARK: Placing

    /// Shows the token's stream over `pixels` (from the surface's top left)
    /// of `view`, but not over `holes` (pixels from the surface's top left
    /// too), or takes it away when `pixels` is empty. Called on the main
    /// thread for `OSC 777;pepper`.
    func place(token: String, pixels: CGRect, holes: [CGRect] = [], in view: Ghostty.SurfaceView) {
        defer { noteVideo() }
        guard !pixels.isEmpty else {
            if let placement = placements.removeValue(forKey: token) {
                remove(placement.layer)
                report(token)
            }
            return
        }
        guard let host = view.layer else { return }

        var placement = placements[token]
        if placement?.view !== view {
            // New, or moving from another view: a layer of its own here.
            if let old = placements.removeValue(forKey: token) { remove(old.layer) }
            pruneGoneViews()
            makeRoom(in: view)
            guard placements.count < Self.maxPlacements else {
                Self.logger.warning("place: too many placements")
                report(token)
                return
            }
            let layer = CALayer()
            layer.contentsGravity = .resizeAspect
            // The host layer's origin is its bottom left; this keeps the
            // box's distance from the top as the surface resizes.
            layer.autoresizingMask = [.layerMinYMargin]
            placementsMade += 1
            placement = Placement(view: view, layer: layer, order: placementsMade)
            placements[token] = placement
        }
        guard var current = placement else { return }
        current.pixels = pixels.size
        // The frame showing; while the stream attaches again, the one kept.
        if let surface = currentSurface(token) {
            current.shown = surface
        } else if streams[token] == nil {
            current.shown = nil
        }
        let layer = current.layer

        let scale = host.contentsScale
        let cut = Cut(
            size: pixels.size,
            pieces: Self.pieces(of: pixels.size, holes: holes.map { $0.offsetBy(dx: -pixels.minX, dy: -pixels.minY) }),
            scale: scale
        )
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        let box = container(in: host)
        layer.frame = CGRect(
            x: pixels.minX / scale,
            y: box.bounds.height - pixels.maxY / scale,
            width: pixels.width / scale,
            height: pixels.height / scale
        )
        if layer.superlayer !== box { box.addSublayer(layer) }
        if cut != current.cut {
            current.pictures = makePictures(in: layer, for: cut)
            current.cut = cut
        }
        placements[token] = current
        showFrame(current.shown, in: current)
        CATransaction.commit()
        report(token)
    }

    /// `place`, for the core's action.
    func place(_ action: ghostty_action_pepper_place_s, in view: Ghostty.SurfaceView) {
        guard let token = action.token else { return }
        place(
            token: String(cString: token),
            pixels: CGRect(
                x: CGFloat(action.x),
                y: CGFloat(action.y),
                width: CGFloat(action.width),
                height: CGFloat(action.height)
            ),
            holes: Self.holes(of: action),
            in: view
        )
    }

    /// The action's holes: at most its list's length, whatever `len` says.
    static func holes(of action: ghostty_action_pepper_place_s) -> [CGRect] {
        guard let list = action.holes?.pointee else { return [] }
        return withUnsafeBytes(of: list.list) { raw in
            raw.assumingMemoryBound(to: ghostty_action_pepper_box_s.self)
                .prefix(Int(list.len))
                .map { CGRect(x: CGFloat($0.x), y: CGFloat($0.y), width: CGFloat($0.width), height: CGFloat($0.height)) }
        }
    }

    /// Lays out the box `layer` for `cut`. Returns nil when no hole is in
    /// the box, which then shows its frames itself; otherwise the pictures
    /// of the whole box it now holds, one in each piece that shows, clipped
    /// to its piece. Call it inside a transaction.
    private func makePictures(in layer: CALayer, for cut: Cut) -> [CALayer]? {
        layer.sublayers = nil
        let box = CGRect(origin: .zero, size: cut.size)
        guard cut.pieces != [box] else { return nil }
        // Its pieces show the frames now.
        layer.contents = nil
        let scale = cut.scale
        return cut.pieces.map { piece in
            // In points from the box's bottom left, as the layer has them.
            let clip = CALayer()
            clip.masksToBounds = true
            clip.frame = CGRect(
                x: piece.minX / scale,
                y: (box.height - piece.maxY) / scale,
                width: piece.width / scale,
                height: piece.height / scale
            )
            let picture = CALayer()
            picture.contentsGravity = .resizeAspect
            picture.frame = CGRect(
                x: -clip.frame.minX,
                y: -clip.frame.minY,
                width: box.width / scale,
                height: box.height / scale
            )
            clip.addSublayer(picture)
            layer.addSublayer(clip)
            return picture
        }
    }

    /// The parts of a box of `size` that no hole covers, as rectangles: the
    /// box is cut into bands at the holes' tops and bottoms, each band where
    /// holes cross it, and a piece joins the one above when their sides line
    /// up. Whole pixels in, whole pixels out, so the pieces meet with no
    /// seam.
    static func pieces(of size: CGSize, holes: [CGRect]) -> [CGRect] {
        let box = CGRect(origin: .zero, size: size)
        let cuts = holes.map { $0.intersection(box) }.filter { !$0.isEmpty }
        guard !cuts.isEmpty else { return [box] }
        let edges = Set([0, size.height] + cuts.flatMap { [$0.minY, $0.maxY] }).sorted()
        var pieces: [CGRect] = []
        // The pieces that reach the bottom of the band before.
        var open: [Int] = []
        for (top, bottom) in zip(edges, edges.dropFirst()) {
            var spans: [(left: CGFloat, right: CGFloat)] = []
            var x: CGFloat = 0
            for cut in cuts.filter({ $0.minY < bottom && $0.maxY > top }).sorted(by: { $0.minX < $1.minX }) {
                if cut.minX > x { spans.append((x, cut.minX)) }
                x = max(x, cut.maxX)
            }
            if x < size.width { spans.append((x, size.width)) }
            var reaching: [Int] = []
            for span in spans {
                if let i = open.first(where: { pieces[$0].minX == span.left && pieces[$0].maxX == span.right }) {
                    pieces[i].size.height = bottom - pieces[i].minY
                    reaching.append(i)
                } else {
                    reaching.append(pieces.count)
                    pieces.append(CGRect(x: span.left, y: top, width: span.right - span.left, height: bottom - top))
                }
            }
            open = reaching
        }
        return pieces
    }

    /// A window went to another display, or its scale changed: its boxes
    /// keep their size in points, so their pixels follow the scale, and each
    /// client hears of its box's pixels and display.
    private func windowMoved(_ window: NSWindow) {
        defer { noteVideo() }
        let scale = window.backingScaleFactor
        // Read again at each: a report can end a client's streams, and their
        // placements with them.
        let tokens = placements.filter { $0.value.view?.window === window }.map(\.key)
        for token in tokens {
            guard var placement = placements[token] else { continue }
            placement.pixels = CGSize(
                width: (placement.layer.frame.width * scale).rounded(),
                height: (placement.layer.frame.height * scale).rounded()
            )
            // Cut for the old scale: the next place cuts it again.
            placement.cut = nil
            placements[token] = placement
            report(token)
        }
    }

    /// The layer a view's boxes go in: the view's size, clipping them, so a
    /// box never shows outside its view whatever it says. Call it inside a
    /// transaction.
    private func container(in host: CALayer) -> CALayer {
        if let layer = host.sublayers?.first(where: { $0.name == Self.containerName }) {
            if layer.frame != host.bounds { layer.frame = host.bounds }
            return layer
        }
        let layer = CALayer()
        layer.name = Self.containerName
        layer.masksToBounds = true
        layer.autoresizingMask = [.layerWidthSizable, .layerHeightSizable]
        layer.frame = host.bounds
        host.addSublayer(layer)
        return layer
    }

    /// Makes room for one more box in `view`: past `maxPlacementsPerView`,
    /// its oldest box with no stream goes, or its oldest if all have one, so
    /// output can't push a playing video out with boxes of its own.
    private func makeRoom(in view: Ghostty.SurfaceView) {
        let here = placements.filter { $0.value.view === view }
        guard here.count >= Self.maxPlacementsPerView else { return }
        let idle = here.filter { streams[$0.key] == nil }
        guard let oldest = (idle.isEmpty ? here : idle)
            .min(by: { $0.value.order < $1.value.order }) else { return }
        placements.removeValue(forKey: oldest.key)
        remove(oldest.value.layer)
        report(oldest.key)
        Self.logger.info("place: a view's oldest box went, to make room")
    }

    private func remove(_ layer: CALayer) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.removeFromSuperlayer()
        CATransaction.commit()
    }

    private func pruneGoneViews() {
        for (token, placement) in placements where placement.view == nil {
            placements.removeValue(forKey: token)
            report(token)
        }
    }

    private func currentSurface(_ token: String) -> IOSurface? {
        guard let stream = streams[token], let slot = stream.slot else { return nil }
        return stream.surfaces[slot]
    }

    /// Puts the token's current frame (or nothing) in its placement's layers.
    /// A placement whose view has gone goes instead.
    private func show(_ token: String) {
        guard let placement = placements[token] else { return }
        guard placement.view != nil else {
            placements.removeValue(forKey: token)
            remove(placement.layer)
            report(token)
            return
        }
        // In place: this runs at every frame.
        let surface = currentSurface(token)
        placements[token]?.shown = surface
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        showFrame(surface, in: placement)
        CATransaction.commit()
    }

    /// Puts `surface` (or nothing) in the layers of `placement` that show
    /// its frames. Call it inside a transaction.
    private func showFrame(_ surface: IOSurface?, in placement: Placement) {
        if let pictures = placement.pictures {
            for picture in pictures { picture.contents = surface }
        } else {
            placement.layer.contents = surface
        }
    }

    // MARK: Streaming

    private func drain() {
        defer { noteVideo() }
        for _ in 0..<Self.drainBatch {
            let kr = mach_msg(
                header,
                MACH_RCV_MSG | MACH_RCV_TIMEOUT | Self.receiveAudit,
                0,
                mach_msg_size_t(Self.bufferSize),
                port,
                0,
                mach_port_t(MACH_PORT_NULL)
            )
            switch kr {
            case KERN_SUCCESS:
                handle()
                // Frees every right the message still carries; the
                // IOSurfaces we looked up hold their own references, and
                // `attach` takes the client's right out of the header.
                mach_msg_destroy(header)
            case MACH_RCV_TIMED_OUT:
                return
            case MACH_RCV_TOO_LARGE:
                // Without MACH_RCV_LARGE the kernel has already dropped it.
                Self.logger.warning("dropped a message over \(Self.bufferSize) bytes")
            case _ where kr & ~MACH_MSG_MASK == MACH_RCV_BODY_ERROR:
                // The header came out with its reply right, and maybe part of
                // the body (mach/message.h): freed, or a resource shortage
                // would leave a sender's rights here.
                Self.logger.warning("a message's body couldn't be received: \(kr)")
                mach_msg_destroy(header)
            default:
                Self.logger.error("mach_msg receive failed: \(kr)")
                return
            }
        }
        // More are waiting: let the rest of the main queue (keys, frames) run
        // first.
        DispatchQueue.main.async { [weak self] in self?.drain() }
    }

    /// The kernel's notices, on `notifyPort`.
    private func drainNotices() {
        defer { noteVideo() }
        while true {
            let kr = mach_msg(
                notice,
                MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                0,
                mach_msg_size_t(Self.noticeSize),
                notifyPort,
                0,
                mach_port_t(MACH_PORT_NULL)
            )
            if kr == MACH_RCV_TOO_LARGE { continue }
            guard kr == KERN_SUCCESS else { return }
            let headerSize = MemoryLayout<mach_msg_header_t>.size
            // The dead name is after the header and an 8-byte NDR record.
            // The others: port-deleted (a stream ended first and its client's
            // name went with it) and send-once (a client attached again, and
            // its earlier request, given back, was let go) need nothing.
            if notice.pointee.msgh_id == Self.notifyDeadName,
               Int(notice.pointee.msgh_size) >= headerSize + 12 {
                clientDied(UnsafeRawPointer(notice).loadUnaligned(
                    fromByteOffset: headerSize + 8,
                    as: mach_port_t.self
                ))
            }
            mach_msg_destroy(notice)
        }
    }

    private func handle() {
        let raw = UnsafeRawPointer(header)
        let size = Int(header.pointee.msgh_size)
        let id = header.pointee.msgh_id
        guard let sender = Self.sender(raw, size) else {
            Self.logger.warning("message \(id) without its sender")
            return
        }

        var offset = MemoryLayout<mach_msg_header_t>.size
        var ports: [mach_port_t] = []
        if header.pointee.msgh_bits & UInt32(MACH_MSGH_BITS_COMPLEX) != 0 {
            guard offset + MemoryLayout<mach_msg_body_t>.size <= size else { return }
            let count = raw.load(fromByteOffset: offset, as: mach_msg_body_t.self).msgh_descriptor_count
            offset += MemoryLayout<mach_msg_body_t>.size
            guard count <= Self.maxSurfaces else { return }
            for _ in 0..<count {
                guard offset + MemoryLayout<mach_msg_port_descriptor_t>.size <= size else { return }
                let descriptor = raw.load(fromByteOffset: offset, as: mach_msg_port_descriptor_t.self)
                // Send rights only (as received, MACH_MSG_TYPE_PORT_SEND).
                // Other kinds of descriptor have other sizes, and other
                // rights aren't surfaces: either refuses the message.
                guard descriptor.type == mach_msg_descriptor_type_t(MACH_MSG_PORT_DESCRIPTOR),
                      descriptor.disposition == mach_msg_type_name_t(MACH_MSG_TYPE_MOVE_SEND)
                else { return }
                ports.append(descriptor.name)
                offset += MemoryLayout<mach_msg_port_descriptor_t>.size
            }
        }

        guard offset + Self.tokenSize <= size, let token = Self.token(raw + offset) else {
            Self.logger.warning("message \(id) without a valid token")
            return
        }
        offset += Self.tokenSize

        switch MessageID(rawValue: id) {
        case .attach:
            attach(token, ports, from: sender)

        case .present:
            guard offset + MemoryLayout<UInt32>.size <= size else { return }
            present(token, Int(raw.loadUnaligned(fromByteOffset: offset, as: UInt32.self)), from: sender)

        case .detach:
            guard streams[token]?.owner == sender else { return }
            dropStream(token)
            show(token)

        case .hello:
            hello(token)

        case nil:
            // Notification ids too: the kernel sends those to notifyPort.
            Self.logger.warning("unknown message id \(id)")
        }
    }

    /// The token at `p`: 1–64 of [A-Za-z0-9_-], NUL padded.
    private static func token(_ p: UnsafeRawPointer) -> String? {
        let bytes = UnsafeRawBufferPointer(start: p, count: tokenSize)
        let length = bytes.firstIndex(of: 0) ?? tokenSize
        guard length > 0 else { return nil }
        for byte in bytes[..<length] {
            switch byte {
            case UInt8(ascii: "a")...UInt8(ascii: "z"),
                 UInt8(ascii: "A")...UInt8(ascii: "Z"),
                 UInt8(ascii: "0")...UInt8(ascii: "9"),
                 UInt8(ascii: "-"), UInt8(ascii: "_"):
                continue
            default:
                return nil
            }
        }
        return String(decoding: bytes[..<length], as: UTF8.self)
    }

    /// Who sent the message of `size` bytes at `raw`, from the audit trailer
    /// the kernel put after it (audit_token_t's 6th and 8th words: the pid
    /// and its version).
    private static func sender(_ raw: UnsafeRawPointer, _ size: Int) -> Sender? {
        let trailer = (size + 3) & ~3
        guard trailer + auditTrailerSize <= bufferSize,
              raw.loadUnaligned(fromByteOffset: trailer, as: UInt32.self) == 0,
              raw.loadUnaligned(fromByteOffset: trailer + 4, as: UInt32.self) >= auditTrailerSize
        else { return nil }
        let audit = trailer + auditTokenOffset
        return Sender(
            pid: raw.loadUnaligned(fromByteOffset: audit + 20, as: UInt32.self),
            version: raw.loadUnaligned(fromByteOffset: audit + 28, as: UInt32.self)
        )
    }

    /// A surface as pepper-play makes them: BGRA, not planar, 4 bytes a
    /// pixel, at most `maxSide` pixels a side, rows that hold them, and the
    /// memory that takes.
    private static func usable(_ surface: IOSurface) -> Bool {
        surface.pixelFormat == 0x4247_5241 // 'BGRA'
            && surface.planeCount == 0
            && surface.bytesPerElement == 4
            && (1...maxSide).contains(surface.width)
            && (1...maxSide).contains(surface.height)
            && surface.bytesPerRow >= 4 * surface.width
            && surface.allocationSize >= surface.bytesPerRow * surface.height
            && surface.allocationSize <= maxSurfaceBytes
    }

    private func attach(_ token: String, _ ports: [mach_port_t], from sender: Sender) {
        // The client's port, as a send right: it tells us when the client dies.
        let client = header.pointee.msgh_remote_port
        let remoteType = header.pointee.msgh_bits & 0x1f
        guard client != MACH_PORT_NULL, remoteType == UInt32(MACH_MSG_TYPE_MOVE_SEND) else {
            refuse(from: sender, "no client port")
            return
        }
        guard clientOnly(client) else {
            // Not told: the port is one Pepper holds for something else.
            refuse(from: sender, "the client's port is one Pepper holds already")
            return
        }
        // From here the client is told, so it can offer its surfaces again.
        let refused = { (why: String) in
            self.refuse(from: sender, why)
            _ = self.send(Self.refusedID, token: token, to: client)
        }
        if let stream = streams[token], stream.owner != sender {
            refused("the token's stream is another process's")
            return
        }
        let surfaces = ports.compactMap { IOSurfaceLookupFromMachPort($0) }
        guard !ports.isEmpty, surfaces.count == ports.count else {
            refused("no surfaces, or a port that isn't one")
            return
        }
        guard surfaces.allSatisfy(Self.usable) else {
            refused("a surface isn't BGRA, or is too big")
            return
        }
        // A new stream must fit; an owner's attach on its token replaces one.
        if streams[token] == nil {
            guard streams.count < Self.maxStreams else {
                refused("too many streams")
                return
            }
            guard streams.values.filter({ $0.owner == sender }).count < Self.maxStreamsPerOwner else {
                refused("the process has all the streams it may")
                return
            }
        }

        var previous = mach_port_t(MACH_PORT_NULL)
        let kr = mach_port_request_notification(
            mach_task_self_,
            client,
            Self.notifyDeadName,
            0,
            notifyPort,
            mach_msg_type_name_t(MACH_MSG_TYPE_MAKE_SEND_ONCE),
            &previous
        )
        guard kr == KERN_SUCCESS else {
            // Most likely the client is already gone.
            refuse(from: sender, "dead-name request failed: \(kr)")
            return
        }
        // Only an earlier attach's request can come back (`clientOnly`).
        if previous != MACH_PORT_NULL { mach_port_deallocate(mach_task_self_, previous) }

        // Keep the client's right past mach_msg_destroy.
        header.pointee.msgh_remote_port = mach_port_t(MACH_PORT_NULL)
        streamsMade += 1
        let serial = streamsMade
        let stream = Stream(surfaces: surfaces, client: client, owner: sender, slot: nil, serial: serial)
        // The owner's own stream on the token goes (not by `dropStream`,
        // which would stop watching the owner).
        if let replaced = streams.updateValue(stream, forKey: token) {
            mach_port_deallocate(mach_task_self_, replaced.client)
        }
        watch(sender)
        // Not shown yet: the box keeps the last frame of the stream this
        // replaces (its client attaches again at a new size) until the first
        // `present`, rather than going blank between them; the same owner's
        // frame (checked above). Not for long: if this stream presents
        // nothing, the box empties.
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.keptFrameLimit) { [weak self] in
            guard let self, let stream = self.streams[token],
                  stream.serial == serial, stream.slot == nil else { return }
            self.show(token)
            self.noteVideo()
        }
        report(token)
        Self.logger.info("attached \(surfaces.count) surfaces")
    }

    /// An attach not taken, logged as an error with the sender's pid, so a
    /// full stream table shows.
    private func refuse(from sender: Sender, _ why: String) {
        Self.logger.error("attach from pid \(sender.pid) refused: \(why, privacy: .public)")
    }

    /// Answers a hello on its reply port, a send-once right: the kernel tells
    /// the client who sent the answer, so it knows this process holds the
    /// port's name, not one that took the name of a Pepper that quit.
    private func hello(_ token: String) {
        let reply = header.pointee.msgh_remote_port
        let replyType = header.pointee.msgh_bits & 0x1f
        guard reply != MACH_PORT_NULL, replyType == UInt32(MACH_MSG_TYPE_MOVE_SEND_ONCE) else { return }
        let kr = send(
            Self.helloAnswerID,
            token: token,
            to: reply,
            disposition: mach_msg_type_name_t(MACH_MSG_TYPE_MOVE_SEND_ONCE)
        )
        // Sent, the answer took the right; if not, mach_msg_destroy frees it.
        if kr == KERN_SUCCESS { header.pointee.msgh_remote_port = mach_port_t(MACH_PORT_NULL) }
    }

    /// Watches the owner's process from its first stream on: when it exits
    /// or runs another program, its streams end, whatever their client port
    /// does (a port can outlive the process that named it).
    private func watch(_ owner: Sender) {
        guard owners[owner] == nil else { return }
        let source = DispatchSource.makeProcessSource(
            identifier: pid_t(bitPattern: owner.pid),
            eventMask: [.exit, .exec],
            queue: .main
        )
        source.setEventHandler { [weak self] in self?.ownerGone(owner) }
        // The kernel watches whichever process has the pid when this is
        // registered: if that isn't the owner (it exited first, and the pid
        // may be another program's now), the owner is gone already. A pid
        // gone before then makes the event come at once.
        source.setRegistrationHandler { [weak self] in
            if Self.running(owner) == false { self?.ownerGone(owner) }
        }
        owners[owner] = source
        source.resume()
    }

    /// The owner exited or ran another program: its streams go, and their
    /// boxes, which nothing will show in again.
    private func ownerGone(_ owner: Sender) {
        defer { noteVideo() }
        var ended = 0
        for (token, stream) in streams where stream.owner == owner {
            dropStream(token)
            if let placement = placements.removeValue(forKey: token) {
                remove(placement.layer)
            }
            ended += 1
        }
        // A watch whose streams had gone already.
        owners.removeValue(forKey: owner)?.cancel()
        if ended > 0 {
            Self.logger.info("pid \(owner.pid) is gone: its \(ended) streams too")
        }
    }

    /// Whether the owner still runs, by the audit token of its pid's process
    /// now; nil if that can't be told (the pid is another user's).
    private static func running(_ owner: Sender) -> Bool? {
        let pid = pid_t(bitPattern: owner.pid)
        var task = mach_port_name_t(MACH_PORT_NULL)
        guard task_name_for_pid(mach_task_self_, pid, &task) == KERN_SUCCESS else {
            return kill(pid, 0) == -1 && errno == ESRCH ? false : nil
        }
        defer { mach_port_deallocate(mach_task_self_, task) }
        var token = audit_token_t()
        var count = mach_msg_type_number_t(MemoryLayout<audit_token_t>.size / MemoryLayout<natural_t>.size)
        let kr = withUnsafeMutablePointer(to: &token) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                task_info(task, task_flavor_t(TASK_AUDIT_TOKEN), $0, &count)
            }
        }
        guard kr == KERN_SUCCESS else { return nil }
        return token.val.5 == owner.pid && token.val.7 == owner.version
    }

    /// Whether the client's name is the streams' alone: a send right with
    /// one user reference per stream on it, plus the one this attach brought,
    /// and no request on it but a stream's. A name this process holds for
    /// anything else (its own port, a service a framework talks to) is
    /// refused: its references and requests are others'.
    private func clientOnly(_ client: mach_port_t) -> Bool {
        var type = mach_port_type_t(0)
        var refs = mach_port_urefs_t(0)
        guard mach_port_type(mach_task_self_, client, &type) == KERN_SUCCESS,
              mach_port_get_refs(
                  mach_task_self_,
                  client,
                  mach_port_right_t(MACH_PORT_RIGHT_SEND),
                  &refs
              ) == KERN_SUCCESS
        else { return false }
        let streamsOnIt = streams.values.filter { $0.client == client }.count
        let allowed = Self.portTypeSend | (streamsOnIt > 0 ? Self.portTypeDeadNameRequest : 0)
        return type & Self.portTypeSend != 0
            && type & ~allowed == 0
            && Int(refs) == streamsOnIt + 1
    }

    private func present(_ token: String, _ slot: Int, from sender: Sender) {
        guard var stream = streams[token], stream.owner == sender,
              stream.surfaces.indices.contains(slot) else { return }
        stream.slot = slot
        streams[token] = stream
        show(token)
    }

    /// Ends the token's stream. Its box may still show the stream's last
    /// frame: every caller clears or removes the box after.
    private func dropStream(_ token: String) {
        guard let stream = streams.removeValue(forKey: token) else { return }
        mach_port_deallocate(mach_task_self_, stream.client)
        // Its owner's last: that process needn't be watched any more.
        if !streams.values.contains(where: { $0.owner == stream.owner }) {
            owners.removeValue(forKey: stream.owner)?.cancel()
        }
    }

    /// The client is gone: its streams go, and their boxes, which nothing
    /// will show in again.
    private func clientGone(_ client: mach_port_t) {
        for (token, stream) in streams where stream.client == client {
            dropStream(token)
            if let placement = placements.removeValue(forKey: token) {
                remove(placement.layer)
            }
        }
    }

    // MARK: Answering

    /// Tells the token's client its box's size, if that changed since it was
    /// last told: the placement's, or the last one, not visible, once the
    /// placement is gone.
    private func report(_ token: String) {
        guard var stream = streams[token] else { return }
        let size: BoxSize
        if let placement = placements[token], placement.view != nil {
            size = BoxSize(
                width: UInt32(placement.pixels.width),
                height: UInt32(placement.pixels.height),
                visible: true,
                display: placement.view?.window?.screen?.displayID ?? 0
            )
        } else if let last = stream.reported {
            size = BoxSize(width: last.width, height: last.height, visible: false, display: last.display)
        } else {
            return
        }
        guard size != stream.reported else { return }
        switch send(
            Self.sizeID,
            token: token,
            values: [size.width, size.height, size.visible ? 1 : 0, size.display],
            to: stream.client
        ) {
        case KERN_SUCCESS:
            stream.reported = size
            streams[token] = stream
        case MACH_SEND_INVALID_DEST:
            // Dead before its dead-name notice came.
            clientGone(stream.client)
        default:
            break
        }
    }

    /// Sends a client a message, the token then `values`, without waiting:
    /// a client whose queue is full, or that's gone, misses it.
    private func send(
        _ id: mach_msg_id_t,
        token: String,
        values: [UInt32] = [],
        to client: mach_port_t,
        disposition: mach_msg_type_name_t = mach_msg_type_name_t(MACH_MSG_TYPE_COPY_SEND)
    ) -> kern_return_t {
        let headerSize = MemoryLayout<mach_msg_header_t>.size
        let byteCount = headerSize + Self.tokenSize + values.count * MemoryLayout<UInt32>.size
        let kr = withUnsafeTemporaryAllocation(byteCount: byteCount, alignment: 8) { buffer in
            guard let raw = buffer.baseAddress else { return KERN_FAILURE }
            raw.initializeMemory(as: UInt8.self, repeating: 0, count: byteCount)
            var header = mach_msg_header_t()
            header.msgh_bits = mach_msg_bits_t(disposition)
            header.msgh_size = mach_msg_size_t(byteCount)
            header.msgh_remote_port = client
            header.msgh_id = id
            raw.storeBytes(of: header, as: mach_msg_header_t.self)
            let bytes = Array(token.utf8.prefix(Self.tokenSize))
            bytes.withUnsafeBytes { token in
                if let base = token.baseAddress {
                    (raw + headerSize).copyMemory(from: base, byteCount: token.count)
                }
            }
            var offset = headerSize + Self.tokenSize
            for value in values {
                raw.storeBytes(of: value, toByteOffset: offset, as: UInt32.self)
                offset += MemoryLayout<UInt32>.size
            }
            return mach_msg(
                raw.assumingMemoryBound(to: mach_msg_header_t.self),
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                mach_msg_size_t(byteCount),
                0,
                mach_port_t(MACH_PORT_NULL),
                0,
                mach_port_t(MACH_PORT_NULL)
            )
        }
        if kr != KERN_SUCCESS {
            Self.logger.warning("message \(id) to a client failed: \(kr)")
        }
        return kr
    }

    /// A dead-name notice, which only the kernel can send (`notifyPort`).
    private func clientDied(_ name: mach_port_t) {
        var type = mach_port_type_t(0)
        guard mach_port_type(mach_task_self_, name, &type) == KERN_SUCCESS,
              type & Self.portTypeDeadName != 0 else { return }
        clientGone(name)
        // The notice's own reference: the kernel adds one to the dead name
        // for each dead-name notice it sends.
        mach_port_deallocate(mach_task_self_, name)
    }
}
