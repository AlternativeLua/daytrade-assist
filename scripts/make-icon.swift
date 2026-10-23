// Draws the 1024×1024 app icon: three candles on a dark rounded square.
// Usage: swift scripts/make-icon.swift assets/AppIcon.png
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "AppIcon.png"
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
                           bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                           colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
let bg = NSBezierPath(roundedRect: tile, xRadius: 185, yRadius: 185)
NSGradient(starting: NSColor(red: 0.09, green: 0.12, blue: 0.18, alpha: 1),
           ending: NSColor(red: 0.04, green: 0.05, blue: 0.08, alpha: 1))!.draw(in: bg, angle: -90)

let green = NSColor(red: 0.12, green: 0.75, blue: 0.46, alpha: 1)
let red = NSColor(red: 0.94, green: 0.28, blue: 0.35, alpha: 1)
// (x, body bottom, body top, wick bottom, wick top, colour)
let candles: [(CGFloat, CGFloat, CGFloat, CGFloat, CGFloat, NSColor)] = [
    (300, 330, 560, 270, 620, green),
    (512, 420, 640, 360, 720, red),
    (724, 470, 760, 410, 820, green),
]
for (x, b0, b1, w0, w1, color) in candles {
    color.setFill()
    NSBezierPath(roundedRect: NSRect(x: x - 9, y: w0, width: 18, height: w1 - w0), xRadius: 9, yRadius: 9).fill()
    NSBezierPath(roundedRect: NSRect(x: x - 62, y: b0, width: 124, height: b1 - b0), xRadius: 22, yRadius: 22).fill()
}

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
