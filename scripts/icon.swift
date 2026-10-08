// Renders the Athena app icon (the tlsc logogram, ink on paper) as a 1024 px PNG.
// Usage: swift scripts/icon.swift <out.png>
import AppKit

let size = 1024.0
let paper = NSColor(srgbRed: 0xf9 / 255.0, green: 0xf7 / 255.0, blue: 0xf3 / 255.0, alpha: 1)
let ink = NSColor(srgbRed: 0x1a / 255.0, green: 0x16 / 255.0, blue: 0x14 / 255.0, alpha: 1)

// Glyph geometry from hephaestus/web/src/platform/brand/glyph.tsx, viewBox 3016.5 x 3258.5.
let viewW = 3016.5, viewH = 3258.5
let glyph: [[(Double, Double)]] = [
    [(0, 0), (754.25, 0), (754.25, 3258.5), (377, 3258.5), (377, 382.5), (0, 382.5)],
]
let rects: [(Double, Double, Double, Double)] = [
    (1131, 0, 377.25, 3258.5),
    (1885, 0, 377.25, 765),
    (1885, 1246.75, 377.25, 765),
    (1885, 2493.5, 377.25, 765),
    (2639.25, 0, 377.25, 3258.5),
]

let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
    bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

// Apple's icon grid: an 824 px tile inset 100 px, corner radius about 22.5% of the tile.
let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
paper.setFill()
NSBezierPath(roundedRect: tile, xRadius: 185, yRadius: 185).fill()

let glyphH = 460.0
let scale = glyphH / viewH
let glyphW = viewW * scale
let originX = (size - glyphW) / 2
let originY = (size - glyphH) / 2 + 12  // optical nudge upwards
// SVG y grows downwards; AppKit's grows upwards.
func pt(_ x: Double, _ y: Double) -> NSPoint {
    NSPoint(x: originX + x * scale, y: originY + (viewH - y) * scale)
}
ink.setFill()
for poly in glyph {
    let path = NSBezierPath()
    path.move(to: pt(poly[0].0, poly[0].1))
    for p in poly.dropFirst() { path.line(to: pt(p.0, p.1)) }
    path.close()
    path.fill()
}
for (x, y, w, h) in rects {
    let a = pt(x, y + h)
    NSRect(x: a.x, y: a.y, width: w * scale, height: h * scale).fill()
}
NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
