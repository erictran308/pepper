import AppKit
import IOSurface
import OSLog
import QuartzCore

/// pepper: shows video frames that another process hands over as IOSurfaces,
/// in a layer over a terminal surface, so macOS composites them with no copies.
///
/// Spike version: one stream at a time, placed over the focused surface, taken
/// from any process in this login session. The escape sequence and its token
/// come next (CLAUDE.md, "Placing them").
///
/// The port's name is in `PEPPER_PORT` for every shell. Messages, by msgh_id:
/// - attach: 1–4 IOSurface ports as port descriptors, then `Attach` inline.
/// - present: a `UInt32` slot inline; shows that attach's surface.
/// - detach: removes the layer.
final class PepperServer {
    static let shared = PepperServer()

    enum MessageID: mach_msg_id_t {
        case attach = 0x7070_0001
        case present = 0x7070_0002
        case detach = 0x7070_0003
    }

    /// The inline part of an attach message, after the port descriptors.
    struct Attach {
        /// The box, in points from the surface's top left corner.
        var x: Float32
        var y: Float32
        var width: Float32
        var height: Float32
    }

    private static let logger = Logger(
        subsystem: Bundle.main.bundleIdentifier!,
        category: "pepper"
    )

    private static let maxSurfaces = 4
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

    private var port = mach_port_t(MACH_PORT_NULL)
    private var source: DispatchSourceMachReceive?
    private let header: UnsafeMutablePointer<mach_msg_header_t> = UnsafeMutableRawPointer
        .allocate(byteCount: bufferSize, alignment: 8)
        .bindMemory(to: mach_msg_header_t.self, capacity: 1)

    private var surfaces: [IOSurface] = []
    private var layer: CALayer?

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

        let source = DispatchSource.makeMachReceiveSource(port: port, queue: .main)
        source.setEventHandler { [weak self] in self?.drain() }
        source.resume()
        self.source = source
        Self.logger.info("listening on \(name, privacy: .public)")
    }

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
                // Frees every right the message carried; the IOSurfaces we
                // looked up hold their own references.
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

        let inline = raw + offset
        let inlineSize = size - offset
        switch MessageID(rawValue: header.pointee.msgh_id) {
        case .attach:
            guard !ports.isEmpty, inlineSize >= MemoryLayout<Attach>.size else { return }
            let surfaces = ports.compactMap { IOSurfaceLookupFromMachPort($0) }
            guard surfaces.count == ports.count else { return }
            attach(surfaces, inline.loadUnaligned(as: Attach.self))

        case .present:
            guard inlineSize >= MemoryLayout<UInt32>.size else { return }
            present(Int(inline.loadUnaligned(as: UInt32.self)))

        case .detach:
            detach()

        case nil:
            Self.logger.warning("unknown message id \(self.header.pointee.msgh_id)")
        }
    }

    private func attach(_ surfaces: [IOSurface], _ box: Attach) {
        detach()
        // The key window, or the frontmost terminal window when pepper isn't
        // the active app (keyWindow is nil then).
        let controller = NSApp.keyWindow?.windowController as? BaseTerminalController
            ?? NSApp.orderedWindows.lazy.compactMap { $0.windowController as? BaseTerminalController }.first
        guard let view = controller?.focusedSurface, let host = view.layer else {
            Self.logger.warning("attach: no focused surface")
            return
        }

        let layer = CALayer()
        layer.contentsGravity = .resizeAspect
        // The host layer's origin is its bottom left; this keeps the box's
        // distance from the top as the surface resizes.
        layer.autoresizingMask = [.layerMinYMargin]

        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.frame = CGRect(
            x: CGFloat(box.x),
            y: host.bounds.height - CGFloat(box.y + box.height),
            width: CGFloat(box.width),
            height: CGFloat(box.height)
        )
        layer.contents = surfaces[0]
        host.addSublayer(layer)
        CATransaction.commit()

        self.surfaces = surfaces
        self.layer = layer
        Self.logger.info("attached \(surfaces.count) surfaces")
    }

    private func present(_ slot: Int) {
        guard let layer, surfaces.indices.contains(slot) else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.contents = surfaces[slot]
        CATransaction.commit()
    }

    private func detach() {
        guard let layer else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.removeFromSuperlayer()
        CATransaction.commit()
        self.layer = nil
        surfaces = []
    }
}
