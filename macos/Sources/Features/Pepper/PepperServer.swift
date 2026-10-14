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
/// - Streaming: a process sends IOSurfaces over the Mach port named in
///   `PEPPER_PORT`, with the same token. Frames show only where that token
///   has been placed.
///
/// Messages, by msgh_id; each inline part starts with the token, NUL padded
/// to 64 bytes:
/// - attach: 1–4 IOSurface ports as port descriptors, then the token. The
///   reply port (msgh_local_port, made a send right) is the client: when it
///   dies, its streams go.
/// - present: the token, then a `UInt32` slot; shows that surface.
/// - detach: the token; ends the stream.
///
/// Pepper answers on the client's port, never waiting for it:
/// - size: the token, then the box's width and height in pixels and whether
///   it's placed, as `UInt32`s. Sent when a token's stream and placement
///   both exist, and when the placement moves, resizes or goes, so the
///   client draws its frames at the box's size.
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

    /// The kernel's notifications (mach/notify.h).
    private static let notifyPortDeleted: mach_msg_id_t = 0o101
    private static let notifySendOnce: mach_msg_id_t = 0o107
    private static let notifyDeadName: mach_msg_id_t = 0o110

    /// MACH_PORT_TYPE_DEAD_NAME, a macro Swift doesn't import.
    private static let portTypeDeadName: mach_port_type_t = 1 << (MACH_PORT_RIGHT_DEAD_NAME + 16)

    private static let logger = Logger(
        subsystem: Bundle.main.bundleIdentifier!,
        category: "pepper"
    )

    private static let tokenSize = 64
    private static let maxSurfaces = 4
    private static let maxStreams = 16
    private static let maxPlacements = 16
    private static let bufferSize = 1024

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

    /// Frames handed over by one client.
    private struct Stream {
        var surfaces: [IOSurface]
        /// Our send right to the client's port; one user reference per stream.
        var client: mach_port_t
        var slot: Int?
        /// What the client was last told of its box.
        var reported: BoxSize?
    }

    /// Where a token's frames show.
    private struct Placement {
        weak var view: Ghostty.SurfaceView?
        let layer: CALayer
        /// The box's size in pixels.
        var pixels: CGSize = .zero
    }

    private struct BoxSize: Equatable {
        var width: UInt32
        var height: UInt32
        var visible: Bool
    }

    private var port = mach_port_t(MACH_PORT_NULL)
    private var source: DispatchSourceMachReceive?
    private let header: UnsafeMutablePointer<mach_msg_header_t> = UnsafeMutableRawPointer
        .allocate(byteCount: bufferSize, alignment: 8)
        .bindMemory(to: mach_msg_header_t.self, capacity: 1)

    private var streams: [String: Stream] = [:]
    private var placements: [String: Placement] = [:]

    /// Registers the port and listens on the main queue. Call it before any
    /// surface starts, so every shell gets PEPPER_PORT.
    func start() {
        guard source == nil else { return }
        guard let checkIn = Self.bootstrapCheckIn else {
            Self.logger.error("bootstrap_check_in not found")
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
            if let old = placement { remove(old.layer) }
            pruneGoneViews()
            guard placements.count < Self.maxPlacements else {
                Self.logger.warning("place: too many placements")
                return
            }
            let layer = CALayer()
            layer.contentsGravity = .resizeAspect
            // The host layer's origin is its bottom left; this keeps the
            // box's distance from the top as the surface resizes.
            layer.autoresizingMask = [.layerMinYMargin]
            placement = Placement(view: view, layer: layer)
            placements[token] = placement
        }
        guard var current = placement else { return }
        current.pixels = pixels.size
        placements[token] = current
        let layer = current.layer

        let scale = host.contentsScale
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.frame = CGRect(
            x: pixels.minX / scale,
            y: host.bounds.height - pixels.maxY / scale,
            width: pixels.width / scale,
            height: pixels.height / scale
        )
        if layer.superlayer !== host { host.addSublayer(layer) }
        layer.contents = currentSurface(token)
        CATransaction.commit()
        report(token)
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
        while true {
            let kr = mach_msg(
                header,
                MACH_RCV_MSG | MACH_RCV_TIMEOUT,
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
    }

    private func handle() {
        let raw = UnsafeRawPointer(header)
        let size = Int(header.pointee.msgh_size)
        let id = header.pointee.msgh_id

        switch id {
        case Self.notifyDeadName:
            // The client's name is after the header and an 8-byte NDR record.
            guard size >= MemoryLayout<mach_msg_header_t>.size + 12 else { return }
            clientDied(raw.loadUnaligned(fromByteOffset: MemoryLayout<mach_msg_header_t>.size + 8, as: mach_port_t.self))
            return
        case Self.notifyPortDeleted:
            // A stream ended first and its client's name went with it.
            return
        case Self.notifySendOnce:
            // A client attached again: its earlier dead-name request, given
            // back by mach_port_request_notification, was let go.
            return
        default:
            break
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
                // Other kinds of descriptor have other sizes; refuse them all.
                guard descriptor.type == mach_msg_descriptor_type_t(MACH_MSG_PORT_DESCRIPTOR) else { return }
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
            attach(token, ports)

        case .present:
            guard offset + MemoryLayout<UInt32>.size <= size else { return }
            present(token, Int(raw.loadUnaligned(fromByteOffset: offset, as: UInt32.self)))

        case .detach:
            dropStream(token)
            show(token)

        case nil:
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

    private func attach(_ token: String, _ ports: [mach_port_t]) {
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
        guard streams[token] != nil || streams.count < Self.maxStreams else {
            Self.logger.warning("attach: too many streams")
            return
        }

        var previous = mach_port_t(MACH_PORT_NULL)
        let kr = mach_port_request_notification(
            mach_task_self_,
            client,
            Self.notifyDeadName,
            0,
            port,
            mach_msg_type_name_t(MACH_MSG_TYPE_MAKE_SEND_ONCE),
            &previous
        )
        guard kr == KERN_SUCCESS else {
            // Most likely the client is already gone.
            Self.logger.warning("attach: dead-name request failed: \(kr)")
            return
        }
        if previous != MACH_PORT_NULL { mach_port_deallocate(mach_task_self_, previous) }

        // Keep the client's right past mach_msg_destroy.
        header.pointee.msgh_remote_port = mach_port_t(MACH_PORT_NULL)
        dropStream(token)
        streams[token] = Stream(surfaces: surfaces, client: client, slot: nil)
        show(token)
        report(token)
        Self.logger.info("attached \(surfaces.count) surfaces")
    }

    private func present(_ token: String, _ slot: Int) {
        guard var stream = streams[token], stream.surfaces.indices.contains(slot) else { return }
        stream.slot = slot
        streams[token] = stream
        show(token)
    }

    private func dropStream(_ token: String) {
        guard let stream = streams.removeValue(forKey: token) else { return }
        mach_port_deallocate(mach_task_self_, stream.client)
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
        guard size != stream.reported, send(size, token: token, to: stream.client) else { return }
        stream.reported = size
        streams[token] = stream
    }

    /// Sends the size message without waiting: a client whose queue is full,
    /// or that's gone, misses it.
    private func send(_ size: BoxSize, token: String, to client: mach_port_t) -> Bool {
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
        return kr == KERN_SUCCESS
    }

    private func clientDied(_ name: mach_port_t) {
        // Anyone holding our port can send this message, so check the name
        // really is dead.
        var type = mach_port_type_t(0)
        guard mach_port_type(mach_task_self_, name, &type) == KERN_SUCCESS,
              type & Self.portTypeDeadName != 0 else { return }

        for (token, stream) in streams where stream.client == name {
            dropStream(token)
            show(token)
        }
        // The notification's own reference to the dead name.
        mach_port_deallocate(mach_task_self_, name)
    }
}
