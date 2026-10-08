// Writes the HEIC fixtures the HEIC tests use, from solid colours alone. macOS
// only (ImageIO's HEVC encoder):
//
//   swiftc tests/fixtures/make_heic_fixture.swift -o /tmp/make_heic_fixture
//   /tmp/make_heic_fixture tests/fixtures/quadrants-orientation-6-gps.heic
//   /tmp/make_heic_fixture tests/fixtures/quadrants-orientation-5-metadata.heic metadata
//   /tmp/make_heic_fixture tests/fixtures/quadrants-alpha.heic alpha
//   /tmp/make_heic_fixture tests/fixtures/swatch-p3.heic p3
//   /tmp/make_heic_fixture tests/fixtures/quadrants-grid.heic grid
//
// Every one is quadrants, red and green over blue and white, unless noted:
//   (default)  64x48, orientation 6 (ImageIO stores it as an `irot` box, as an
//              iPhone does) and a GPS block.
//   metadata   orientation 5 (mirrored, then turned), with GPS, Exif, TIFF and
//              XMP fields that hold FIXTURE- marker strings.
//   alpha      the green quadrant fully transparent; orientation 1.
//   p3         one colour, sRGB (204, 77, 51), stored in Display P3.
//   grid       1024x768, which ImageIO stores as a grid of 512-pixel tiles.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let out = CommandLine.arguments[1]
let mode = CommandLine.arguments.count > 2 ? CommandLine.arguments[2] : "default"
let (width, height) = mode == "grid" ? (1024, 768) : (64, 48)
let space = CGColorSpace(name: mode == "p3" ? CGColorSpace.displayP3 : CGColorSpace.sRGB)!
let context = CGContext(
  data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0, space: space,
  bitmapInfo: mode == "alpha"
    ? CGImageAlphaInfo.premultipliedLast.rawValue : CGImageAlphaInfo.noneSkipLast.rawValue)!
// (red, green, blue, alpha, left, top); CoreGraphics counts y from the bottom.
var cells = [(1.0, 0.0, 0.0, 1.0, 0, 0), (0, 1, 0, 1, 1, 0), (0, 0, 1, 1, 0, 1), (1, 1, 1, 1, 1, 1)]
if mode == "alpha" { cells[1].3 = 0 }
if mode == "p3" { cells = cells.map { (0.8, 0.3, 0.2, 1, $0.4, $0.5) } }
for (r, g, b, a, x, y) in cells {
  context.setFillColor(red: r, green: g, blue: b, alpha: a)
  context.fill(CGRect(x: x * width / 2, y: (1 - y) * height / 2, width: width / 2, height: height / 2))
}
let destination = CGImageDestinationCreateWithURL(
  URL(fileURLWithPath: out) as CFURL, UTType.heic.identifier as CFString, 1, nil)!
var gps: [CFString: Any] = [
  kCGImagePropertyGPSLatitude: 37.7749, kCGImagePropertyGPSLatitudeRef: "N",
  kCGImagePropertyGPSLongitude: 122.4194, kCGImagePropertyGPSLongitudeRef: "W",
]
var properties: [CFString: Any] = [
  kCGImagePropertyOrientation: 6, kCGImageDestinationLossyCompressionQuality: 0.9,
  kCGImagePropertyGPSDictionary: gps,
]
switch mode {
case "default":
  CGImageDestinationAddImage(destination, context.makeImage()!, properties as CFDictionary)
case "metadata":
  gps[kCGImagePropertyGPSAltitude] = 12.5
  properties[kCGImagePropertyOrientation] = 5
  properties[kCGImagePropertyGPSDictionary] = gps
  properties[kCGImagePropertyExifDictionary] = [
    kCGImagePropertyExifUserComment: "FIXTURE-EXIF-COMMENT",
    kCGImagePropertyExifDateTimeOriginal: "2026:10:08 12:34:56",
    kCGImagePropertyExifLensModel: "FIXTURE-LENS",
  ]
  properties[kCGImagePropertyTIFFDictionary] = [
    kCGImagePropertyTIFFMake: "FIXTURE-MAKE", kCGImagePropertyTIFFModel: "FIXTURE-MODEL",
  ]
  let xmp = CGImageMetadataCreateMutable()
  _ = CGImageMetadataSetValueWithPath(xmp, nil, "dc:creator" as CFString, "FIXTURE-XMP-CREATOR" as CFString)
  _ = CGImageMetadataSetValueWithPath(xmp, nil, "xmp:CreatorTool" as CFString, "FIXTURE-XMP-TOOL" as CFString)
  CGImageDestinationAddImageAndMetadata(destination, context.makeImage()!, xmp, properties as CFDictionary)
default:
  properties[kCGImagePropertyOrientation] = 1
  properties[kCGImagePropertyGPSDictionary] = nil
  CGImageDestinationAddImage(destination, context.makeImage()!, properties as CFDictionary)
}
precondition(CGImageDestinationFinalize(destination), "could not write \(out)")
