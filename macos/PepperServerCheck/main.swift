// pepper: checks PepperServer.swift on its own, without the app. This
// program runs the server, under a name of its own, and copies of itself as
// its clients, each a process of its own (`owner`, `orphan`, `keeper`,
// `hello`). run.sh builds and runs it.
import AppKit
import IOSurface

// MARK: Mach

private typealias Bootstrap = @convention(c) (
    mach_port_t, UnsafePointer<CChar>, UnsafeMutablePointer<mach_port_t>
) -> kern_return_t

private func bootstrap(_ symbol: String) -> Bootstrap {
    unsafeBitCast(dlsym(UnsafeMutableRawPointer(bitPattern: -2), symbol)!, to: Bootstrap.self)
}

func lookUp(_ name: String) -> mach_port_t {
    var port = mach_port_t(MACH_PORT_NULL)
    let kr = bootstrap("bootstrap_look_up")(bootstrap_port, name, &port)
    precondition(kr == KERN_SUCCESS, "bootstrap_look_up(\(name)): \(kr)")
    return port
}

func checkIn(_ name: String) -> mach_port_t {
    var port = mach_port_t(MACH_PORT_NULL)
    let kr = bootstrap("bootstrap_check_in")(bootstrap_port, name, &port)
    precondition(kr == KERN_SUCCESS, "bootstrap_check_in(\(name)): \(kr)")
    return port
}

func newPort() -> mach_port_t {
    var port = mach_port_t(MACH_PORT_NULL)
    precondition(mach_port_allocate(mach_task_self_, MACH_PORT_RIGHT_RECEIVE, &port) == KERN_SUCCESS)
    return port
}

/// This process's pid version, from its audit token.
func ownVersion() -> UInt32 {
    var task = mach_port_name_t(MACH_PORT_NULL)
    precondition(task_name_for_pid(mach_task_self_, getpid(), &task) == KERN_SUCCESS)
    var token = audit_token_t()
    var count = mach_msg_type_number_t(MemoryLayout<audit_token_t>.size / MemoryLayout<natural_t>.size)
    let kr = withUnsafeMutablePointer(to: &token) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            task_info(task, task_flavor_t(TASK_AUDIT_TOKEN), $0, &count)
        }
    }
    precondition(kr == KERN_SUCCESS)
    return token.val.7
}

let msgAttach: Int32 = 0x7070_0001
let msgHello: Int32 = 0x7070_0004
let msgSize: Int32 = 0x7070_0101
let msgRefused: Int32 = 0x7070_0102
let msgHelloAnswer: Int32 = 0x7070_0103

/// A message's bytes, laid out as PepperServer reads them.
struct Writer {
    var bytes: [UInt8] = []

    mutating func u32(_ value: UInt32) {
        withUnsafeBytes(of: value) { bytes += $0 }
    }

    mutating func header(bits: UInt32, remote: mach_port_t, local: mach_port_t, id: Int32) {
        for word in [bits, 0, remote, local, 0, UInt32(bitPattern: id)] { u32(word) }
    }

    mutating func token(_ token: String) {
        let utf8 = Array(token.utf8.prefix(64))
        bytes += utf8 + Array(repeating: 0, count: 64 - utf8.count)
    }

    /// Sends it, with its size set.
    mutating func send() -> kern_return_t {
        let size = UInt32(bytes.count)
        withUnsafeBytes(of: size) { bytes.replaceSubrange(4..<8, with: $0) }
        return bytes.withUnsafeMutableBytes { raw in
            mach_msg(
                raw.baseAddress!.assumingMemoryBound(to: mach_msg_header_t.self),
                MACH_SEND_MSG | MACH_SEND_TIMEOUT, size, 0, 0, 1000, 0
            )
        }
    }
}

/// Surfaces made here, kept while the program runs.
var surfaces: [IOSurface] = []

/// A 64x64 BGRA surface's port, of `bytesPerElement` bytes a pixel.
func surfacePort(bytesPerElement: Int) -> mach_port_t? {
    guard let surface = IOSurface(properties: [
        .width: 64, .height: 64, .bytesPerElement: bytesPerElement, .pixelFormat: 0x4247_5241,
    ]) else { return nil }
    surfaces.append(surface)
    return IOSurfaceCreateMachPort(surface)
}

