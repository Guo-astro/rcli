//! HEIC/HEIF photos to JPEG, for `decisions --image`: the decisions API takes
//! PNG, JPEG or WebP only, and HEIC is the iPhone camera's default.
//!
//! On macOS the system's ImageIO decodes the photo (Apple ships and licenses
//! the HEVC decoder), applies its rotation and mirror to the pixels, and writes
//! a full-size JPEG carrying none of the photo's metadata: no EXIF, no GPS.
//! Elsewhere there is no HEIC decoder this CLI can rely on without bundling
//! libheif and an HEVC decoder (LGPL code, HEVC patent licensing), so the
//! photo is refused with a message saying so.

/// JPEG quality of a converted photo, on ImageIO's 0 to 1 scale.
pub const JPEG_QUALITY: f64 = 0.92;

/// HEVC-coded HEIF images and sequences.
const HEVC_BRANDS: [&[u8]; 8] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs",
];
/// Generic HEIF image and sequence brands, which AVIF files carry too.
const HEIF_BRANDS: [&[u8]; 2] = [b"mif1", b"msf1"];
const AVIF_BRANDS: [&[u8]; 2] = [b"avif", b"avis"];

/// Whether `bytes` is a HEIC/HEIF photo, read from the brands in its `ftyp`
/// box whatever the file is called. AVIF shares the container and is not one.
pub fn is_heif(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let compatible = bytes.get(16..size.min(bytes.len())).unwrap_or_default();
    let brands: Vec<&[u8]> = std::iter::once(&bytes[8..12])
        .chain(compatible.chunks_exact(4))
        .collect();
    let any = |set: &[&[u8]]| brands.iter().any(|brand| set.contains(brand));
    any(&HEVC_BRANDS) || (!any(&AVIF_BRANDS) && any(&HEIF_BRANDS))
}

/// The photo's primary image as a full-size JPEG, upright, with no metadata.
/// The error is a reason, to follow "could not be converted to JPEG: ".
pub fn to_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "macos")]
    {
        imageio::to_jpeg(bytes)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bytes;
        Err("this CLI converts HEIC on macOS only; export it as JPEG first".to_string())
    }
}

#[cfg(target_os = "macos")]
mod imageio {
    use std::ffi::{c_char, c_void};
    use std::ptr::{addr_of, null};

    pub(super) type CfTypeRef = *const c_void;
    type CfIndex = isize;
    type CfNumberType = CfIndex;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_FLOAT64_TYPE: CfNumberType = 6;

    #[repr(C)]
    pub(super) struct CfDictionaryCallBacks {
        _opaque: [u8; 0],
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub(super) static kCFAllocatorDefault: CfTypeRef;
        pub(super) static kCFBooleanTrue: CfTypeRef;
        static kCFTypeDictionaryKeyCallBacks: CfDictionaryCallBacks;
        static kCFTypeDictionaryValueCallBacks: CfDictionaryCallBacks;
        pub(super) fn CFRelease(cf: CfTypeRef);
        pub(super) fn CFDataCreate(
            alloc: CfTypeRef,
            bytes: *const u8,
            length: CfIndex,
        ) -> CfTypeRef;
        fn CFDataCreateMutable(alloc: CfTypeRef, capacity: CfIndex) -> CfTypeRef;
        fn CFDataGetLength(data: CfTypeRef) -> CfIndex;
        fn CFDataGetBytePtr(data: CfTypeRef) -> *const u8;
        fn CFDictionaryCreate(
            alloc: CfTypeRef,
            keys: *const CfTypeRef,
            values: *const CfTypeRef,
            count: CfIndex,
            key_callbacks: *const CfDictionaryCallBacks,
            value_callbacks: *const CfDictionaryCallBacks,
        ) -> CfTypeRef;
        fn CFNumberCreate(alloc: CfTypeRef, kind: CfNumberType, value: *const c_void) -> CfTypeRef;
        fn CFStringCreateWithCString(
            alloc: CfTypeRef,
            text: *const c_char,
            encoding: u32,
        ) -> CfTypeRef;
    }

