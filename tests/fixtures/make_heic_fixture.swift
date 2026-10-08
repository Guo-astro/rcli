// Writes quadrants-orientation-6-gps.heic, the HEIC fixture src/io/heic.rs
// tests against: 64x48, four solid quadrants (red, green / blue, white),
// orientation 6 (ImageIO stores it as an `irot` box, as an iPhone does) and a
// GPS block. macOS only (ImageIO's HEVC encoder):
//
//   swiftc tests/fixtures/make_heic_fixture.swift -o /tmp/make_heic_fixture
//   /tmp/make_heic_fixture tests/fixtures/quadrants-orientation-6-gps.heic
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let out = CommandLine.arguments[1]
let width = 64, height = 48
let context = CGContext(
  data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
  space: CGColorSpace(name: CGColorSpace.sRGB)!,
  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
// (red, green, blue, left, top); CoreGraphics counts y from the bottom.
for (r, g, b, x, y) in [(1.0, 0.0, 0.0, 0, 0), (0, 1, 0, 1, 0), (0, 0, 1, 0, 1), (1, 1, 1, 1, 1)] {
  context.setFillColor(red: r, green: g, blue: b, alpha: 1)
  context.fill(CGRect(x: x * width / 2, y: (1 - y) * height / 2, width: width / 2, height: height / 2))
}
let destination = CGImageDestinationCreateWithURL(
  URL(fileURLWithPath: out) as CFURL, UTType.heic.identifier as CFString, 1, nil)!
let gps: [CFString: Any] = [
  kCGImagePropertyGPSLatitude: 37.7749, kCGImagePropertyGPSLatitudeRef: "N",
  kCGImagePropertyGPSLongitude: 122.4194, kCGImagePropertyGPSLongitudeRef: "W",
]
let properties: [CFString: Any] = [
  kCGImagePropertyOrientation: 6, kCGImageDestinationLossyCompressionQuality: 0.9,
  kCGImagePropertyGPSDictionary: gps,
]
CGImageDestinationAddImage(destination, context.makeImage()!, properties as CFDictionary)
precondition(CGImageDestinationFinalize(destination), "could not write \(out)")