func attach(_ token: String, to server: mach_port_t, reply: mach_port_t, replyBits: UInt32, surface: mach_port_t) -> kern_return_t {
    var message = Writer()
    message.header(
        bits: UInt32(MACH_MSG_TYPE_COPY_SEND) | (replyBits << 8) | 0x8000_0000,
        remote: server, local: reply, id: msgAttach
    )
    message.u32(1)
    // A port descriptor: the name, two pads, MOVE_SEND, a port descriptor.
    message.u32(surface)
    message.u32(0)
    message.bytes += [0, 0, UInt8(MACH_MSG_TYPE_MOVE_SEND), 0]
    message.token(token)
    return message.send()
}

struct Received {
    let id: Int32
    let token: String
    let values: [UInt32]
    let pid: UInt32
    let version: UInt32
}

func receive(_ port: mach_port_t, timeoutMs: UInt32) -> Received? {
    let capacity = 1024
    let buffer = UnsafeMutableRawPointer.allocate(byteCount: capacity, alignment: 8)
    defer { buffer.deallocate() }
    let header = buffer.bindMemory(to: mach_msg_header_t.self, capacity: 1)
    let kr = mach_msg(
        header, MACH_RCV_MSG | MACH_RCV_TIMEOUT | (3 << 24), 0,
        mach_msg_size_t(capacity), port, timeoutMs, 0
    )
    guard kr == KERN_SUCCESS else { return nil }
    defer { mach_msg_destroy(header) }
    let size = Int(header.pointee.msgh_size)
    let headerSize = MemoryLayout<mach_msg_header_t>.size
    var token = ""
    var values: [UInt32] = []
    if size >= headerSize + 64 {
        let bytes = UnsafeRawBufferPointer(start: buffer + headerSize, count: 64)
        token = String(decoding: bytes.prefix { $0 != 0 }, as: UTF8.self)
        var offset = headerSize + 64
        while offset + 4 <= size {
            values.append(buffer.loadUnaligned(fromByteOffset: offset, as: UInt32.self))
            offset += 4
        }
    }
    let audit = ((size + 3) & ~3) + 20
    return Received(
        id: header.pointee.msgh_id, token: token, values: values,
        pid: buffer.loadUnaligned(fromByteOffset: audit + 20, as: UInt32.self),
        version: buffer.loadUnaligned(fromByteOffset: audit + 28, as: UInt32.self)
    )
}

func say(_ line: String) {
    print(line)
    fflush(stdout)
}

func describe(_ message: Received) -> String {
    switch message.id {
    case msgSize: return (["size", message.token] + message.values.map { String($0) }).joined(separator: " ")
    case msgRefused: return "refused \(message.token)"
    case msgHelloAnswer: return "answer \(message.pid) \(message.version)"
    default: return "message \(message.id)"
    }
}

func pepper() -> mach_port_t {
    lookUp(ProcessInfo.processInfo.environment["PEPPER_PORT"]!)
}

// MARK: Clients

/// `owner [--keeper NAME] [--bpe N] TOKEN...`: attaches each token, with its
/// own port as the client (or the keeper's), then says what the server sends
/// it, and on stdin takes `die-port` (its port goes, it stays).
func owner(_ arguments: [String]) -> Never {
    var arguments = arguments
    var keeper: String?
    var bytesPerElement = 4
    while let first = arguments.first, first.hasPrefix("--") {
        arguments.removeFirst()
        let value = arguments.removeFirst()
        if first == "--keeper" { keeper = value } else { bytesPerElement = Int(value)! }
    }
    let server = pepper()
    let port = newPort()
    let reply = keeper.map { (lookUp($0), UInt32(MACH_MSG_TYPE_COPY_SEND)) }
        ?? (port, UInt32(MACH_MSG_TYPE_MAKE_SEND))
    for token in arguments {
        guard let surface = surfacePort(bytesPerElement: bytesPerElement) else {
            say("no surface \(token)")
            continue
        }
        say("sent \(token) \(attach(token, to: server, reply: reply.0, replyBits: reply.1, surface: surface))")
    }
    let lock = NSLock()
    var portGone = false
    Thread.detachNewThread {
        while let line = readLine(), line == "die-port" {
            lock.lock()
            portGone = true
            mach_port_mod_refs(mach_task_self_, port, MACH_PORT_RIGHT_RECEIVE, -1)
            lock.unlock()
            say("port-gone")
        }
        exit(0)
    }
    while true {
        lock.lock()
        let gone = portGone
        lock.unlock()
        if gone {
            Thread.sleep(forTimeInterval: 0.05)
        } else if let message = receive(port, timeoutMs: 50) {
            say(describe(message))
        }
    }
}