    #[link(name = "ImageIO", kind = "framework")]
    extern "C" {
        static kCGImageSourceCreateThumbnailFromImageAlways: CfTypeRef;
        static kCGImageSourceCreateThumbnailWithTransform: CfTypeRef;
        static kCGImageDestinationLossyCompressionQuality: CfTypeRef;
        pub(super) fn CGImageSourceCreateWithData(data: CfTypeRef, options: CfTypeRef)
            -> CfTypeRef;
        pub(super) fn CGImageSourceGetCount(source: CfTypeRef) -> usize;
        pub(super) fn CGImageSourceGetPrimaryImageIndex(source: CfTypeRef) -> usize;
        fn CGImageSourceCreateThumbnailAtIndex(
            source: CfTypeRef,
            index: usize,
            options: CfTypeRef,
        ) -> CfTypeRef;
        fn CGImageDestinationCreateWithData(
            data: CfTypeRef,
            kind: CfTypeRef,
            count: usize,
            options: CfTypeRef,
        ) -> CfTypeRef;
        fn CGImageDestinationAddImage(
            destination: CfTypeRef,
            image: CfTypeRef,
            properties: CfTypeRef,
        );
        fn CGImageDestinationFinalize(destination: CfTypeRef) -> bool;
    }

    /// A Core Foundation object this code created, released on drop.
    pub(super) struct Owned(pub(super) CfTypeRef);

    impl Owned {
        pub(super) fn new(object: CfTypeRef, failed: &str) -> Result<Self, String> {
            if object.is_null() {
                Err(failed.to_string())
            } else {
                Ok(Self(object))
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: `new` admits only a non-null object this code owns.
            unsafe { CFRelease(self.0) }
        }
    }

    fn dictionary(pairs: &[(CfTypeRef, CfTypeRef)]) -> Result<Owned, String> {
        let keys: Vec<CfTypeRef> = pairs.iter().map(|pair| pair.0).collect();
        let values: Vec<CfTypeRef> = pairs.iter().map(|pair| pair.1).collect();
        // SAFETY: both arrays hold `pairs.len()` live CF objects; the type
        // callbacks retain them for the dictionary's lifetime.
        let dict = unsafe {
            CFDictionaryCreate(
                kCFAllocatorDefault,
                keys.as_ptr(),
                values.as_ptr(),
                pairs.len() as CfIndex,
                addr_of!(kCFTypeDictionaryKeyCallBacks),
                addr_of!(kCFTypeDictionaryValueCallBacks),
            )
        };
        Owned::new(dict, "ImageIO options could not be built")
    }

    fn float(value: f64) -> Result<Owned, String> {
        // SAFETY: the type names an f64, which CF copies.
        let number = unsafe {
            CFNumberCreate(
                kCFAllocatorDefault,
                K_CF_NUMBER_FLOAT64_TYPE,
                (&value as *const f64).cast(),
            )
        };
        Owned::new(number, "ImageIO options could not be built")
    }

