//! Parser of the game list stored in the console's flash dump.
//!
//! The firmware keeps a table of 32-byte entries (see `ENTRY_LEN`):
//!
//! | offset | size | meaning                                                    |
//! |--------|------|------------------------------------------------------------|
//! | 0      | 20   | name, terminated/padded with `0x00` or `0xFF`              |
//! | 20     | 4    | constant `00 00 FE CA`                                     |
//! | 24     | 4    | ROM offset in flash (u32 LE, relative to the flash start)  |
//! | 28     | 4    | ROM size in bytes (u32 LE, rounded up to 256)              |
//!
//! The ROMs themselves are iNES images (`NES\x1A` header) located at those offsets.
//! The table is located by scanning the dump for the longest run of valid entries, so its
//! position is not hardcoded.

use anyhow::{Result, bail};

const ENTRY_LEN: usize = 32;
const NAME_LEN: usize = 20;
const ENTRY_MAGIC: [u8; 4] = [0x00, 0x00, 0xFE, 0xCA];
const INES_MAGIC: [u8; 4] = *b"NES\x1A";
const MIN_TABLE_ENTRIES: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ines {
    pub prg_kib: u32,
    pub chr_kib: u32,
    pub mapper: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub name: String,
    pub rom_offset: u32,
    pub rom_size: u32,
    /// `None` if the data at `rom_offset` is not a valid iNES image.
    pub ines: Option<Ines>,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub table_offset: usize,
    pub games: Vec<Game>,
}

pub fn parse(dump: &[u8]) -> Result<Catalog> {
    let mut best = (0, 0);
    let mut pos = 0;
    while pos + ENTRY_LEN <= dump.len() {
        let run = (pos..dump.len() - ENTRY_LEN + 1)
            .step_by(ENTRY_LEN)
            .take_while(|&p| parse_entry(dump, p).is_some())
            .count();
        if run > best.1 {
            best = (pos, run);
        }
        pos += ENTRY_LEN * run.max(1);
    }
    let (table_offset, count) = best;
    if count < MIN_TABLE_ENTRIES {
        bail!("таблица игр не найдена в дампе");
    }
    let games = (0..count)
        .map(|i| {
            let mut game = parse_entry(dump, table_offset + i * ENTRY_LEN).unwrap();
            let start = game.rom_offset as usize;
            game.ines = parse_ines(&dump[start..start + game.rom_size as usize]);
            game
        })
        .collect();
    Ok(Catalog {
        table_offset,
        games,
    })
}

fn parse_entry(dump: &[u8], pos: usize) -> Option<Game> {
    let e = dump.get(pos..pos + ENTRY_LEN)?;
    if e[NAME_LEN..NAME_LEN + 4] != ENTRY_MAGIC {
        return None;
    }
    let name_bytes: Vec<u8> = e[..NAME_LEN]
        .iter()
        .copied()
        .take_while(|&b| b != 0x00 && b != 0xFF)
        .collect();
    if name_bytes.is_empty() || !name_bytes.iter().all(|b| (0x20..0x7F).contains(b)) {
        return None;
    }
    let rom_offset = u32::from_le_bytes(e[24..28].try_into().unwrap());
    let rom_size = u32::from_le_bytes(e[28..32].try_into().unwrap());
    let end = (rom_offset as usize).checked_add(rom_size as usize)?;
    if rom_size == 0 || end > dump.len() {
        return None;
    }
    Some(Game {
        name: String::from_utf8(name_bytes).ok()?,
        rom_offset,
        rom_size,
        ines: None,
    })
}

fn parse_ines(rom: &[u8]) -> Option<Ines> {
    if rom.len() < 16 || rom[..4] != INES_MAGIC {
        return None;
    }
    let mut mapper = (rom[6] >> 4) as u16;
    // Old dumps carry garbage ("DiskDude!") in bytes 7..16; the upper mapper nibble is only
    // trustworthy when the tail of the header is clean.
    if rom[12..16] == [0; 4] {
        mapper |= (rom[7] & 0xF0) as u16;
    }
    Some(Ines {
        prg_kib: rom[4] as u32 * 16,
        chr_kib: rom[5] as u32 * 8,
        mapper,
    })
}

