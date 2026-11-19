//! The application icon: the window icon decoded from `assets/playr.png`, the multi-size
//! `assets/playr.ico`, and (on Windows) the icon resource that `build.rs` embeds in playr.exe,
//! which is what Explorer, the taskbar and "Open with" show.

use std::path::Path;

/// The sizes `assets/make_icon.py` writes into playr.ico and that the exe must carry, so that
/// Windows has a hand-made picture for every DPI scale instead of a blurry resample.
const ICON_SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];

fn asset(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn the_window_icon_is_the_256_pixel_square_rgba_picture() {
    let icon = playr::ui::window_icon();

    assert_eq!((icon.width, icon.height), (256, 256));
    assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    let pixels: Vec<&[u8]> = icon.rgba.chunks_exact(4).collect();
    assert!(pixels.iter().any(|p| p[3] == 255), "no opaque pixel: the picture is empty");
    assert!(pixels.iter().any(|p| *p != pixels[0]), "every pixel is {:?}: a flat fill, not a picture", pixels[0]);
}

/// Pixels whose colour follows from the geometry in `assets/make_icon.py`: a rounded square
/// (corner radius 56) filled with a violet #7c3aed to orange #f97316 gradient along x + y, and
/// a white play triangle (76,72) (76,184) (168,128) over it.
#[test]
fn the_window_icon_pixels_follow_the_drawing() {
    let icon = playr::ui::window_icon();
    let pixel = |x: usize, y: usize| -> [u8; 4] { icon.rgba[(y * 256 + x) * 4..(y * 256 + x) * 4 + 4].try_into().unwrap() };
    let near = |actual: [u8; 4], expected: [u8; 4]| actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 3);

    assert_eq!(pixel(0, 0)[3], 0, "(0,0) lies outside the rounded corner: {:?}", pixel(0, 0));
    assert_eq!(pixel(255, 255)[3], 0, "(255,255) lies outside the rounded corner: {:?}", pixel(255, 255));
    assert!(near(pixel(110, 128), [255, 255, 255, 255]), "(110,128) is inside the triangle: {:?}", pixel(110, 128));
    // t = (20 + 128) / 510 = 0.290: 124 + 125t, 58 + 57t, 237 - 215t.
    assert!(near(pixel(20, 128), [160, 75, 175, 255]), "(20,128) is plain gradient: {:?}", pixel(20, 128));
}

/// `(width, height)` of every image in an .ico file, in file order; a 0 in the directory means 256.
fn ico_sizes(file: &[u8]) -> Vec<(u32, u32)> {
    assert_eq!(&file[..4], &[0, 0, 1, 0], "ICONDIR header: reserved 0, type 1 (icon)");
    let count = usize::from(u16::from_le_bytes([file[4], file[5]]));
    (0..count)
        .map(|n| {
            let entry = &file[6 + 16 * n..6 + 16 * (n + 1)];
            let size = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
            assert!(offset + size <= file.len(), "image {n}: {size} bytes at {offset} lie outside the {}-byte file", file.len());
            let side = |b: u8| if b == 0 { 256 } else { u32::from(b) };
            (side(entry[0]), side(entry[1]))
        })
        .collect()
}

#[test]
fn the_ico_file_holds_one_square_image_per_size() {
    let mut sizes = ico_sizes(&asset("playr.ico"));
    sizes.sort();
    assert_eq!(sizes, ICON_SIZES.map(|s| (s, s)).to_vec());
}

#[cfg(windows)]
mod embedded {
    use super::ICON_SIZES;

    const RT_ICON: u32 = 3;
    const RT_GROUP_ICON: u32 = 14;
    const RESOURCE_DIRECTORY: u64 = 2;

    fn u16_at(file: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(file[at..at + 2].try_into().unwrap())
    }

    fn u32_at(file: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(file[at..at + 4].try_into().unwrap())
    }