/// `orphan KEEPER TOKEN`: attaches the token with the keeper's port as the
/// client, a port that outlives this process, and exits.
func orphan(_ keeper: String, _ token: String) -> Never {
    let surface = surfacePort(bytesPerElement: 4)!
    let kr = attach(token, to: pepper(), reply: lookUp(keeper), replyBits: UInt32(MACH_MSG_TYPE_COPY_SEND), surface: surface)
    say("sent \(token) \(kr)")
    exit(0)
}

/// `keeper NAME`: holds a port by that name, and says what comes to it.
func keeper(_ name: String) -> Never {
    let port = checkIn(name)
    Thread.detachNewThread {
        while readLine() != nil {}
        exit(0)
    }
    say("ready")
    while true {
        if let message = receive(port, timeoutMs: 100) { say(describe(message)) }
    }
}

/// `hello`: says hello, and who answered.
func hello() -> Never {
    let port = newPort()
    var message = Writer()
    message.header(
        bits: UInt32(MACH_MSG_TYPE_COPY_SEND) | (UInt32(MACH_MSG_TYPE_MAKE_SEND_ONCE) << 8),
        remote: pepper(), local: port, id: msgHello
    )
    message.token("hello")
    let kr = message.send()
    say("sent \(kr); " + (receive(port, timeoutMs: 3000).map(describe) ?? "none"))
    exit(0)
}

// MARK: The checks

final class Child {
    let process = Process()
    private let input = Pipe()
    private let output = Pipe()
    private let lock = NSLock()
    private var lines: [String] = []

    init(_ arguments: [String]) {
        process.executableURL = URL(fileURLWithPath: CommandLine.arguments[0])
        process.arguments = arguments
        process.standardInput = input
        process.standardOutput = output
        do {
            try process.run()
        } catch {
            fatalError("can't run a client: \(error)")
        }
        let reading = output.fileHandleForReading
        Thread.detachNewThread { [weak self] in
            var pending = Data()
            while true {
                let data = reading.availableData
                if data.isEmpty { return }
                pending += data
                while let newline = pending.firstIndex(of: 10) {
                    let line = String(decoding: pending[pending.startIndex..<newline], as: UTF8.self)
                    pending.removeSubrange(pending.startIndex...newline)
                    self?.append(line)
                }
            }
        }
    }

    private func append(_ line: String) {
        lock.lock()
        lines.append(line)
        lock.unlock()
    }

    func saw(_ prefix: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return lines.contains { $0.hasPrefix(prefix) }
    }

    /// Whether it says a line that starts so within `seconds`.
    @discardableResult
    func await(_ prefix: String, _ seconds: TimeInterval = 3) -> Bool {
        let deadline = Date().addingTimeInterval(seconds)
        while !saw(prefix) && Date() < deadline { Thread.sleep(forTimeInterval: 0.01) }
        return saw(prefix)
    }

    func tell(_ line: String) {
        input.fileHandleForWriting.write(Data((line + "\n").utf8))
    }

    func end() {
        try? input.fileHandleForWriting.close()
        process.waitUntilExit()
    }
}

var failures = 0

func check(_ name: String, _ ok: Bool) {
    say((ok ? "ok - " : "FAIL - ") + name)
    if !ok { failures += 1 }
}

func onMain<T>(_ work: () -> T) -> T {
    DispatchQueue.main.sync(execute: work)
}

func makeView() -> Ghostty.SurfaceView {
    let view = Ghostty.SurfaceView(frame: NSRect(x: 0, y: 0, width: 400, height: 300))
    view.layer = CALayer()
    view.wantsLayer = true
    return view
}

func place(_ token: String, in view: Ghostty.SurfaceView, width: CGFloat = 100, height: CGFloat = 50) {
    onMain {
        PepperServer.shared.place(token: token, pixels: CGRect(x: 0, y: 0, width: width, height: height), in: view)
    }
}

func pause(_ seconds: TimeInterval) {
    Thread.sleep(forTimeInterval: seconds)
}