    pub(super) fn to_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
        let unreadable = "macOS could not read it as an image";
        // SAFETY: every object is checked for null by `Owned::new` before use
        // and released when its `Owned` drops; `bytes` outlives `CFDataCreate`,
        // which copies it.
        unsafe {
            let data = Owned::new(
                CFDataCreate(kCFAllocatorDefault, bytes.as_ptr(), bytes.len() as CfIndex),
                unreadable,
            )?;
            let source = Owned::new(CGImageSourceCreateWithData(data.0, null()), unreadable)?;
            if CGImageSourceGetCount(source.0) == 0 {
                return Err(unreadable.to_string());
            }
            let index = CGImageSourceGetPrimaryImageIndex(source.0);
            // A "thumbnail" with no size limit, made from the image rather
            // than an embedded preview, is the full-size photo with its
            // orientation applied: ImageIO's way to an upright image.
            let options = dictionary(&[
                (kCGImageSourceCreateThumbnailFromImageAlways, kCFBooleanTrue),
                (kCGImageSourceCreateThumbnailWithTransform, kCFBooleanTrue),
            ])?;
            let image = Owned::new(
                CGImageSourceCreateThumbnailAtIndex(source.0, index, options.0),
                "macOS could not decode it",
            )?;

            let written = "macOS could not write the JPEG";
            let out = Owned::new(CFDataCreateMutable(kCFAllocatorDefault, 0), written)?;
            let jpeg = Owned::new(
                CFStringCreateWithCString(
                    kCFAllocatorDefault,
                    c"public.jpeg".as_ptr(),
                    K_CF_STRING_ENCODING_UTF8,
                ),
                written,
            )?;
            let destination = Owned::new(
                CGImageDestinationCreateWithData(out.0, jpeg.0, 1, null()),
                written,
            )?;
            let quality = float(super::JPEG_QUALITY)?;
            let settings = dictionary(&[(kCGImageDestinationLossyCompressionQuality, quality.0)])?;
            CGImageDestinationAddImage(destination.0, image.0, settings.0);
            if !CGImageDestinationFinalize(destination.0) {
                return Err(written.to_string());
            }
            let length = CFDataGetLength(out.0) as usize;
            let start = CFDataGetBytePtr(out.0);
            if length == 0 || start.is_null() {
                return Err(written.to_string());
            }
            Ok(std::slice::from_raw_parts(start, length).to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64x48, four solid quadrants (red, green / blue, white), written by
    /// macOS ImageIO as HEIC with orientation 6 (an `irot` box, the way an
    /// iPhone stores a portrait photo) and a GPS block. Made for these tests
    /// from those four colours alone (tests/fixtures/make_heic_fixture.swift).
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/quadrants-orientation-6-gps.heic");

    fn ftyp(major: &[u8], compatible: &[&[u8]]) -> Vec<u8> {
        let mut body = [b"ftyp", major, b"\0\0\0\0"].concat();
        body.extend(compatible.concat());
        let mut out = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        out.extend(body);
        out
    }

    #[test]
    fn heic_and_heif_are_read_from_the_ftyp_brands() {
        assert!(is_heif(FIXTURE));
        for bytes in [
            ftyp(b"heic", &[b"mif1", b"heic"]),
            ftyp(b"heix", &[b"mif1"]),
            ftyp(b"hevc", &[b"msf1"]),
            ftyp(b"heim", &[]),
            ftyp(b"mif1", &[b"heic"]),
            ftyp(b"mif1", &[b"miaf"]),
            ftyp(b"msf1", &[]),
        ] {
            assert!(is_heif(&bytes), "{:?}", String::from_utf8_lossy(&bytes));
        }
    }

    #[test]
    fn avif_other_iso_media_and_other_images_are_not_heic() {
        for bytes in [
            ftyp(b"avif", &[b"mif1", b"miaf"]),
            ftyp(b"mif1", &[b"avif"]),
            ftyp(b"avis", &[b"msf1"]),
            ftyp(b"isom", &[b"mp42"]),
            ftyp(b"qt  ", &[]),
            b"\0\0\0\x18ftyp".to_vec(),
            b"\x89PNG\r\n\x1a\n\0\0\0\x0d".to_vec(),
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0, 0, 0, 0, 0],
            b"RIFF\0\0\0\0WEBPVP8 ".to_vec(),
        ] {
            assert!(!is_heif(&bytes), "{:?}", String::from_utf8_lossy(&bytes));
        }
    }

    #[test]
    fn an_unreadable_heic_is_an_error() {
        let mut bytes = ftyp(b"heic", &[b"mif1", b"heic"]);
        bytes.extend([0u8; 64]);
        assert!(to_jpeg(&bytes).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn heic_is_refused_off_macos() {
        assert!(to_jpeg(FIXTURE).unwrap_err().contains("macOS only"));
    }

    #[cfg(target_os = "macos")]
    mod macos {
        use super::super::imageio::{
            kCFAllocatorDefault, CFDataCreate, CGImageSourceCreateWithData,
            CGImageSourceGetPrimaryImageIndex, CfTypeRef, Owned,
        };
        use super::FIXTURE;
        use crate::io::heic::to_jpeg;
        use std::ffi::c_void;
        use std::ptr::null;

        #[repr(C)]
        struct CgRect {
            x: f64,
            y: f64,
            width: f64,
            height: f64,
        }

        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CFDictionaryGetValue(dict: CfTypeRef, key: *const c_void) -> *const c_void;
        }

        #[link(name = "ImageIO", kind = "framework")]
        extern "C" {
            static kCGImagePropertyGPSDictionary: CfTypeRef;
            static kCGImagePropertyOrientation: CfTypeRef;
            fn CGImageSourceCopyPropertiesAtIndex(
                source: CfTypeRef,
                index: usize,
                options: CfTypeRef,
            ) -> CfTypeRef;
            fn CGImageSourceCreateImageAtIndex(
                source: CfTypeRef,
                index: usize,
                options: CfTypeRef,
            ) -> CfTypeRef;
        }

        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGImageGetWidth(image: CfTypeRef) -> usize;
            fn CGImageGetHeight(image: CfTypeRef) -> usize;
            fn CGColorSpaceCreateDeviceRGB() -> CfTypeRef;
            fn CGBitmapContextCreate(
                data: *mut c_void,
                width: usize,
                height: usize,
                bits_per_component: usize,
                bytes_per_row: usize,
                space: CfTypeRef,
                bitmap_info: u32,
            ) -> CfTypeRef;
            fn CGContextDrawImage(context: CfTypeRef, rect: CgRect, image: CfTypeRef);
        }

        /// What ImageIO reads back from an image file: its size, RGB pixels
        /// (row 0 at the top), and whether it carries GPS or an orientation.
        struct Read {
            width: usize,
            height: usize,
            rgba: Vec<u8>,
            gps: bool,
            orientation: bool,
        }

        fn read(bytes: &[u8]) -> Read {
            const NONE_SKIP_LAST: u32 = 5;
            // SAFETY: test-only; each object is checked by `Owned::new`.
            unsafe {
                let data = Owned::new(
                    CFDataCreate(kCFAllocatorDefault, bytes.as_ptr(), bytes.len() as isize),
                    "data",
                )
                .unwrap();
                let source =
                    Owned::new(CGImageSourceCreateWithData(data.0, null()), "source").unwrap();
                let index = CGImageSourceGetPrimaryImageIndex(source.0);
                let properties = Owned::new(
                    CGImageSourceCopyPropertiesAtIndex(source.0, index, null()),
                    "properties",
                )
                .unwrap();
                let gps =
                    !CFDictionaryGetValue(properties.0, kCGImagePropertyGPSDictionary).is_null();
                let orientation =
                    !CFDictionaryGetValue(properties.0, kCGImagePropertyOrientation).is_null();
                let image = Owned::new(
                    CGImageSourceCreateImageAtIndex(source.0, index, null()),
                    "image",
                )
                .unwrap();
                let (width, height) = (CGImageGetWidth(image.0), CGImageGetHeight(image.0));
                let mut rgba = vec![0u8; width * height * 4];
                let space = Owned::new(CGColorSpaceCreateDeviceRGB(), "space").unwrap();
                let context = Owned::new(
                    CGBitmapContextCreate(
                        rgba.as_mut_ptr().cast(),
                        width,
                        height,
                        8,
                        width * 4,
                        space.0,
                        NONE_SKIP_LAST,
                    ),
                    "context",
                )
                .unwrap();
                let rect = CgRect {
                    x: 0.0,
                    y: 0.0,
                    width: width as f64,
                    height: height as f64,
                };
                CGContextDrawImage(context.0, rect, image.0);
                drop(context);
                Read {
                    width,
                    height,
                    rgba,
                    gps,
                    orientation,
                }
            }
        }

        #[test]
        fn a_heic_photo_becomes_an_upright_jpeg_without_its_metadata() {
            let source = read(FIXTURE);
            assert!(
                source.gps && source.orientation,
                "the fixture carries GPS and an orientation"
            );

            let jpeg = to_jpeg(FIXTURE).unwrap();
            assert!(jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));
            let out = read(&jpeg);
            assert_eq!((out.width, out.height), (48, 64));
            assert!(!out.gps, "GPS survived the conversion");
            assert!(
                !out.orientation,
                "an orientation tag survived the conversion"
            );
            let at = |x: usize, y: usize| -> [bool; 3] {
                let i = (y * out.width + x) * 4;
                [
                    out.rgba[i] > 128,
                    out.rgba[i + 1] > 128,
                    out.rgba[i + 2] > 128,
                ]
            };
            // 90 degrees clockwise: the stored left column (red over blue) is
            // now the top row.
            assert_eq!(at(4, 4), [false, false, true]);
            assert_eq!(at(43, 4), [true, false, false]);
            assert_eq!(at(4, 59), [true, true, true]);
            assert_eq!(at(43, 59), [false, true, false]);
        }
    }
}
