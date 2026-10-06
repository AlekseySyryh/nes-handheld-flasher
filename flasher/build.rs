//! Builds `rp2040-payload` for the RP2040 and embeds it into the flasher as UF2.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use object::{
    Endianness,
    elf::{FileHeader32, PT_LOAD},
    read::elf::{FileHeader, ProgramHeader},
};

const PAYLOAD_TARGET: &str = "thumbv6m-none-eabi";
const PAYLOAD_NAME: &str = "rp2040-payload";
/// Files of the payload project that should trigger a rebuild.
const PAYLOAD_INPUTS: &[&str] = &["src", "Cargo.toml", "Cargo.lock", "build.rs", "memory.x", ".cargo/config.toml"];

const UF2_MAGIC_START0: u32 = 0x0A32_4655;
const UF2_MAGIC_START1: u32 = 0x9E5D_5157;
const UF2_MAGIC_END: u32 = 0x0AB1_6F30;
const UF2_FLAG_FAMILY_ID_PRESENT: u32 = 0x0000_2000;
const RP2040_FAMILY_ID: u32 = 0xE48B_FF56;
const UF2_PAGE_SIZE: u32 = 256;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let payload_dir = manifest_dir.join("..").join(PAYLOAD_NAME);

    for input in PAYLOAD_INPUTS {
        println!("cargo:rerun-if-changed={}", payload_dir.join(input).display());
    }
    // Path dependency of the payload.
    println!("cargo:rerun-if-changed={}", manifest_dir.join("..").join("protocol").display());

    // OUT_DIR = <target>/<profile>/build/<pkg>-<hash>/out; keep the payload build next to
    // the host artifacts so it survives build script reruns and is reused incrementally.
    let target_dir = out_dir.ancestors().nth(4).unwrap().join(PAYLOAD_NAME);
    let elf_path = build_payload(&payload_dir, &target_dir);

    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("reading {}: {e}", elf_path.display()));
    fs::write(out_dir.join(format!("{PAYLOAD_NAME}.uf2")), elf_to_uf2(&elf)).unwrap();
}

fn build_payload(payload_dir: &Path, target_dir: &Path) -> PathBuf {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(payload_dir)
        .args(["build", "--release", "--target", PAYLOAD_TARGET, "--target-dir"])
        .arg(target_dir);

    // Variables set by the outer cargo for this build script would leak into the nested build
    // (e.g. CARGO_ENCODED_RUSTFLAGS overrides the payload's .cargo/config.toml rustflags).
    for (key, _) in env::vars_os() {
        let key = key.to_string_lossy();
        if (key.starts_with("CARGO_") && key != "CARGO_HOME" && key != "CARGO_MAKEFLAGS")
            || matches!(&*key, "RUSTFLAGS" | "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER")
        {
            cmd.env_remove(&*key);
        }
    }

    let status = cmd.status().expect("failed to spawn cargo for the payload");
    assert!(status.success(), "building {PAYLOAD_NAME} failed");
    target_dir.join(PAYLOAD_TARGET).join("release").join(PAYLOAD_NAME)
}

/// Converts loadable ELF segments (by physical/load address) into RP2040 UF2 blocks.
fn elf_to_uf2(elf: &[u8]) -> Vec<u8> {
    let header = FileHeader32::<Endianness>::parse(elf).expect("payload is not a 32-bit ELF");
    let endian = header.endian().unwrap();

    let mut pages: BTreeMap<u32, [u8; UF2_PAGE_SIZE as usize]> = BTreeMap::new();
    for ph in header.program_headers(endian, elf).unwrap() {
        if ph.p_type(endian) != PT_LOAD || ph.p_filesz(endian) == 0 {
            continue;
        }
        let data = ph.data(endian, elf).unwrap();
        let base = ph.p_paddr(endian);
        for (i, byte) in data.iter().enumerate() {
            let addr = base + i as u32;
            let page = pages.entry(addr & !(UF2_PAGE_SIZE - 1)).or_insert([0; UF2_PAGE_SIZE as usize]);
            page[(addr % UF2_PAGE_SIZE) as usize] = *byte;
        }
    }
    assert!(!pages.is_empty(), "payload ELF has no loadable segments");

    let total = pages.len() as u32;
    let mut uf2 = Vec::with_capacity(pages.len() * 512);
    for (block_no, (addr, page)) in pages.iter().enumerate() {
        for word in [
            UF2_MAGIC_START0,
            UF2_MAGIC_START1,
            UF2_FLAG_FAMILY_ID_PRESENT,
            *addr,
            UF2_PAGE_SIZE,
            block_no as u32,
            total,
            RP2040_FAMILY_ID,
        ] {
            uf2.extend_from_slice(&word.to_le_bytes());
        }
        uf2.extend_from_slice(page);
        uf2.resize(uf2.len() + 476 - UF2_PAGE_SIZE as usize, 0);
        uf2.extend_from_slice(&UF2_MAGIC_END.to_le_bytes());
    }
    uf2
}
