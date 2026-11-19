//! "One executable, no DLLs to ship": `.cargo/config.toml` links the C runtime statically
//! (`-C target-feature=+crt-static`). Without that flag the executable imports
//! `VCRUNTIME140.dll` and does not start on a Windows install that lacks the Visual C++
//! redistributable. Nothing else notices when the flag is lost, so the import tables of the
//! built executables are read here.
#![cfg(windows)]

use std::path::Path;

/// DLL name prefixes of the Microsoft C/C++ runtime, lower case. `api-ms-win-core-*` and the
/// like are API sets of Windows itself and are not listed; only `api-ms-win-crt-*` is the
/// C runtime.
const C_RUNTIME_PREFIXES: [&str; 5] = ["vcruntime", "msvcp", "ucrtbase", "api-ms-win-crt-", "msvcr"];

fn is_c_runtime(dll: &str) -> bool {
    let name = dll.to_ascii_lowercase();
    C_RUNTIME_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

fn bytes_at<'a>(file: &'a [u8], at: u64, len: usize, what: &str) -> Result<&'a [u8], String> {
    usize::try_from(at)
        .ok()
        .and_then(|at| file.get(at..at.checked_add(len)?))
        .ok_or_else(|| format!("{what}: {len} bytes at offset {at:#x} lie outside the {}-byte file", file.len()))
}

