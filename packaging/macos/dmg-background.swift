// Background of the Akolade disk image window (660×400 pt, normal and Retina).
// Regenerate: swift packaging/macos/dmg-background.swift packaging/macos
// The icons are placed over it by packaging/macos/dmg-settings.py: app at (165, 190),
// Applications at (495, 190), from the top-left corner. Light on purpose: the Finder
// writes the icon names in black.
import AppKit

let (w, h): (CGFloat, CGFloat) = (660, 400)
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "."

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: CGFloat(hex >> 16 & 0xff) / 255, green: CGFloat(hex >> 8 & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: alpha)
}

// Points measured from the top, like the icon positions.
func pt(_ x: CGFloat, _ y: CGFloat) -> NSPoint { NSPoint(x: x, y: h - y) }

func text(_ s: String, size: CGFloat, weight: NSFont.Weight, hex: UInt32, alpha: CGFloat = 1, center: CGFloat, top: CGFloat, mono: Bool = false) {
    let font = mono ? NSFont.monospacedSystemFont(ofSize: size, weight: weight) : NSFont.systemFont(ofSize: size, weight: weight)
    let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color(hex, alpha)]
    let str = NSAttributedString(string: s, attributes: attrs)
    let sz = str.size()
    str.draw(at: NSPoint(x: center - sz.width / 2, y: h - top - sz.height))
}

func draw() {
    // Soft vertical gradient, a hint of Akolade's indigo.
    NSGradient(starting: color(0xfbfbff), ending: color(0xecebf8))!.draw(in: NSRect(x: 0, y: 0, width: w, height: h), angle: -90)

    // The logo's braces, faint and large, framing the two icons.
    text("{", size: 300, weight: .ultraLight, hex: 0x5b66ff, alpha: 0.07, center: 70, top: 10, mono: true)
    text("}", size: 300, weight: .ultraLight, hex: 0x9b5cff, alpha: 0.07, center: 590, top: 10, mono: true)

    // Arrow from the app to Applications.
    let arrow = NSBezierPath()
    arrow.move(to: pt(250, 190))
    arrow.curve(to: pt(410, 190), controlPoint1: pt(300, 160), controlPoint2: pt(360, 160))
    arrow.lineWidth = 4
    arrow.lineCapStyle = .round
    color(0x5b66ff, 0.55).setStroke()
    arrow.stroke()
    let head = NSBezierPath()
    head.move(to: pt(394, 172))
    head.line(to: pt(412, 191))
    head.line(to: pt(390, 200))
    head.lineWidth = 4
    head.lineCapStyle = .round
    head.lineJoinStyle = .round
    head.stroke()

    text("Glissez Akolade dans Applications", size: 15, weight: .semibold, hex: 0x2b2d3a, center: w / 2, top: 318)
    text("puis ouvrez-le depuis le Launchpad ou le dossier Applications.", size: 12, weight: .regular, hex: 0x5d606b, center: w / 2, top: 342)
}

for (scale, name) in [(1, "background.png"), (2, "background@2x.png")] {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(w) * scale, pixelsHigh: Int(h) * scale, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: w, height: h)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    draw()
    NSGraphicsContext.restoreGraphicsState()
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: "\(out)/\(name)"))
}
