//! HEIC/HEIF photos to JPEG, for `decisions --image`: the decisions API takes
//! PNG, JPEG or WebP only, and HEIC is the iPhone camera's default.
//!
//! On macOS the system's ImageIO decodes the photo (Apple ships and licenses
//! the HEVC decoder), applies its rotation and mirror to the pixels, puts any
//! transparency on white, converts it to sRGB (the service reads pixel values
//! and ignores colour profiles), and writes a full-size JPEG carrying none of
//! the photo's metadata: no EXIF, no GPS. Elsewhere there is no HEIC decoder
//! this CLI can rely on without bundling libheif and an HEVC decoder (LGPL
//! code, HEVC patent licensing), so the photo is refused with a message saying
//! so.

use std::collections::{HashMap, HashSet};

/// JPEG quality of a converted photo, on ImageIO's 0 to 1 scale.
pub const JPEG_QUALITY: f64 = 0.92;

/// HEVC-coded HEIF images and sequences.
const HEVC_BRANDS: [&[u8]; 8] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs",
];
/// Generic HEIF image and sequence brands, which AVIF files carry too.
const HEIF_BRANDS: [&[u8]; 2] = [b"mif1", b"msf1"];
const AVIF_BRANDS: [&[u8]; 2] = [b"avif", b"avis"];
/// Real files list a handful of brands; this bounds the scan of a crafted one.
const MAX_BRANDS: usize = 64;
/// Bounds every box walk, item table and derivation chain in a crafted file.
const MAX_ENTRIES: usize = 4096;

/// Whether `bytes` is a HEIC/HEIF photo, read from the brands in its `ftyp`
/// box whatever the file is called. AVIF shares the container and is not one.
pub fn is_heif(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let end = size.min(bytes.len()).min(16 + 4 * MAX_BRANDS);
    let compatible = bytes.get(16..end).unwrap_or_default();
    let brands: Vec<&[u8]> = std::iter::once(&bytes[8..12])
        .chain(compatible.chunks_exact(4))
        .collect();
    let any = |set: &[&[u8]]| brands.iter().any(|brand| set.contains(brand));
    any(&HEVC_BRANDS) || (!any(&AVIF_BRANDS) && any(&HEIF_BRANDS))
}

/// A big-endian unsigned field of 0, 2, 4 or 8 bytes at `at`.
fn field(bytes: &[u8], at: usize, size: usize) -> Option<u64> {
    if size == 0 {
        return Some(0);
    }
    if !matches!(size, 2 | 4 | 8) {
        return None;
    }
    let raw = bytes.get(at..at.checked_add(size)?)?;
    Some(
        raw.iter()
            .fold(0, |value, &byte| value << 8 | u64::from(byte)),
    )
}

/// One ISO BMFF box: its type, where its payload starts, and where it ends.
struct HeifBox {
    kind: [u8; 4],
    body: usize,
    end: usize,
}

/// The boxes from `start` to `end`, stopping at one that runs past `end`.
fn boxes(bytes: &[u8], start: usize, end: usize) -> Vec<HeifBox> {
    let mut out = Vec::new();
    let mut at = start;
    while at.saturating_add(8) <= end && out.len() < MAX_ENTRIES {
        let Some(mut size) = field(bytes, at, 4) else {
            break;
        };
        let mut body = at + 8;
        if size == 1 {
            let Some(large) = field(bytes, at + 8, 8) else {
                break;
            };
            size = large;
            body = at + 16;
        } else if size == 0 {
            size = (end - at) as u64;
        }
        let Some(box_end) = usize::try_from(size)
            .ok()
            .and_then(|size| at.checked_add(size))
        else {
            break;
        };
        if box_end < body || box_end > end {
            break;
        }
        let mut kind = [0; 4];
        kind.copy_from_slice(&bytes[at + 4..at + 8]);
        out.push(HeifBox {
            kind,
            body,
            end: box_end,
        });
        at = box_end;
    }
    out
}

/// Where an item's data is (`iloc`, ISO/IEC 14496-12 8.11.3).
struct Location {
    method: u64,
    data_reference: u64,
    extents: Vec<(u64, u64)>,
}

