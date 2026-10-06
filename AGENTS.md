# Project notes

- `flasher/` — desktop app (eframe/egui 0.36), the only member of the root Cargo workspace.
- `rp2040-payload/` — standalone no_std firmware (excluded from the workspace, own `Cargo.lock`,
  default target `thumbv6m-none-eabi` via `rp2040-payload/.cargo/config.toml`).
- `flasher/build.rs` builds the payload (release, into `target/<profile>/../rp2040-payload`),
  converts the ELF to UF2 and embeds it (`flasher/src/payload.rs`, `PAYLOAD_UF2`).

- `protocol/` (`flasher-protocol`) — shared no_std wire protocol (workspace member, path dep of the
  payload, host tests: `cargo test -p flasher-protocol`). Design notes: `docs/PROTOCOL.md`.
- The payload is a RAM-only image (memory.x at 0x20000000, no boot2): it must never write to flash.

- `flasher/src/catalog.rs` — parser of the game table in the console dump (32-byte entries: name[20],
  `00 00 FE CA`, rom offset u32, rom size u32; table at 0x64000, iNES ROMs from 0x65000). UI has two
  screens: "Игры" (game list from a dump file) and "Чтение прошивки" (BOOTSEL -> payload -> dump).
- Writing firmware back is planned via UF2 in BOOTSEL (not through the payload protocol).

- Startup: if `backup.uf2` or `rp2040_dump.bin` is missing next to the executable, the
  app shows the backup screen and runs the dump automatically (no reboot, user holds Start), writing
  `rp2040_dump.bin` then `backup.uf2` (atomic rename). `flasher/src/uf2.rs` wraps a flash image into UF2.

- Write flow (`flasher/src/write.rs`, screen `Screen::Write`): "Записать на приставку" builds the edited image,
  wraps only `Library::write_range()` (table + ROM area, sector-aligned) into UF2, waits for BOOTSEL, copies
  `firmware.uf2` to the drive and waits for the drive to vanish. Refuses to run without `backup.uf2`.
  Restore = copy `backup.uf2` (full image) to a BOOTSEL drive.

Commands:
- `cargo build` / `cargo run` in the root — builds the flasher (and the payload through build.rs).
- `cargo build` inside `rp2040-payload/` — firmware only; `cargo run` there flashes via `elf2uf2-rs -d`.

Notes:
- build.rs strips `CARGO_*`/`RUSTFLAGS` env vars before the nested cargo call; otherwise the outer
  `CARGO_ENCODED_RUSTFLAGS` overrides the payload's linker flags.
- eframe 0.35+ API: implement `App::ui(&mut self, ui: &mut egui::Ui, frame)`, panels use `.show(ui, ..)`.
- Toolchain: stable (1.99 at time of writing), see `rust-toolchain.toml`; eframe 0.36/sysinfo 0.39 need >= 1.95.

Tests and data hygiene:
- Never commit or use by default real dumps, firmware or ROMs (`*.bin`, `*.uf2`, `*.nes` are gitignored).
- Tests use generated images (`console_like_image()` in `catalog.rs`). Optional check on a real dump:
  `FLASHER_REAL_DUMP=<path> cargo test -p flasher real_dump` (skipped when the variable is not set).
- Optional local guard: `git config core.hooksPath .githooks` enables `.githooks/pre-commit`, which rejects
  staged `*.bin`/`*.uf2`/`*.nes` files and anything starting with the iNES magic. After the first
  commit of the hook on Linux/macOS run `git update-index --chmod=+x .githooks/pre-commit`.

Hardware facts learned on the real console (verified by the user):
- Entering BOOTSEL: console must be OFF first (long-press Start). Then USB, hold Menu, press Start,
  release Menu, keep holding Start. Menu is the BOOTSEL selector; Start acts as power: releasing it during
  BOOTSEL (e.g. mid-UF2 copy) drops the device out of BOOTSEL. Keep Start held until the tool reports completion.
- `Reboot` from the payload works while Start is still held (the dump/backup flows send it at the end).
- The emulator is NTSC-only: PAL ROMs run fast (music too). The UI flags PAL ROMs (header or name tags).
- ROMs must be iNES; NES 2.0 headers are downgraded to iNES 1 on import (mapper > 255 rejected).
- `backup.uf2` (full 2 MiB flash image) is never overwritten; `rp2040_dump.bin` is overwritten by every dump.
  Restore = BOOTSEL + copy `backup.uf2` to the drive. Writing edits only touches table + ROM area.
- Flash layout: code < 0x64000, game table 0x64000 (max 128 entries), packed ROMs from 0x65000,
  settings/data area from 0x1F2000 (changes by itself while the console runs; not touched by writes).