    /// The resource tree of a PE image, flattened to `(type, id, data)` for resources with
    /// numeric type and id (an icon group from an .rc file has both).
    fn resources(file: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
        assert_eq!(&file[..2], b"MZ");
        let pe = u32_at(file, 0x3C) as usize;
        assert_eq!(&file[pe..pe + 4], b"PE\0\0");
        let section_count = usize::from(u16_at(file, pe + 6));
        let optional = pe + 24;
        let directories = match u16_at(file, optional) {
            0x20B => optional + 112,
            0x10B => optional + 96,
            other => panic!("optional header magic {other:#x}"),
        };
        let rsrc_rva = u32_at(file, directories + 8 * RESOURCE_DIRECTORY as usize);
        assert_ne!(rsrc_rva, 0, "the image has no resource directory, so no icon");
        let table = optional + usize::from(u16_at(file, pe + 20));
        let sections: Vec<(u32, u32, u32)> = (0..section_count)
            .map(|n| (u32_at(file, table + 40 * n + 12), u32_at(file, table + 40 * n + 16), u32_at(file, table + 40 * n + 20)))
            .collect();
        let offset_of = |rva: u32| -> usize {
            let (start, _, raw) = sections
                .iter()
                .find(|(start, raw_size, _)| rva >= *start && rva - *start < *raw_size)
                .unwrap_or_else(|| panic!("RVA {rva:#x} is in no section"));
            (raw + (rva - start)) as usize
        };
        let base = offset_of(rsrc_rva);

        // Entries of the directory at `dir` (offset from the section start): (id, is_dir, target).
        let entries = |dir: usize| -> Vec<(u32, bool, usize)> {
            let at = base + dir;
            let count = usize::from(u16_at(file, at + 12)) + usize::from(u16_at(file, at + 14));
            (0..count)
                .map(|n| {
                    let entry = at + 16 + 8 * n;
                    let target = u32_at(file, entry + 4);
                    (u32_at(file, entry), target & 0x8000_0000 != 0, (target & 0x7FFF_FFFF) as usize)
                })
                .collect()
        };
        let mut out = Vec::new();
        for (kind, kind_is_dir, kind_dir) in entries(0) {
            if kind & 0x8000_0000 != 0 || !kind_is_dir {
                continue;
            }
            for (id, id_is_dir, id_dir) in entries(kind_dir) {
                if id & 0x8000_0000 != 0 || !id_is_dir {
                    continue;
                }
                for (_language, leaf_is_dir, leaf) in entries(id_dir) {
                    assert!(!leaf_is_dir, "type {kind} id {id}: a fourth directory level");
                    let data_rva = u32_at(file, base + leaf);
                    let size = u32_at(file, base + leaf + 4) as usize;
                    let start = offset_of(data_rva);
                    out.push((kind, id, file[start..start + size].to_vec()));
                }
            }
        }
        out
    }

    /// `(width, height, bytes, icon id)` per GRPICONDIRENTRY; a 0 side means 256.
    fn group_entries(group: &[u8]) -> Vec<(u32, u32, usize, u32)> {
        assert_eq!(&group[..4], &[0, 0, 1, 0], "GRPICONDIR header: reserved 0, type 1 (icon)");
        let count = usize::from(u16_at(group, 4));
        assert_eq!(group.len(), 6 + 14 * count, "GRPICONDIR of {count} entries");
        let side = |b: u8| if b == 0 { 256 } else { u32::from(b) };
        (0..count)
            .map(|n| {
                let e = &group[6 + 14 * n..6 + 14 * (n + 1)];
                (side(e[0]), side(e[1]), u32_at(e, 8) as usize, u32::from(u16_at(e, 12)))
            })
            .collect()
    }

    #[test]
    fn playr_exe_embeds_icon_group_1_with_every_size_and_its_images() {
        let exe = std::path::Path::new(env!("CARGO_BIN_EXE_playr"));
        let file = std::fs::read(exe).unwrap_or_else(|e| panic!("cannot read {}: {e}", exe.display()));
        let resources = resources(&file);

        let groups: Vec<&(u32, u32, Vec<u8>)> = resources.iter().filter(|(kind, _, _)| *kind == RT_GROUP_ICON).collect();
        // assets/playr.rc declares `1 ICON`: one group, id 1, the one Explorer shows first.
        assert_eq!(groups.iter().map(|(_, id, _)| *id).collect::<Vec<_>>(), vec![1], "RT_GROUP_ICON ids in {}", exe.display());

        let entries = group_entries(&groups[0].2);
        let mut sizes: Vec<(u32, u32)> = entries.iter().map(|(w, h, _, _)| (*w, *h)).collect();
        sizes.sort();
        assert_eq!(sizes, ICON_SIZES.map(|s| (s, s)).to_vec());

        for (w, h, bytes, icon_id) in &entries {
            let images: Vec<usize> =
                resources.iter().filter(|(kind, id, _)| *kind == RT_ICON && id == icon_id).map(|(_, _, data)| data.len()).collect();
            assert_eq!(images, vec![*bytes], "{w}x{h}: RT_ICON {icon_id} sizes in the exe");
        }
    }
}
