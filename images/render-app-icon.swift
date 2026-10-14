// Renders an app's icon as macOS draws it (Icon Composer layers, glass and
// all), SIZE px square, to a PNG: the flat copies in
// macos/Assets.xcassets/AppIconImage.imageset come from the built app.
//
//   swift images/render-app-icon.swift zig-out/Pepper.app 1024 out.png
import AppKit
let args = CommandLine.arguments
let size = CGFloat(Double(args[2])!)
let icon = NSWorkspace.shared.icon(forFile: args[1])
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
icon.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: args[3]))