fn locations(bytes: &[u8], iloc: &HeifBox) -> Option<HashMap<u64, Location>> {
    let version = *bytes.get(iloc.body)?;
    let sizes = field(bytes, iloc.body + 4, 2)? as usize;
    let (offset_size, length_size, base_size) = (sizes >> 12, (sizes >> 8) & 15, (sizes >> 4) & 15);
    let index_size = if matches!(version, 1 | 2) {
        sizes & 15
    } else {
        0
    };
    let id_size = if version < 2 { 2 } else { 4 };
    let count = field(bytes, iloc.body + 6, id_size)?;
    let mut at = iloc.body + 6 + id_size;
    let mut items = HashMap::new();
    for _ in 0..count.min(MAX_ENTRIES as u64) {
        let id = field(bytes, at, id_size)?;
        at += id_size;
        let mut method = 0;
        if matches!(version, 1 | 2) {
            method = field(bytes, at, 2)? & 15;
            at += 2;
        }
        let data_reference = field(bytes, at, 2)?;
        let base = field(bytes, at + 2, base_size)?;
        at += 2 + base_size;
        let extent_count = field(bytes, at, 2)?;
        at += 2;
        let mut extents = Vec::new();
        for _ in 0..extent_count {
            at += index_size;
            let offset = base.checked_add(field(bytes, at, offset_size)?)?;
            extents.push((offset, field(bytes, at + offset_size, length_size)?));
            at += offset_size + length_size;
        }
        if at > iloc.end {
            return None;
        }
        items.insert(
            id,
            Location {
                method,
                data_reference,
                extents,
            },
        );
    }
    Some(items)
}

/// Whether all of the primary image's data is in the file: every extent of the
/// primary item, of the tiles a grid (or other derived) primary is made from,
/// and of their auxiliary images (an alpha plane), lies within `bytes`. A
/// truncated photo otherwise decodes without an error into a black image.
pub fn is_complete(bytes: &[u8]) -> bool {
    complete(bytes).unwrap_or(false)
}

fn complete(bytes: &[u8]) -> Option<bool> {
    let meta = boxes(bytes, 0, bytes.len())
        .into_iter()
        .find(|item| &item.kind == b"meta")?;
    let children = boxes(bytes, meta.body + 4, meta.end);
    let child = |kind: &[u8; 4]| children.iter().find(|item| &item.kind == kind);
    let pitm = child(b"pitm")?;
    let primary = field(
        bytes,
        pitm.body + 4,
        if *bytes.get(pitm.body)? == 0 { 2 } else { 4 },
    )?;

    let mut types: HashMap<u64, &[u8]> = HashMap::new();
    if let Some(iinf) = child(b"iinf") {
        let first = iinf.body + if *bytes.get(iinf.body)? == 0 { 6 } else { 8 };
        for infe in boxes(bytes, first, iinf.end) {
            let version = *bytes.get(infe.body)?;
            if &infe.kind != b"infe" || version < 2 {
                continue;
            }
            let (id_size, kind_at) = if version == 2 { (2, 8) } else { (4, 10) };
            let id = field(bytes, infe.body + 4, id_size)?;
            types.insert(id, bytes.get(infe.body + kind_at..infe.body + kind_at + 4)?);
        }
    }
    // What an item is made from (`dimg`: a grid's tiles), and the auxiliary
    // images that belong to it (`auxl`, from the auxiliary: its alpha plane).
    let mut sources: HashMap<u64, Vec<u64>> = HashMap::new();
    if let Some(iref) = child(b"iref") {
        let id_size = if *bytes.get(iref.body)? == 0 { 2 } else { 4 };
        for reference in boxes(bytes, iref.body + 4, iref.end) {
            let derived = &reference.kind == b"dimg";
            if !derived && &reference.kind != b"auxl" {
                continue;
            }
            let from = field(bytes, reference.body, id_size)?;
            let count = field(bytes, reference.body + id_size, 2)?;
            let mut at = reference.body + id_size + 2;
            for _ in 0..count {
                let to = field(bytes, at, id_size)?;
                if derived {
                    sources.entry(from).or_default().push(to);
                } else {
                    sources.entry(to).or_default().push(from);
                }
                at += id_size;
            }
        }
    }
    let items = locations(bytes, child(b"iloc")?)?;
    let idat = child(b"idat");

    let mut pending = vec![primary];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if seen.len() > MAX_ENTRIES {
            return Some(false);
        }
        let Some(item) = items.get(&id) else {
            return Some(false);
        };
        if item.data_reference != 0 {
            return Some(false);
        }
        for &(offset, length) in &item.extents {
            let (start, limit) = match (item.method, idat) {
                (0, _) => (offset, bytes.len() as u64),
                (1, Some(idat)) => ((idat.body as u64).checked_add(offset)?, idat.end as u64),
                (1, None) => return Some(false),
                _ => continue,
            };
            if start.checked_add(length).is_none_or(|end| end > limit) {
                return Some(false);
            }
        }
        let from = sources.get(&id).map(Vec::as_slice).unwrap_or_default();
        let made_from_others = types
            .get(&id)
            .is_some_and(|kind| matches!(*kind, b"grid" | b"iovl" | b"iden"));
        if made_from_others && from.is_empty() {
            return Some(false);
        }
        pending.extend_from_slice(from);
    }
    Some(true)
}

