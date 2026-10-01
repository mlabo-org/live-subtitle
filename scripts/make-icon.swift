// Turns a square picture into the macOS app icon set: the picture is clipped to an Apple-style rounded square
// (a superellipse) inside a 1024 canvas with the usual margin, given a soft drop shadow, and scaled to every
// size an .iconset needs.
//   swift scripts/make-icon.swift <source.png> <iconset-dir>
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let arguments = CommandLine.arguments
guard arguments.count == 3 else {
    FileHandle.standardError.write(Data("usage: make-icon.swift <source.png> <iconset-dir>\n".utf8))
    exit(2)
}
let sourceURL = URL(fileURLWithPath: arguments[1])
let outputDir = URL(fileURLWithPath: arguments[2])
guard let source = CGImageSourceCreateWithURL(sourceURL as CFURL, nil),
      let picture = CGImageSourceCreateImageAtIndex(source, 0, nil) else {
    FileHandle.standardError.write(Data("cannot read \(arguments[1])\n".utf8))
    exit(1)
}

let canvas = 1024.0
let artwork = 824.0 // Apple's icon grid: the shape fills 824 of the 1024 canvas
let inset = (canvas - artwork) / 2
let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!

func superellipse(in rect: CGRect, exponent n: Double = 5) -> CGPath {
    let path = CGMutablePath()
    let steps = 720
    for step in 0...steps {
        let t = Double(step) / Double(steps) * 2 * Double.pi
        let c = cos(t), s = sin(t)
        let x = rect.midX + rect.width / 2 * (c < 0 ? -1 : 1) * pow(abs(c), 2 / n)
        let y = rect.midY + rect.height / 2 * (s < 0 ? -1 : 1) * pow(abs(s), 2 / n)
        if step == 0 { path.move(to: CGPoint(x: x, y: y)) } else { path.addLine(to: CGPoint(x: x, y: y)) }
    }
    path.closeSubpath()
    return path
}

func bitmap(_ size: Int) -> CGContext {
    CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0, space: colorSpace,
              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
}

let rect = CGRect(x: inset, y: inset, width: artwork, height: artwork)
let shape = superellipse(in: rect)
let masterContext = bitmap(Int(canvas))
masterContext.saveGState()
masterContext.setShadow(offset: CGSize(width: 0, height: -12), blur: 28, color: CGColor(gray: 0, alpha: 0.28))
masterContext.addPath(shape)
masterContext.setFillColor(CGColor(gray: 1, alpha: 1))
masterContext.fillPath()
masterContext.restoreGState()
masterContext.saveGState()
masterContext.addPath(shape)
masterContext.clip()
masterContext.interpolationQuality = .high
masterContext.draw(picture, in: rect)
masterContext.restoreGState()
let master = masterContext.makeImage()!

func writePNG(_ image: CGImage, to url: URL) {
    guard let destination = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else { exit(1) }
    CGImageDestinationAddImage(destination, image, nil)
    if !CGImageDestinationFinalize(destination) { exit(1) }
}

try FileManager.default.createDirectory(at: outputDir, withIntermediateDirectories: true)
// name: pixel size
let files: [(String, Int)] = [
    ("icon_16x16.png", 16), ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32), ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128), ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256), ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512), ("icon_512x512@2x.png", 1024),
]
// The window icon the running app gives to macOS (eframe would otherwise show its own default in the Dock).
let runtimeContext = bitmap(512)
runtimeContext.interpolationQuality = .high
runtimeContext.draw(master, in: CGRect(x: 0, y: 0, width: 512, height: 512))
writePNG(runtimeContext.makeImage()!, to: outputDir.appendingPathComponent("window-icon-512.png"))

for (name, size) in files {
    let context = bitmap(size)
    context.interpolationQuality = .high
    context.draw(master, in: CGRect(x: 0, y: 0, width: size, height: size))
    writePNG(context.makeImage()!, to: outputDir.appendingPathComponent(name))
}
