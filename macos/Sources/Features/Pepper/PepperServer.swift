import AppKit
import IOSurface
import OSLog
import QuartzCore

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
///   view.
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
///   attach it again.
/// - present: the token, then a `UInt32` slot; shows that surface.
/// - detach: the token; ends the stream.
///
/// Pepper answers on the client's port, never waiting for it:
/// - size: the token, then the box's width and height in pixels and whether
///   it's placed, as `UInt32`s. Sent when a token's stream and placement
///   both exist, and when the placement moves, resizes or goes, so the
///   client draws its frames at the box's size.
///
/// The kernel's notices that a client died come on a port of their own, which
/// no other process can send to.
///
/// Every shell gets the port's name as `PEPPER_PORT`, and pepper-play's path
/// (the player in `Contents/Helpers`) as `PEPPER_PLAY`.
final class PepperServer {
    static let shared = PepperServer()

    enum MessageID: mach_msg_id_t {
        case attach = 0x7070_0001
        case present = 0x7070_0002
        case detach = 0x7070_0003
    }

    /// The size message, to a client.
    private static let sizeID: mach_msg_id_t = 0x7070_0101

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
    private static let maxStreams = 16
    private static let maxPlacementsPerView = 4
    private static let maxPlacements = 64
    private static let bufferSize = 1024
    private static let noticeSize = 256
    /// The most messages handled before the rest of the main queue runs.
    private static let drainBatch = 64
    /// The layer each view's boxes go in.
    private static let containerName = "pepper"

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
    private struct Sender: Equatable {
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
    }

    /// Where a token's frames show.
    private struct Placement {
        weak var view: Ghostty.SurfaceView?
        let layer: CALayer
        /// When it was made, to find a view's oldest.
        let order: Int
        /// The box's size in pixels.
        var pixels: CGSize = .zero
    }

    private struct BoxSize: Equatable {
        var width: UInt32
        var height: UInt32
        var visible: Bool
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

    /// Registers the port and listens on the main queue. Call it before any
    /// surface starts, so every shell gets PEPPER_PORT.
    func start() {
        guard source == nil else { return }
        // What Pepper was started with isn't its own: no shell gets it, even
        // if checking in fails.
        unsetenv("PEPPER_PORT")
        unsetenv("PEPPER_PLAY")
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
        setenv("PEPPER_PORT", name, 1)
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
        Self.logger.info("listening on \(name, privacy: .public)")
    }

    // MARK: Placing

    /// Shows the token's stream over `pixels` (from the surface's top left)
    /// of `view`, or takes it away when `pixels` is empty. Called on the
    /// main thread for `OSC 777;pepper`.
    func place(token: String, pixels: CGRect, in view: Ghostty.SurfaceView) {
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
        placements[token] = current
        let layer = current.layer

        let scale = host.contentsScale
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
        layer.contents = currentSurface(token)
        CATransaction.commit()
        report(token)
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
    /// its oldest goes.
    private func makeRoom(in view: Ghostty.SurfaceView) {
        let here = placements.filter { $0.value.view === view }
        guard here.count >= Self.maxPlacementsPerView,
              let oldest = here.min(by: { $0.value.order < $1.value.order }) else { return }
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

    /// Puts the token's current frame (or nothing) in its placement's layer.
    private func show(_ token: String) {
        guard let placement = placements[token] else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        placement.layer.contents = currentSurface(token)
        CATransaction.commit()
    }

    // MARK: Streaming

    private func drain() {
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

    /// A surface as pepper-play makes them: BGRA, not planar, at most
    /// `maxSide` pixels a side.
    private static func usable(_ surface: IOSurface) -> Bool {
        surface.pixelFormat == 0x4247_5241 // 'BGRA'
            && surface.planeCount == 0
            && (1...maxSide).contains(surface.width)
            && (1...maxSide).contains(surface.height)
    }

    private func attach(_ token: String, _ ports: [mach_port_t], from sender: Sender) {
        if let stream = streams[token], stream.owner != sender {
            Self.logger.warning("attach: the token's stream is another process's")
            return
        }
        // The client's port, as a send right: it tells us when the client dies.
        let client = header.pointee.msgh_remote_port
        let remoteType = header.pointee.msgh_bits & 0x1f
        guard client != MACH_PORT_NULL, remoteType == UInt32(MACH_MSG_TYPE_MOVE_SEND) else {
            Self.logger.warning("attach without a client port")
            return
        }
        guard !ports.isEmpty else { return }
        let surfaces = ports.compactMap { IOSurfaceLookupFromMachPort($0) }
        guard surfaces.count == ports.count else {
            Self.logger.warning("attach with a port that isn't an IOSurface")
            return
        }
        guard surfaces.allSatisfy(Self.usable) else {
            Self.logger.warning("attach: a surface isn't BGRA, or is too big")
            return
        }
        guard streams[token] != nil || streams.count < Self.maxStreams else {
            Self.logger.warning("attach: too many streams")
            return
        }
        guard clientOnly(client) else {
            Self.logger.warning("attach: the client's port is one Pepper holds already")
            return
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
            Self.logger.warning("attach: dead-name request failed: \(kr)")
            return
        }
        // Only an earlier attach's request can come back (`clientOnly`).
        if previous != MACH_PORT_NULL { mach_port_deallocate(mach_task_self_, previous) }

        // Keep the client's right past mach_msg_destroy.
        header.pointee.msgh_remote_port = mach_port_t(MACH_PORT_NULL)
        dropStream(token)
        streams[token] = Stream(surfaces: surfaces, client: client, owner: sender, slot: nil)
        show(token)
        report(token)
        Self.logger.info("attached \(surfaces.count) surfaces")
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

    private func dropStream(_ token: String) {
        guard let stream = streams.removeValue(forKey: token) else { return }
        mach_port_deallocate(mach_task_self_, stream.client)
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
                visible: true
            )
        } else if let last = stream.reported {
            size = BoxSize(width: last.width, height: last.height, visible: false)
        } else {
            return
        }
        guard size != stream.reported else { return }
        switch send(size, token: token, to: stream.client) {
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

    /// Sends the size message without waiting: a client whose queue is full,
    /// or that's gone, misses it.
    private func send(_ size: BoxSize, token: String, to client: mach_port_t) -> kern_return_t {
        let headerSize = MemoryLayout<mach_msg_header_t>.size
        let byteCount = headerSize + Self.tokenSize + 3 * MemoryLayout<UInt32>.size
        let kr = withUnsafeTemporaryAllocation(byteCount: byteCount, alignment: 8) { buffer in
            guard let raw = buffer.baseAddress else { return KERN_FAILURE }
            raw.initializeMemory(as: UInt8.self, repeating: 0, count: byteCount)
            var header = mach_msg_header_t()
            header.msgh_bits = mach_msg_bits_t(MACH_MSG_TYPE_COPY_SEND)
            header.msgh_size = mach_msg_size_t(byteCount)
            header.msgh_remote_port = client
            header.msgh_id = Self.sizeID
            raw.storeBytes(of: header, as: mach_msg_header_t.self)
            let bytes = Array(token.utf8.prefix(Self.tokenSize))
            bytes.withUnsafeBytes { token in
                if let base = token.baseAddress {
                    (raw + headerSize).copyMemory(from: base, byteCount: token.count)
                }
            }
            var offset = headerSize + Self.tokenSize
            for value in [size.width, size.height, size.visible ? 1 : 0] {
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
            Self.logger.warning("size to client failed: \(kr)")
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