/// The photo's primary image as a full-size JPEG, upright, with no metadata.
/// The error is a reason, to follow "could not be converted to JPEG: ".
pub fn to_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if !is_complete(bytes) {
        return Err("the file is incomplete or damaged".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        imageio::to_jpeg(bytes)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("this CLI converts HEIC on macOS only; export it as JPEG first".to_string())
    }
}

#[cfg(target_os = "macos")]
mod imageio {
    use std::ffi::{c_char, c_void};
    use std::ptr::{addr_of, null, null_mut};

    pub(super) type CfTypeRef = *const c_void;
    type CfIndex = isize;
    type CfNumberType = CfIndex;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_FLOAT64_TYPE: CfNumberType = 6;
    /// kCGImageAlphaNoneSkipLast: 8-bit RGB with the fourth byte unused.
    pub(super) const RGB_SKIP_LAST: u32 = 5;

    #[repr(C)]
    pub(super) struct CfDictionaryCallBacks {
        _opaque: [u8; 0],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct CgRect {
        pub(super) x: f64,
        pub(super) y: f64,
        pub(super) width: f64,
        pub(super) height: f64,
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
        pub(super) fn CFDictionaryGetValue(dict: CfTypeRef, key: CfTypeRef) -> CfTypeRef;
        fn CFBooleanGetValue(boolean: CfTypeRef) -> u8;
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
        static kCGImageDestinationOptimizeColorForSharing: CfTypeRef;
        static kCGImagePropertyHasAlpha: CfTypeRef;
        pub(super) fn CGImageSourceCreateWithData(data: CfTypeRef, options: CfTypeRef)
            -> CfTypeRef;
        pub(super) fn CGImageSourceGetCount(source: CfTypeRef) -> usize;
        pub(super) fn CGImageSourceGetPrimaryImageIndex(source: CfTypeRef) -> usize;
        pub(super) fn CGImageSourceCopyPropertiesAtIndex(
            source: CfTypeRef,
            index: usize,
            options: CfTypeRef,
        ) -> CfTypeRef;
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

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        static kCGColorSpaceSRGB: CfTypeRef;
        pub(super) fn CGImageGetWidth(image: CfTypeRef) -> usize;
        pub(super) fn CGImageGetHeight(image: CfTypeRef) -> usize;
        fn CGColorSpaceCreateWithName(name: CfTypeRef) -> CfTypeRef;
        pub(super) fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: CfTypeRef,
            bitmap_info: u32,
        ) -> CfTypeRef;
        fn CGContextSetRGBFillColor(
            context: CfTypeRef,
            red: f64,
            green: f64,
            blue: f64,
            alpha: f64,
        );
        fn CGContextFillRect(context: CfTypeRef, rect: CgRect);
        pub(super) fn CGContextDrawImage(context: CfTypeRef, rect: CgRect, image: CfTypeRef);
        fn CGBitmapContextCreateImage(context: CfTypeRef) -> CfTypeRef;
    }

    /// A Core Foundation object this code created, released on drop.
    pub(super) struct Owned(CfTypeRef);

    impl Owned {
        pub(super) fn new(object: CfTypeRef, failed: &str) -> Result<Self, String> {
            if object.is_null() {
                Err(failed.to_string())
            } else {
                Ok(Self(object))
            }
        }

