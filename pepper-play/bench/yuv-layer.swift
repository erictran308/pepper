// Does CoreAnimation show a YUV IOSurface as CALayer.contents correctly?
// (player-perf 10.7, the raw decoder path.) A window with the same six sRGB
// patches over mid grey twice: left a BGRA IOSurface tagged sRGB, right an
// NV12 one ('420v', BT.709 limited range, the attachments VideoToolbox puts
// on its frames). Screenshot it: the two halves should measure the same.
//
//   swiftc -O bench/yuv-layer.swift -o /tmp/yuv-layer && /tmp/yuv-layer
import AppKit
import CoreVideo
import IOSurface
import QuartzCore

let width = 480, height = 360
// (r, g, b) patches in a 3×2 grid over grey 128.
let patches: [(Int, Int, Int)] = [
    (255, 0, 0), (0, 255, 0), (0, 0, 255),
    (0, 255, 255), (255, 0, 255), (255, 255, 0),
]

func color(x: Int, y: Int) -> (Int, Int, Int) {
    let cw = width / 3, ch = height / 2
    let (cx, cy) = (x / cw, y / ch)
    let (lx, ly) = (x % cw, y % ch)
    if lx > cw / 6, lx < cw * 5 / 6, ly > ch / 6, ly < ch * 5 / 6 {
        return patches[cy * 3 + cx]
    }
    return (128, 128, 128)
}

func bgraSurface() -> IOSurface {
    let surface = IOSurface(properties: [
        .width: width, .height: height, .bytesPerElement: 4,
        .pixelFormat: kCVPixelFormatType_32BGRA,
    ])!
    surface.lock(options: [], seed: nil)
    let base = surface.baseAddress.assumingMemoryBound(to: UInt8.self)
    for y in 0..<height {
        for x in 0..<width {
            let (r, g, b) = color(x: x, y: y)
            let p = base + y * surface.bytesPerRow + x * 4
            p[0] = UInt8(b); p[1] = UInt8(g); p[2] = UInt8(r); p[3] = 255
        }
    }
    surface.unlock(options: [], seed: nil)
    IOSurfaceSetValue(unsafeBitCast(surface, to: IOSurfaceRef.self),
                      kIOSurfaceColorSpace, CGColorSpace.sRGB as CFString)
    return surface
}

/// BT.709 limited range, as the clips are encoded.
func ycbcr(_ c: (Int, Int, Int)) -> (Double, Double, Double) {
    let (r, g, b) = (Double(c.0) / 255, Double(c.1) / 255, Double(c.2) / 255)
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b
    let cb = (b - y) / 1.8556, cr = (r - y) / 1.5748
    return (16 + 219 * y, 128 + 224 * cb, 128 + 224 * cr)
}

func nv12Surface() -> IOSurface {
    var buffer: CVPixelBuffer?
    CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                        [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer)
    let pb = buffer!
    CVBufferSetAttachment(pb, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, .shouldPropagate)
    CVBufferSetAttachment(pb, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
    CVBufferSetAttachment(pb, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, .shouldPropagate)
    CVPixelBufferLockBaseAddress(pb, [])
    let yBase = CVPixelBufferGetBaseAddressOfPlane(pb, 0)!.assumingMemoryBound(to: UInt8.self)
    let yRow = CVPixelBufferGetBytesPerRowOfPlane(pb, 0)
    let cBase = CVPixelBufferGetBaseAddressOfPlane(pb, 1)!.assumingMemoryBound(to: UInt8.self)
    let cRow = CVPixelBufferGetBytesPerRowOfPlane(pb, 1)
    for y in 0..<height {
        for x in 0..<width {
            yBase[y * yRow + x] = UInt8(ycbcr(color(x: x, y: y)).0.rounded())
        }
    }
    for y in 0..<height / 2 {
        for x in 0..<width / 2 {
            let (_, cb, cr) = ycbcr(color(x: x * 2, y: y * 2))
            cBase[y * cRow + x * 2] = UInt8(cb.rounded())
            cBase[y * cRow + x * 2 + 1] = UInt8(cr.rounded())
        }
    }
    CVPixelBufferUnlockBaseAddress(pb, [])
    let surface = CVPixelBufferGetIOSurface(pb)!.takeUnretainedValue()
    return unsafeBitCast(surface, to: IOSurface.self)
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(contentRect: NSRect(x: 200, y: 200, width: width, height: height / 2),
                      styleMask: [.titled, .closable], backing: .buffered, defer: false)
window.title = "pepper yuv-layer: BGRA left, NV12 right"
let view = NSView(frame: window.contentLayoutRect)
view.wantsLayer = true
window.contentView = view
let bgra = bgraSurface(), nv12 = nv12Surface()
for (i, surface) in [bgra, nv12].enumerated() {
    let layer = CALayer()
    layer.frame = CGRect(x: CGFloat(i * width / 2), y: 0, width: CGFloat(width / 2), height: CGFloat(height / 2))
    layer.contentsGravity = .resize
    layer.contents = surface
    view.layer!.addSublayer(layer)
}
print("BGRA \(bgra.pixelFormat), NV12 planes \(nv12.planeCount); keys \(IOSurfaceCopyAllValues(unsafeBitCast(nv12, to: IOSurfaceRef.self)) ?? [:] as CFDictionary)")
fflush(stdout)
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)
app.run()