/// Maximum length of a game name without the "NN " number prefix.
pub const MAX_BASE_NAME: usize = NAME_LEN - 3;
const ROM_ALIGN: usize = 256;
const SECTOR: usize = 4096;
const FLASH_ERASED: u8 = 0xFF;

#[derive(Debug, Clone)]
pub struct Entry {
    /// Name without the "NN " prefix; the number is derived from the position.
    pub name: String,
    /// iNES image padded with zeros to a multiple of 256 bytes.
    pub rom: Vec<u8>,
    /// The game targets the PAL video system (the console's emulator is NTSC-only).
    pub pal: bool,
}

impl Entry {
    /// The iNES file without the zero padding added for flash alignment.
    pub fn ines_file(&self) -> &[u8] {
        let r = &self.rom;
        if r.len() < 16 || r[..4] != INES_MAGIC {
            return r;
        }
        let trainer = if r[6] & 4 != 0 { 512 } else { 0 };
        let len = 16 + trainer + r[4] as usize * 16 * 1024 + r[5] as usize * 8 * 1024;
        &r[..len.min(r.len())]
    }

    pub fn ines(&self) -> Option<Ines> {
        parse_ines(&self.rom)
    }
}

/// Editable game list built on top of a flash dump. ROMs are stored back-to-back after the
/// table; everything outside the table and the ROM area is preserved byte for byte.
#[derive(Debug, Clone)]
pub struct Library {
    base: Vec<u8>,
    table_offset: usize,
    rom_start: usize,
    area_end: usize,
    pub entries: Vec<Entry>,
}

/// Keeps printable ASCII only and limits the length so that "NN " + name fits the 20-byte field.
pub fn clean_name(s: &str, trim: bool) -> String {
    let s: String = s
        .chars()
        .filter(|c| (' '..='~').contains(c))
        .take(MAX_BASE_NAME)
        .collect();
    if trim { s.trim().to_owned() } else { s }
}

fn strip_number(name: &str) -> &str {
    let b = name.as_bytes();
    if b.len() > 3 && b[0].is_ascii_digit() && b[1].is_ascii_digit() && b[2] == b' ' {
        &name[3..]
    } else {
        name
    }
}

pub struct Prepared {
    pub rom: Vec<u8>,
    pub pal: bool,
}

/// File names of European/PAL releases usually carry a region tag.
fn name_looks_pal(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "europe",
        "pal",
        "australia",
        "germany",
        "france",
        "spain",
        "italy",
        "sweden",
        "(e)",
    ]
    .iter()
    .any(|t| n.contains(t))
}

/// Validates an `.nes` file and returns the ROM image to store (header + data, padded to 256).
pub fn prepare_rom(nes: &[u8]) -> Result<Prepared> {
    let Some(h) = parse_ines(nes) else {
        bail!("файл не является iNES-образом (нет заголовка NES 1A)")
    };
    let nes2 = nes[7] & 0x0C == 0x08;
    let pal = if nes2 {
        nes[12] & 3 == 1
    } else {
        nes[9] & 1 == 1 && nes[12..16] == [0; 4]
    };
    if nes2 && (nes[9] != 0 || nes[8] & 0x0F != 0) {
        bail!("NES 2.0: mapper больше 255 или нестандартные размеры ROM не поддерживаются");
    }
    if h.prg_kib == 0 {
        bail!("в образе нет PRG ROM");
    }
    let trainer = if nes[6] & 4 != 0 { 512 } else { 0 };
    let real = 16 + trainer + (h.prg_kib + h.chr_kib) as usize * 1024;
    if nes.len() < real {
        bail!("файл усечён: {} байт вместо {real}", nes.len());
    }
    let mut rom = nes[..real].to_vec();
    if nes2 {
        // Downgrade the header to plain iNES: the data layout is identical, only the extension
        // fields (submapper, RAM sizes, region, ...) are dropped.
        rom[7] &= !0x0C;
        rom[8..16].fill(0);
    }
    rom.resize(real.next_multiple_of(ROM_ALIGN), 0);
    Ok(Prepared { rom, pal })
}

