# Project notes

- `flasher/` — desktop app (eframe/egui 0.36), the only member of the root Cargo workspace.
- `rp2040-payload/` — standalone no_std firmware (excluded from the workspace, own `Cargo.lock`,
  default target `thumbv6m-none-eabi` via `rp2040-payload/.cargo/config.toml`).
- `flasher/build.rs` builds the payload (release, into `target/<profile>/../rp2040-payload`),
  converts the ELF to UF2 and embeds it (`flasher/src/payload.rs`, `PAYLOAD_UF2`).

- `protocol/` (`flasher-protocol`) — shared no_std wire protocol (workspace member, path dep of the
  payload, host tests: `cargo test -p flasher-protocol`). Design notes: `docs/PROTOCOL.md`.
- The payload is a RAM-only image (memory.x at 0x20000000, no boot2): it must never write to flash.

Commands:
- `cargo build` / `cargo run` in the root — builds the flasher (and the payload through build.rs).
- `cargo build` inside `rp2040-payload/` — firmware only; `cargo run` there flashes via `elf2uf2-rs -d`.

Notes:
- build.rs strips `CARGO_*`/`RUSTFLAGS` env vars before the nested cargo call; otherwise the outer
  `CARGO_ENCODED_RUSTFLAGS` overrides the payload's linker flags.
- eframe 0.35+ API: implement `App::ui(&mut self, ui: &mut egui::Ui, frame)`, panels use `.show(ui, ..)`.
- Toolchain: stable (1.99 at time of writing), see `rust-toolchain.toml`; eframe 0.36/sysinfo 0.39 need >= 1.95.