func checks() {
    let hello = Child(["hello"])
    hello.end()
    check("a hello is answered, by the server's own process", hello.saw("sent 0; answer \(getpid()) \(ownVersion())"))

    // One process gets so many streams, and hears of the next; another
    // still gets one.
    let full = Child(["owner", "a1", "a2", "a3", "a4", "a5"])
    check("a process's fifth stream is refused, and it's told", full.await("refused a5"))
    check("its first four are taken", !(1...4).contains { full.saw("refused a\($0)") })
    let other = Child(["owner", "b1"])
    other.await("sent b1")
    let views = onMain { (makeView(), makeView()) }
    place("b1", in: views.0)
    check("another process still gets a stream", other.await("size b1 100 50 1") && !other.saw("refused"))
    place("a1", in: views.1)
    check("the full process's streams are there", full.await("size a1 100 50 1"))
    full.end()
    other.end()

    // A stream whose client port is a port that outlives its process.
    let keepName = "pepper.server-check.keep.\(getpid())"
    let keeper = Child(["keeper", keepName])
    keeper.await("ready")
    let alive = Child(["owner", "--keeper", keepName, "k1"])
    alive.await("sent k1")
    let keepViews = onMain { (makeView(), makeView()) }
    place("k1", in: keepViews.0)
    check("a stream with another's port as its client shows while its process runs", keeper.await("size k1 100 50 1"))
    let orphan = Child(["orphan", keepName, "k2"])
    orphan.process.waitUntilExit()
    pause(0.3)
    place("k2", in: keepViews.1)
    pause(0.5)
    check("a stream ends with its process though its client port lives on", !keeper.saw("size k2"))
    alive.end()
    pause(0.3)
    place("k1", in: keepViews.0, width: 120, height: 60)
    pause(0.5)
    check("a running stream ends when its process exits", !keeper.saw("size k1 120 60"))
    keeper.end()

    // Output in a view takes a box for room: one with no stream first.
    let playing = Child(["owner", "m1"])
    playing.await("sent m1")
    let roomView = onMain { makeView() }
    place("m1", in: roomView)
    playing.await("size m1 100 50 1")
    for junk in ["j1", "j2", "j3", "j4"] { place(junk, in: roomView) }
    pause(0.3)
    check("output pushes out a box with no stream before a playing one", !playing.saw("size m1 100 50 0"))
    playing.end()

    // A client's port dies, and its window moves before the server hears.
    let moved = Child(["owner", "w1", "w2"])
    moved.await("sent w2")
    let (window, wa, wb) = onMain { () -> (NSWindow, Ghostty.SurfaceView, Ghostty.SurfaceView) in
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 800, height: 600),
            styleMask: [.borderless], backing: .buffered, defer: true
        )
        let (a, b) = (makeView(), makeView())
        window.contentView?.addSubview(a)
        window.contentView?.addSubview(b)
        return (window, a, b)
    }
    place("w1", in: wa)
    place("w2", in: wb)
    check("boxes in a window are told", moved.await("size w1 100 50 1") && moved.await("size w2 100 50 1"))
    onMain {
        moved.tell("die-port")
        moved.await("port-gone")
        NotificationCenter.default.post(name: NSWindow.didChangeScreenNotification, object: window)
    }
    let after = Child(["owner", "w2"])
    after.await("sent w2")
    pause(0.5)
    check("a box its client's end took away stays away when its window moves", !after.saw("size w2"))
    after.end()
    moved.end()
    _ = window

    // A surface the server can't show.
    let odd = Child(["owner", "--bpe", "1", "u1"])
    if odd.await("no surface u1", 1) {
        say("# skip: IOSurface makes no BGRA surface of 1 byte a pixel")
    } else {
        check("a surface of 1 byte a pixel is refused", odd.await("refused u1"))
    }
    odd.end()
}

// MARK: Main

let arguments = Array(CommandLine.arguments.dropFirst())
switch arguments.first {
case "owner": owner(Array(arguments.dropFirst()))
case "orphan": orphan(arguments[1], arguments[2])
case "keeper": keeper(arguments[1])
case "hello": hello()
default: break
}

_ = NSApplication.shared
NSApp.setActivationPolicy(.prohibited)
PepperServer.shared.start()
guard let name = getenv("PEPPER_PORT").map({ String(cString: $0) }) else {
    say("FAIL - the server didn't start")
    exit(1)
}
say("# the server is \(name)")
Thread.detachNewThread {
    checks()
    say(failures == 0 ? "# all passed" : "# \(failures) failed")
    exit(failures == 0 ? 0 : 1)
}
// The main thread runs the main queue (AppKit wants it), with a timer so
// the run loop has something to wait on.
Timer.scheduledTimer(withTimeInterval: 3600, repeats: true) { _ in }
RunLoop.main.run()