impl Library {
    pub fn from_dump(dump: Vec<u8>) -> Result<Self> {
        let cat = parse(&dump)?;
        let rom_start = cat
            .games
            .iter()
            .map(|g| g.rom_offset as usize)
            .min()
            .unwrap();
        let rom_end = cat
            .games
            .iter()
            .map(|g| (g.rom_offset + g.rom_size) as usize)
            .max()
            .unwrap();
        if rom_start < cat.table_offset + ENTRY_LEN {
            bail!("неподдерживаемая раскладка: ROM расположены перед таблицей");
        }
        // The ROM area ends where the next non-erased sector (settings / filesystem) begins.
        let area_end = (rom_end.next_multiple_of(SECTOR)..dump.len())
            .step_by(SECTOR)
            .find(|&o| {
                dump[o..(o + SECTOR).min(dump.len())]
                    .iter()
                    .any(|&b| b != FLASH_ERASED)
            })
            .unwrap_or(dump.len());
        let entries = cat
            .games
            .iter()
            .map(|g| {
                let rom =
                    dump[g.rom_offset as usize..(g.rom_offset + g.rom_size) as usize].to_vec();
                let pal = rom[9] & 1 == 1 && rom[12..16] == [0; 4] && rom[..4] == INES_MAGIC;
                Entry {
                    name: strip_number(&g.name).to_owned(),
                    rom,
                    pal,
                }
            })
            .collect();
        Ok(Self {
            base: dump,
            table_offset: cat.table_offset,
            rom_start,
            area_end,
            entries,
        })
    }

    #[cfg(test)]
    pub fn rom_start(&self) -> usize {
        self.rom_start
    }

    /// Flash byte range (sector-aligned) that `build()` may change: the table and the ROM area.
    pub fn write_range(&self) -> (usize, usize) {
        (self.table_offset / SECTOR * SECTOR, self.area_end)
    }

    pub fn max_entries(&self) -> usize {
        (self.rom_start - self.table_offset) / ENTRY_LEN
    }

    pub fn capacity(&self) -> usize {
        self.area_end - self.rom_start
    }

    pub fn used(&self) -> usize {
        self.entries.iter().map(|e| e.rom.len()).sum()
    }

    pub fn numbered_name(&self, i: usize) -> String {
        let mut s = format!("{:02} {}", i + 1, self.entries[i].name);
        s.truncate(NAME_LEN);
        s
    }

    /// Flash offsets of the ROMs as they will be laid out on save.
    pub fn offsets(&self) -> Vec<usize> {
        self.entries
            .iter()
            .scan(self.rom_start, |off, e| {
                let o = *off;
                *off += e.rom.len();
                Some(o)
            })
            .collect()
    }

    fn ensure_fits(&self, new_len: usize, old_len: usize, new_entry: bool) -> Result<()> {
        if new_entry && self.entries.len() >= self.max_entries() {
            bail!("таблица заполнена (максимум {} игр)", self.max_entries());
        }
        let total = self.used() - old_len + new_len;
        if total > self.capacity() {
            bail!(
                "не хватает места: нужно {} КиБ, доступно {} КиБ",
                total / 1024,
                self.capacity() / 1024
            );
        }
        Ok(())
    }

    fn entry_name(name: &str) -> String {
        let name = clean_name(name, true);
        if name.is_empty() { "Game".into() } else { name }
    }

    pub fn insert(&mut self, at: usize, name: &str, nes: &[u8]) -> Result<()> {
        let p = prepare_rom(nes)?;
        self.ensure_fits(p.rom.len(), 0, true)?;
        let entry = Entry {
            name: Self::entry_name(name),
            rom: p.rom,
            pal: p.pal || name_looks_pal(name),
        };
        self.entries.insert(at.min(self.entries.len()), entry);
        Ok(())
    }

    pub fn replace(&mut self, i: usize, name: &str, nes: &[u8]) -> Result<()> {
        let p = prepare_rom(nes)?;
        self.ensure_fits(p.rom.len(), self.entries[i].rom.len(), false)?;
        self.entries[i] = Entry {
            name: Self::entry_name(name),
            rom: p.rom,
            pal: p.pal || name_looks_pal(name),
        };
        Ok(())
    }

    pub fn remove(&mut self, i: usize) {
        self.entries.remove(i);
    }