        pub(super) fn get(&self) -> CfTypeRef {
            self.0
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

    /// `image` drawn on white in sRGB: JPEG has no transparency, and
    /// ImageIO's encoder would leave transparent pixels black.
    fn on_white(image: Owned) -> Result<Owned, String> {
        // SAFETY: `image` is a live CGImage; every object made here is checked
        // by `Owned::new` and released when its `Owned` drops.
        unsafe {
            let failed = "macOS could not flatten its transparency";
            let (width, height) = (CGImageGetWidth(image.get()), CGImageGetHeight(image.get()));
            let space = Owned::new(CGColorSpaceCreateWithName(kCGColorSpaceSRGB), failed)?;
            let context = Owned::new(
                CGBitmapContextCreate(null_mut(), width, height, 8, 0, space.get(), RGB_SKIP_LAST),
                failed,
            )?;
            let rect = CgRect {
                x: 0.0,
                y: 0.0,
                width: width as f64,
                height: height as f64,
            };
            CGContextSetRGBFillColor(context.get(), 1.0, 1.0, 1.0, 1.0);
            CGContextFillRect(context.get(), rect);
            CGContextDrawImage(context.get(), rect, image.get());
            Owned::new(CGBitmapContextCreateImage(context.get()), failed)
        }
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
            let source = Owned::new(CGImageSourceCreateWithData(data.get(), null()), unreadable)?;
            if CGImageSourceGetCount(source.get()) == 0 {
                return Err(unreadable.to_string());
            }
            let index = CGImageSourceGetPrimaryImageIndex(source.get());
            let properties = Owned::new(
                CGImageSourceCopyPropertiesAtIndex(source.get(), index, null()),
                unreadable,
            )?;
            // Set only for a photo with an alpha plane; the decoded image
            // itself reports alpha either way.
            let has_alpha = CFDictionaryGetValue(properties.get(), kCGImagePropertyHasAlpha);
            let has_alpha = !has_alpha.is_null() && CFBooleanGetValue(has_alpha) != 0;
            // A "thumbnail" with no size limit, made from the image rather
            // than an embedded preview, is the full-size photo with its
            // orientation applied: ImageIO's way to an upright image.
            let options = dictionary(&[
                (kCGImageSourceCreateThumbnailFromImageAlways, kCFBooleanTrue),
                (kCGImageSourceCreateThumbnailWithTransform, kCFBooleanTrue),
            ])?;
            let mut image = Owned::new(
                CGImageSourceCreateThumbnailAtIndex(source.get(), index, options.get()),
                "macOS could not decode it",
            )?;
            if has_alpha {
                image = on_white(image)?;
            }

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
                CGImageDestinationCreateWithData(out.get(), jpeg.get(), 1, null()),
                written,
            )?;
            let quality = float(super::JPEG_QUALITY)?;
            // The service reads pixel values and ignores any colour profile, so
            // a Display P3 or HDR photo goes as sRGB values.
            let settings = dictionary(&[
                (kCGImageDestinationLossyCompressionQuality, quality.get()),
                (kCGImageDestinationOptimizeColorForSharing, kCFBooleanTrue),
            ])?;
            CGImageDestinationAddImage(destination.get(), image.get(), settings.get());
            if !CGImageDestinationFinalize(destination.get()) {
                return Err(written.to_string());
            }
            let length = CFDataGetLength(out.get()) as usize;
            let start = CFDataGetBytePtr(out.get());
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

    // Made with macOS ImageIO from solid colours alone, by
    // tests/fixtures/make_heic_fixture.swift: quadrants, red and green over blue
    // and white, as stored.
    /// 64x48, orientation 6 (an `irot` box, as an iPhone stores a portrait), GPS.
    const ROTATED: &[u8] = include_bytes!("../../tests/fixtures/quadrants-orientation-6-gps.heic");
    /// Orientation 5 (mirrored, then turned), with GPS, Exif, TIFF and XMP
    /// fields holding FIXTURE- markers.
    const MIRRORED: &[u8] =
        include_bytes!("../../tests/fixtures/quadrants-orientation-5-metadata.heic");
    /// The green quadrant transparent (an `auxl` alpha plane), orientation 1.
    const ALPHA: &[u8] = include_bytes!("../../tests/fixtures/quadrants-alpha.heic");
    /// 1024x768, stored as a grid of four 512-pixel tiles.
    const GRID: &[u8] = include_bytes!("../../tests/fixtures/quadrants-grid.heic");
    /// One colour, sRGB (204, 77, 51), stored in Display P3.
    const P3: &[u8] = include_bytes!("../../tests/fixtures/swatch-p3.heic");
    const ALL: [&[u8]; 5] = [ROTATED, MIRRORED, ALPHA, GRID, P3];

    fn ftyp(major: &[u8], compatible: &[&[u8]]) -> Vec<u8> {
        let mut body = [b"ftyp", major, b"\0\0\0\0"].concat();
        body.extend(compatible.concat());
        let mut out = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        out.extend(body);
        out
    }

    #[test]
    fn heic_and_heif_are_read_from_the_ftyp_brands() {
        for bytes in ALL {
            assert!(is_heif(bytes));
        }
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
    fn at_most_64_compatible_brands_are_read() {
        let brands = |filler: usize| {
            let mut compatible = vec![&b"mp41"[..]; filler];
            compatible.push(b"heic");
            ftyp(b"isom", &compatible)
        };
        assert!(is_heif(&brands(63)));
        assert!(!is_heif(&brands(64)));
    }

    #[test]
    fn a_whole_file_is_complete() {
        for bytes in ALL {
            assert!(is_complete(bytes));
        }
    }

    #[test]
    fn a_file_cut_short_in_its_primary_image_is_refused_everywhere() {
        for bytes in ALL {
            for length in [bytes.len() - 1, bytes.len() - 40, bytes.len() / 2, 200, 40] {
                let cut = &bytes[..length];
                assert!(!is_complete(cut), "{length} of {}", bytes.len());
                assert_eq!(
                    to_jpeg(cut).unwrap_err(),
                    "the file is incomplete or damaged"
                );
            }
        }
        // The cut lands in a grid's last tile, and in the alpha plane.
        assert!(!is_complete(&GRID[..GRID.len() - 1]));
        assert!(!is_complete(&ALPHA[..ALPHA.len() - 1]));
        for bytes in [
            &[][..],
            &ftyp(b"heic", &[b"mif1"]),
            &[0xFF, 0xD8, 0xFF, 0xE0],
        ] {
            assert!(!is_complete(bytes));
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn heic_is_refused_off_macos() {
        assert!(to_jpeg(ROTATED).unwrap_err().contains("macOS only"));
    }

    #[cfg(target_os = "macos")]
    mod macos {
        use super::super::imageio::{
            kCFAllocatorDefault, CFDataCreate, CFDictionaryGetValue, CGBitmapContextCreate,
            CGContextDrawImage, CGImageGetHeight, CGImageGetWidth,
            CGImageSourceCopyPropertiesAtIndex, CGImageSourceCreateWithData,
            CGImageSourceGetPrimaryImageIndex, CfTypeRef, CgRect, Owned, RGB_SKIP_LAST,
        };
        use super::{ALPHA, GRID, MIRRORED, P3, ROTATED};
        use crate::io::heic::to_jpeg;
        use std::ptr::null;

        #[link(name = "ImageIO", kind = "framework")]
        extern "C" {
            static kCGImagePropertyGPSDictionary: CfTypeRef;
            static kCGImagePropertyOrientation: CfTypeRef;
            fn CGImageSourceCreateImageAtIndex(
                source: CfTypeRef,
                index: usize,
                options: CfTypeRef,
            ) -> CfTypeRef;
        }

        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGImageGetColorSpace(image: CfTypeRef) -> CfTypeRef;
        }

        /// What ImageIO reads back from an image file: its size, its stored
        /// pixel values (no colour conversion, as the service reads them; row 0
        /// at the top), and whether it carries GPS or an orientation.
        struct Read {
            width: usize,
            height: usize,
            rgba: Vec<u8>,
            gps: bool,
            orientation: bool,
        }

        impl Read {
            fn at(&self, x: usize, y: usize) -> [u8; 3] {
                let i = (y * self.width + x) * 4;
                [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]]
            }

            /// The four corners as R, G, B, W (or K for black).
            fn corners(&self) -> String {
                let (w, h) = (self.width, self.height);
                [(4, 4), (w - 5, 4), (4, h - 5), (w - 5, h - 5)]
                    .into_iter()
                    .map(|(x, y)| match self.at(x, y).map(|value| value > 128) {
                        [true, false, false] => 'R',
                        [false, true, false] => 'G',
                        [false, false, true] => 'B',
                        [true, true, true] => 'W',
                        [false, false, false] => 'K',
                        _ => '?',
                    })
                    .collect()
            }
        }

        fn read(bytes: &[u8]) -> Read {
            // SAFETY: test-only; each object is checked by `Owned::new`.
            unsafe {
                let data = Owned::new(
                    CFDataCreate(kCFAllocatorDefault, bytes.as_ptr(), bytes.len() as isize),
                    "data",
                )
                .unwrap();
                let source =
                    Owned::new(CGImageSourceCreateWithData(data.get(), null()), "source").unwrap();
                let index = CGImageSourceGetPrimaryImageIndex(source.get());
                let properties = Owned::new(
                    CGImageSourceCopyPropertiesAtIndex(source.get(), index, null()),
                    "properties",
                )
                .unwrap();
                let gps = !CFDictionaryGetValue(properties.get(), kCGImagePropertyGPSDictionary)
                    .is_null();
                let orientation =
                    !CFDictionaryGetValue(properties.get(), kCGImagePropertyOrientation).is_null();
                let image = Owned::new(
                    CGImageSourceCreateImageAtIndex(source.get(), index, null()),
                    "image",
                )
                .unwrap();
                let (width, height) = (CGImageGetWidth(image.get()), CGImageGetHeight(image.get()));
                let mut rgba = vec![0u8; width * height * 4];
                // Drawn in the image's own colour space: its values, unconverted.
                let context = Owned::new(
                    CGBitmapContextCreate(
                        rgba.as_mut_ptr().cast(),
                        width,
                        height,
                        8,
                        width * 4,
                        CGImageGetColorSpace(image.get()),
                        RGB_SKIP_LAST,
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
                CGContextDrawImage(context.get(), rect, image.get());
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

        fn has(haystack: &[u8], needle: &[u8]) -> bool {
            haystack
                .windows(needle.len())
                .any(|window| window == needle)
        }

        #[test]
        fn a_heic_photo_becomes_an_upright_jpeg_without_its_metadata() {
            let source = read(ROTATED);
            assert!(
                source.gps && source.orientation,
                "the fixture carries GPS and an orientation"
            );
            let jpeg = to_jpeg(ROTATED).unwrap();
            assert!(jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));
            let out = read(&jpeg);
            assert!(!out.gps, "GPS survived the conversion");
            assert!(
                !out.orientation,
                "an orientation tag survived the conversion"
            );
            // A quarter turn clockwise: the stored left column (red over
            // blue) is now the top row.
            assert_eq!(
                (out.width, out.height, out.corners()),
                (48, 64, "BRWG".into())
            );
        }

        #[test]
        fn a_mirrored_photo_is_upright_and_none_of_its_metadata_survives() {
            assert!(has(MIRRORED, b"FIXTURE-XMP-CREATOR") && has(MIRRORED, b"FIXTURE-MAKE"));
            let jpeg = to_jpeg(MIRRORED).unwrap();
            for marker in [
                &b"FIXTURE-"[..],
                b"http://ns.adobe.com/xap/1.0/",
                b"Exif\0\0",
            ] {
                assert!(!has(&jpeg, marker), "{}", String::from_utf8_lossy(marker));
            }
            let out = read(&jpeg);
            assert!(!out.gps && !out.orientation);
            // Orientation 5: mirrored, then turned.
            assert_eq!(
                (out.width, out.height, out.corners()),
                (48, 64, "RBGW".into())
            );
        }

        #[test]
        fn transparency_goes_on_white() {
            let out = read(&to_jpeg(ALPHA).unwrap());
            assert_eq!(
                (out.width, out.height, out.corners()),
                (64, 48, "RWBW".into())
            );
        }

        #[test]
        fn a_grid_photo_is_put_together_from_its_tiles() {
            let out = read(&to_jpeg(GRID).unwrap());
            assert_eq!(
                (out.width, out.height, out.corners()),
                (1024, 768, "RGBW".into())
            );
        }

        #[test]
        fn display_p3_is_sent_as_srgb_values() {
            // Unconverted, the stored P3 values read (189, 85, 60).
            let pixel = read(&to_jpeg(P3).unwrap()).at(32, 24);
            let off = pixel
                .iter()
                .zip([204u8, 77, 51])
                .map(|(got, want)| got.abs_diff(want))
                .max();
            assert!(off <= Some(3), "{pixel:?}");
        }
    }
}