fn u16_at(file: &[u8], at: u64, what: &str) -> Result<u16, String> {
    bytes_at(file, at, 2, what).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(file: &[u8], at: u64, what: &str) -> Result<u32, String> {
    bytes_at(file, at, 4, what).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The headers of a PE image, as far as finding the import tables takes.
struct Image<'a> {
    file: &'a [u8],
    /// Offset of the data directories and how many the image declares.
    directories: u64,
    directory_count: u32,
    /// Per section: virtual address, size of the raw data, file offset of the raw data.
    sections: Vec<(u32, u32, u32)>,
}

impl<'a> Image<'a> {
    fn parse(file: &'a [u8]) -> Result<Self, String> {
        if bytes_at(file, 0, 2, "DOS signature")? != b"MZ" {
            return Err("not an executable: no MZ signature at offset 0".to_string());
        }
        let pe = u64::from(u32_at(file, 0x3C, "e_lfanew")?);
        if bytes_at(file, pe, 4, "PE signature")? != b"PE\0\0" {
            return Err(format!("no PE signature at offset {pe:#x}"));
        }
        let section_count = u16_at(file, pe + 6, "number of sections")?;
        let optional_size = u16_at(file, pe + 20, "size of the optional header")?;
        let optional = pe + 24;
        let directories = match u16_at(file, optional, "optional header magic")? {
            0x20B => optional + 112,
            0x10B => optional + 96,
            other => return Err(format!("optional header magic {other:#x} is neither PE32 (0x10b) nor PE32+ (0x20b)")),
        };
        let directory_count = u32_at(file, directories - 4, "number of data directories")?;
        let table = optional + u64::from(optional_size);
        let mut sections = Vec::new();
        for n in 0..u64::from(section_count) {
            let at = table + 40 * n;
            sections.push((
                u32_at(file, at + 12, "section virtual address")?,
                u32_at(file, at + 16, "section raw size")?,
                u32_at(file, at + 20, "section raw offset")?,
            ));
        }
        Ok(Image { file, directories, directory_count, sections })
    }

    /// RVA of data directory `index`, or `None` when the image has no such table.
    fn directory(&self, index: u32) -> Result<Option<u32>, String> {
        if index >= self.directory_count {
            return Ok(None);
        }
        let rva = u32_at(self.file, self.directories + 8 * u64::from(index), "data directory")?;
        Ok((rva != 0).then_some(rva))
    }

    fn offset_of(&self, rva: u32, what: &str) -> Result<u64, String> {
        self.sections
            .iter()
            .find(|(start, raw_size, _)| rva >= *start && rva - *start < *raw_size)
            .map(|(start, _, raw_offset)| u64::from(*raw_offset) + u64::from(rva - *start))
            .ok_or_else(|| format!("{what}: RVA {rva:#x} is in no section's file data"))
    }

    fn name_at(&self, rva: u32) -> Result<String, String> {
        let start = self.offset_of(rva, "DLL name")?;
        let rest = usize::try_from(start)
            .ok()
            .and_then(|start| self.file.get(start..))
            .ok_or_else(|| format!("DLL name: offset {start:#x} lies outside the {}-byte file", self.file.len()))?;
        let len = rest.iter().position(|b| *b == 0).ok_or_else(|| format!("DLL name at offset {start:#x} has no terminating NUL"))?;
        let name = &rest[..len];
        if name.is_empty() || !name.iter().all(|b| b.is_ascii_graphic()) {
            return Err(format!("DLL name at offset {start:#x} is not printable ASCII: {name:?}"));
        }
        Ok(String::from_utf8_lossy(name).into_owned())
    }

    /// DLL names of a descriptor table: records of `size` bytes with the name RVA at
    /// `name_field`, ended by an all-zero record.
    fn descriptor_names(&self, table_rva: u32, size: usize, name_field: usize, what: &str) -> Result<Vec<String>, String> {
        let mut at = self.offset_of(table_rva, what)?;
        let mut names = Vec::new();
        loop {
            let descriptor = bytes_at(self.file, at, size, what)?;
            if descriptor.iter().all(|b| *b == 0) {
                return Ok(names);
            }
            if what == "delay-load descriptor" && descriptor[0] & 1 == 0 {
                return Err(format!("{what} at offset {at:#x} holds virtual addresses, not RVAs (pre-VC7 layout)"));
            }
            let name_rva = u32::from_le_bytes(descriptor[name_field..name_field + 4].try_into().unwrap());
            names.push(self.name_at(name_rva)?);
            at += size as u64;
        }
    }
}

/// Every DLL the image names in its import table (data directory 1) followed by those in
/// its delay-load table (data directory 13), as written in the file.
fn imported_dlls(file: &[u8]) -> Result<Vec<String>, String> {
    let image = Image::parse(file)?;
    let mut dlls = Vec::new();
    if let Some(rva) = image.directory(1)? {
        dlls.extend(image.descriptor_names(rva, 20, 12, "import descriptor")?);
    }
    if let Some(rva) = image.directory(13)? {
        dlls.extend(image.descriptor_names(rva, 32, 4, "delay-load descriptor")?);
    }
    Ok(dlls)
}

fn assert_no_c_runtime_dll(what: &str, path: &Path) {
    let file = std::fs::read(path).unwrap_or_else(|e| panic!("{what}: cannot read {}: {e}", path.display()));
    let dlls = imported_dlls(&file).unwrap_or_else(|e| panic!("{what} ({}): {e}", path.display()));
    // Every Windows program imports kernel32; without it the table was not really read.
    assert!(
        dlls.iter().any(|dll| dll.eq_ignore_ascii_case("kernel32.dll")),
        "{what} ({}): kernel32.dll is not among the imports read, so the import table was not found; read: {dlls:?}",
        path.display()
    );
    let runtime: Vec<&str> = dlls.iter().map(String::as_str).filter(|dll| is_c_runtime(dll)).collect();
    assert!(
        runtime.is_empty(),
        "{what} ({}) imports the C runtime from {runtime:?}: it will not start without the Visual C++ redistributable. \
         Is `-C target-feature=+crt-static` still set in .cargo/config.toml? All imports: {dlls:?}",
        path.display()
    );
}

#[test]
fn the_playr_executable_imports_no_c_runtime_dll() {
    assert_no_c_runtime_dll("playr.exe", Path::new(env!("CARGO_BIN_EXE_playr")));
}

#[test]
fn this_test_executable_imports_no_c_runtime_dll() {
    let own = std::env::current_exe().expect("path of the running test executable");
    assert_no_c_runtime_dll("the test executable", &own);
}

// ---------------------------------------------------------------------------------------------
// The reader and the deny-list themselves, on hand-made input.

#[test]
fn only_c_runtime_dll_names_are_refused_whatever_their_case() {
    for (dll, refused) in [
        ("VCRUNTIME140.dll", true),
        ("vcruntime140_1.dll", true),
        ("MSVCP140.dll", true),
        ("ucrtbase.dll", true),
        ("UCRTBASED.DLL", true),
        ("api-ms-win-crt-runtime-l1-1-0.dll", true),
        ("API-MS-WIN-CRT-heap-l1-1-0.dll", true),
        ("MSVCR120.dll", true),
        ("msvcrt.dll", true),
        ("api-ms-win-core-synch-l1-2-0.dll", false),
        ("api-ms-win-core-winrt-error-l1-1-0.dll", false),
        ("KERNEL32.dll", false),
        ("OPENGL32.dll", false),
        ("bcryptprimitives.dll", false),
        ("not-vcruntime140.dll", false),
    ] {
        assert_eq!(is_c_runtime(dll), refused, "{dll}");
    }
}

/// A minimal image with two sections whose RVAs differ from their file offsets. `.rdata`
/// (RVA 0x2000, file offset 0x600) holds an import table naming `KERNEL32.dll` and
/// `VCRUNTIME140.dll` and a delay-load table naming `api-ms-win-crt-heap-l1-1-0.dll`.
/// The file ends with the NUL of the last name.
fn crafted_image(magic: u16) -> Vec<u8> {
    fn put16(file: &mut [u8], at: usize, value: u16) {
        file[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put32(file: &mut [u8], at: usize, value: u32) {
        file[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    let pe = 0x80;
    let optional = pe + 24;
    let directories = optional + if magic == 0x20B { 112 } else { 96 };
    let optional_size = directories + 16 * 8 - optional;
    let section_table = optional + optional_size;

    let mut file = vec![0u8; 0x600];
    file[..2].copy_from_slice(b"MZ");
    put32(&mut file, 0x3C, pe as u32);
    file[pe..pe + 4].copy_from_slice(b"PE\0\0");
    put16(&mut file, pe + 6, 2);
    put16(&mut file, pe + 20, optional_size as u16);
    put16(&mut file, optional, magic);
    put32(&mut file, directories - 4, 16);
    put32(&mut file, directories + 8, 0x2000);
    put32(&mut file, directories + 8 + 4, 60);
    put32(&mut file, directories + 8 * 13, 0x2040);
    put32(&mut file, directories + 8 * 13 + 4, 64);
    for (n, (name, rva, raw_size, raw_offset)) in [(b".text\0\0\0", 0x1000, 0x200, 0x400), (b".rdata\0\0", 0x2000, 0x200, 0x600)].into_iter().enumerate() {
        let at = section_table + 40 * n;
        file[at..at + 8].copy_from_slice(name);
        put32(&mut file, at + 8, raw_size);
        put32(&mut file, at + 12, rva);
        put32(&mut file, at + 16, raw_size);
        put32(&mut file, at + 20, raw_offset);
    }

    // .rdata: import descriptors at 0x00, delay-load descriptors at 0x40, names from 0x100.
    let mut rdata = vec![0u8; 0x100];
    let mut name_rvas = Vec::new();
    for name in ["KERNEL32.dll", "VCRUNTIME140.dll", "api-ms-win-crt-heap-l1-1-0.dll"] {
        name_rvas.push(0x2000 + rdata.len() as u32);
        rdata.extend_from_slice(name.as_bytes());
        rdata.push(0);
    }
    for n in 0..2 {
        put32(&mut rdata, 20 * n, 0x2F00 + n as u32);
        put32(&mut rdata, 20 * n + 12, name_rvas[n]);
    }
    put32(&mut rdata, 0x40, 1);
    put32(&mut rdata, 0x40 + 4, name_rvas[2]);
    file.extend_from_slice(&rdata);
    file
}

#[test]
fn the_reader_lists_the_import_table_and_then_the_delay_load_table_of_both_pe_layouts() {
    for (layout, magic) in [("PE32+", 0x20B), ("PE32", 0x10B)] {
        assert_eq!(
            imported_dlls(&crafted_image(magic)),
            Ok(vec!["KERNEL32.dll".to_string(), "VCRUNTIME140.dll".to_string(), "api-ms-win-crt-heap-l1-1-0.dll".to_string()]),
            "{layout}"
        );
    }
}

#[test]
fn an_image_without_a_delay_load_table_lists_its_import_table_alone() {
    let mut image = crafted_image(0x20B);
    let delay_directory = 0x80 + 24 + 112 + 8 * 13;
    image[delay_directory..delay_directory + 8].fill(0);
    assert_eq!(imported_dlls(&image), Ok(vec!["KERNEL32.dll".to_string(), "VCRUNTIME140.dll".to_string()]));
}

#[test]
fn a_file_cut_anywhere_is_an_error_and_never_a_shorter_import_list() {
    for magic in [0x20B, 0x10B] {
        let image = crafted_image(magic);
        for cut in 0..image.len() {
            let read = imported_dlls(&image[..cut]);
            assert!(read.is_err(), "magic {magic:#x}, first {cut} of {} bytes: read {read:?}", image.len());
        }
    }
}

#[test]
fn a_file_that_is_no_pe_image_is_refused_with_the_reason() {
    let mut wrong_magic = crafted_image(0x20B);
    wrong_magic[0x80 + 24..0x80 + 26].copy_from_slice(&0x107u16.to_le_bytes());
    let mut no_pe = crafted_image(0x20B);
    no_pe[0x80..0x84].copy_from_slice(b"NE\0\0");
    let mut stray_name = crafted_image(0x20B);
    stray_name[0x600 + 20 + 12..0x600 + 20 + 16].copy_from_slice(&0x5000u32.to_le_bytes());
    for (case, file, reason) in [
        ("empty file", Vec::new(), "DOS signature: 2 bytes at offset 0x0 lie outside the 0-byte file"),
        ("text file", b"#!/bin/sh\n".to_vec(), "not an executable: no MZ signature at offset 0"),
        ("DOS header alone", b"MZ\x90\0".to_vec(), "e_lfanew: 4 bytes at offset 0x3c lie outside the 4-byte file"),
        ("no PE signature", no_pe, "no PE signature at offset 0x80"),
        ("ROM image magic", wrong_magic, "optional header magic 0x107 is neither PE32 (0x10b) nor PE32+ (0x20b)"),
        ("name RVA outside every section", stray_name, "DLL name: RVA 0x5000 is in no section's file data"),
    ] {
        assert_eq!(imported_dlls(&file), Err(reason.to_string()), "{case}");
    }
}