    /// Swaps the entry with its neighbour; returns the new index.
    pub fn move_entry(&mut self, i: usize, up: bool) -> Option<usize> {
        let j = if up { i.checked_sub(1)? } else { i + 1 };
        (j < self.entries.len()).then(|| {
            self.entries.swap(i, j);
            j
        })
    }

    /// Produces a new flash image: table and ROM area are rewritten, the rest is kept intact.
    pub fn build(&self) -> Result<Vec<u8>> {
        self.ensure_fits(0, 0, false)?;
        if self.entries.len() > self.max_entries() {
            bail!(
                "слишком много игр: {} (максимум {})",
                self.entries.len(),
                self.max_entries()
            );
        }
        let mut out = self.base.clone();
        out[self.table_offset..self.rom_start].fill(FLASH_ERASED);
        out[self.rom_start..self.area_end].fill(FLASH_ERASED);
        for (i, (entry, offset)) in self.entries.iter().zip(self.offsets()).enumerate() {
            out[offset..offset + entry.rom.len()].copy_from_slice(&entry.rom);

            let name = self.numbered_name(i);
            let mut e = [FLASH_ERASED; ENTRY_LEN];
            e[..name.len()].copy_from_slice(name.as_bytes());
            if name.len() < NAME_LEN {
                e[name.len()] = 0;
            }
            e[NAME_LEN..NAME_LEN + 4].copy_from_slice(&ENTRY_MAGIC);
            e[24..28].copy_from_slice(&(offset as u32).to_le_bytes());
            e[28..32].copy_from_slice(&(entry.rom.len() as u32).to_le_bytes());
            let at = self.table_offset + i * ENTRY_LEN;
            out[at..at + ENTRY_LEN].copy_from_slice(&e);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nes_file(prg: u8, chr: u8) -> Vec<u8> {
        let mut f = vec![0x55; 16 + prg as usize * 16384 + chr as usize * 8192];
        f[..4].copy_from_slice(&INES_MAGIC);
        f[4..16].copy_from_slice(&[prg, chr, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        f
    }

    fn synthetic() -> Vec<u8> {
        let mut dump = vec![0xFF; 0x20000];
        dump[0x1000..0x1020].copy_from_slice(&entry("01 A", 0x2000, 0x100));
        dump[0x1020..0x1040].copy_from_slice(&entry("02 B", 0x2100, 0x100));
        dump[0x2000..0x2100].copy_from_slice(&rom(2, 1, 0, 0, [0; 8]));
        dump[0x2100..0x2200].copy_from_slice(&rom(1, 1, 0, 0, [0; 8]));
        dump[0x10000] = 0; // start of the non-ROM data: the ROM area is 0x2000..0x10000
        dump
    }

    #[test]
    fn library_roundtrip_is_identical() {
        let dump = synthetic();
        let lib = Library::from_dump(dump.clone()).unwrap();
        assert_eq!((lib.rom_start(), lib.capacity()), (0x2000, 0xE000));
        assert_eq!(lib.build().unwrap(), dump);
        assert_eq!(lib.write_range(), (0x1000, 0x10000));
    }

    #[test]
    fn reorder_renumbers_and_repacks() {
        let mut lib = Library::from_dump(synthetic()).unwrap();
        assert_eq!(lib.move_entry(1, true), Some(0));
        assert_eq!(lib.move_entry(0, true), None);
        let c = parse(&lib.build().unwrap()).unwrap();
        let names: Vec<_> = c.games.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["01 B", "02 A"]);
        assert_eq!(
            (c.games[0].rom_offset, c.games[1].rom_offset),
            (0x2000, 0x2100)
        );
        assert_eq!(c.games[0].ines.as_ref().unwrap().prg_kib, 16);
    }

    #[test]
    fn insert_replace_remove_and_capacity() {
        let mut lib = Library::from_dump(synthetic()).unwrap();
        lib.insert(1, "Новая игра with a very long name", &nes_file(1, 1))
            .unwrap();
        assert_eq!(lib.numbered_name(1), "02 with a very lon");
        assert_eq!(lib.numbered_name(2), "03 B");
        lib.replace(0, "Repl", &nes_file(1, 0)).unwrap();
        assert_eq!(lib.entries[0].rom.len(), 16640);
        // capacity is 0xE000 (57344): 16640 + 24832 + 256 used, one more 24832 does not fit
        assert!(lib.insert(3, "X", &nes_file(1, 1)).is_err());
        lib.remove(0);
        lib.insert(2, "X", &nes_file(1, 1)).unwrap();
        let c = parse(&lib.build().unwrap()).unwrap();
        assert_eq!(c.games.len(), 3);
        assert_eq!(c.games[0].name, "01 with a very lon");
        assert!(prepare_rom(&[0; 100]).is_err());
        assert!(prepare_rom(&nes_file(1, 1)[..1000]).is_err());
    }

    #[test]
    fn pal_is_detected_from_header_and_from_name() {
        let mut lib = Library::from_dump(synthetic()).unwrap();
        assert!(!lib.entries[0].pal);
        lib.insert(0, "Game (Europe)", &nes_file(1, 0)).unwrap();
        assert!(lib.entries[0].pal);
        let mut f = nes_file(1, 0);
        f[9] = 1;
        lib.replace(0, "Plain", &f).unwrap();
        assert!(lib.entries[0].pal);
        lib.replace(0, "Plain", &nes_file(1, 0)).unwrap();
        assert!(!lib.entries[0].pal);
    }

    fn prng(seed: &mut u32) -> u8 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed >> 24) as u8
    }

    /// A 2 MiB image with the same layout as the console's flash, filled with generated data:
    /// "code" up to 0x64000, game table at 0x64000, packed ROMs from 0x65000 and a non-empty
    /// data area at 0x1F2000. Contains no real firmware or games.
    fn console_like_image() -> Vec<u8> {
        let mut seed = 0x1234_5678;
        let mut img = vec![0xFF; 2 * 1024 * 1024];
        img[..0x64000].iter_mut().for_each(|b| *b = prng(&mut seed));
        img[0x1F2000..0x1F9000]
            .iter_mut()
            .for_each(|b| *b = prng(&mut seed));

        let shapes: [(usize, usize); 6] = [(2, 1), (1, 1), (2, 2), (4, 2), (2, 0), (1, 0)];
        let mut offset = 0x65000;
        for i in 0..21 {
            let (prg, chr) = shapes[i % shapes.len()];
            let real = 16 + prg * 16384 + chr * 8192;
            let size = real.next_multiple_of(256);
            let mut rom: Vec<u8> = (0..size).map(|_| prng(&mut seed)).collect();
            rom[..4].copy_from_slice(&INES_MAGIC);
            rom[4..16].copy_from_slice(&[
                prg as u8,
                chr as u8,
                (i as u8 % 4) << 4,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ]);
            img[offset..offset + size].copy_from_slice(&rom);
            let table = 0x64000 + i * ENTRY_LEN;
            img[table..table + ENTRY_LEN].copy_from_slice(&entry(
                &format!("{:02} Game {}", i + 1, i + 1),
                offset as u32,
                size as u32,
            ));
            offset += size;
        }
        img
    }

    #[test]
    fn console_like_layout_is_parsed_and_roundtrips() {
        let img = console_like_image();
        let lib = Library::from_dump(img.clone()).unwrap();
        assert_eq!(lib.entries.len(), 21);
        assert_eq!((lib.rom_start(), lib.max_entries()), (0x65000, 128));
        assert_eq!(lib.write_range(), (0x64000, 0x1F2000));
        assert_eq!(lib.build().unwrap(), img);
    }

    #[test]
    fn editing_touches_only_the_write_range_and_renumbers() {
        let img = console_like_image();
        let mut lib = Library::from_dump(img.clone()).unwrap();
        lib.move_entry(5, true);
        lib.remove(0);
        lib.insert(3, "New", &nes_file(2, 1)).unwrap();
        let out = lib.build().unwrap();
        let (start, end) = lib.write_range();
        assert_eq!(out[..start], img[..start]);
        assert_eq!(out[end..], img[end..]);

        let c = parse(&out).unwrap();
        assert_eq!(c.games.len(), 21);
        for (i, g) in c.games.iter().enumerate() {
            assert!(g.name.starts_with(&format!("{:02} ", i + 1)), "{}", g.name);
            assert!(g.ines.is_some());
        }
        assert_eq!(c.games[3].name, "04 New");
        assert!(
            c.games
                .windows(2)
                .all(|w| w[0].rom_offset + w[0].rom_size == w[1].rom_offset)
        );
    }

    /// Optional check against a real dump: `FLASHER_REAL_DUMP=<file> cargo test`.
    /// Skipped when the variable is not set; real dumps never belong in the repository.
    #[test]
    fn real_dump_invariants_if_provided() {
        let Some(path) = std::env::var_os("FLASHER_REAL_DUMP") else {
            return;
        };
        let dump = std::fs::read(path).unwrap();
        let lib = Library::from_dump(dump.clone()).unwrap();
        assert!(lib.entries.iter().all(|e| e.ines().is_some()));
        assert_eq!(lib.build().unwrap(), dump);
    }

    fn entry(name: &str, offset: u32, size: u32) -> [u8; ENTRY_LEN] {
        let mut e = [0xFF; ENTRY_LEN];
        e[..name.len()].copy_from_slice(name.as_bytes());
        e[name.len()] = 0;
        e[20..24].copy_from_slice(&ENTRY_MAGIC);
        e[24..28].copy_from_slice(&offset.to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    fn rom(prg: u8, chr: u8, f6: u8, f7: u8, tail: [u8; 8]) -> Vec<u8> {
        let mut r = vec![0xAA; 0x100];
        r[..4].copy_from_slice(&INES_MAGIC);
        r[4..8].copy_from_slice(&[prg, chr, f6, f7]);
        r[8..16].copy_from_slice(&tail);
        r
    }

    #[test]
    fn finds_table_and_parses_games() {
        let mut dump = vec![0xFF; 0x4000];
        dump[0x1000..0x1020].copy_from_slice(&entry("01 Mario", 0x2000, 0x100));
        dump[0x1020..0x1040].copy_from_slice(&entry("02 Broken", 0x2100, 0x100));
        dump[0x2000..0x2100].copy_from_slice(&rom(2, 1, 0x21, 0x00, [0; 8]));
        dump[0x2100..0x2200].copy_from_slice(&[0x11; 0x100]);
        let c = parse(&dump).unwrap();
        assert_eq!(c.table_offset, 0x1000);
        assert_eq!(c.games.len(), 2);
        assert_eq!(c.games[0].name, "01 Mario");
        assert_eq!(
            c.games[0].ines,
            Some(Ines {
                prg_kib: 32,
                chr_kib: 8,
                mapper: 2
            })
        );
        assert_eq!(c.games[1].ines, None);
    }

    #[test]
    fn nes2_header_is_downgraded_to_ines() {
        let mut f = nes_file(2, 1);
        f[6] = 0x10;
        f[7] = 0x08;
        f[12] = 1;
        f[15] = 1;
        let prepared = prepare_rom(&f).unwrap();
        assert!(prepared.pal);
        let rom = prepared.rom;
        assert_eq!(&rom[4..16], &[2, 1, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            parse_ines(&rom).unwrap(),
            Ines {
                prg_kib: 32,
                chr_kib: 8,
                mapper: 1
            }
        );
        assert_eq!(
            rom[16..],
            f[16..]
                .iter()
                .copied()
                .chain(std::iter::repeat_n(0, rom.len() - f.len()))
                .collect::<Vec<_>>()[..]
        );
        f[8] = 0x01; // mapper bits 8..11
        assert!(prepare_rom(&f).is_err());
    }

    #[test]
    fn dirty_header_ignores_upper_mapper_nibble() {
        let r = rom(8, 0, 0x21, 0x44, *b"DiskDude");
        assert_eq!(parse_ines(&r).unwrap().mapper, 2);
        let r = rom(8, 0, 0x21, 0x40, [0; 8]);
        assert_eq!(parse_ines(&r).unwrap().mapper, 0x42);
    }

    #[test]
    fn entries_pointing_outside_the_dump_are_rejected() {
        let mut dump = vec![0xFF; 0x400];
        dump[0..32].copy_from_slice(&entry("01 A", 0x1000, 0x100));
        dump[32..64].copy_from_slice(&entry("02 B", 0x100, 0x100));
        assert!(parse(&dump).is_err());
    }
}
