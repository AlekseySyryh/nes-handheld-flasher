//! The write workflow: wait for BOOTSEL -> copy the firmware UF2 (with progress) -> wait until
//! the bootloader has flashed it and the device restarted (the BOOTSEL drive disappears).

use std::{
    fs::File,
    io::Write as _,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool, mpsc::Sender},
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use eframe::egui;
use flasher_protocol::CRC32;

use crate::{
    devices,
    dump::{Event, Reporter, Step, Summary},
    uf2::BLOCK_LEN,
};

const FLASH_TIMEOUT: Duration = Duration::from_secs(120);
/// Copy granularity: each chunk is flushed to the device so the progress bar reflects real flashing.
const CHUNK: usize = 64 * BLOCK_LEN;

pub fn spawn(tx: Sender<Event>, ctx: egui::Context, cancel: Arc<AtomicBool>, uf2: Arc<Vec<u8>>) {
    thread::spawn(move || {
        let rep = Reporter::new(tx, ctx, cancel);
        let result = run(&rep, &uf2).map_err(|e| format!("{e:#}"));
        rep.send(Event::Finished(result));
    });
}

fn run(rep: &Reporter, uf2: &[u8]) -> Result<Summary> {
    rep.step(Step::WaitBootsel);
    let drive = rep.wait_for(
        None,
        "устройство в режиме BOOTSEL",
        devices::find_bootsel_drives_first,
    )?;
    rep.log(format!(
        "BOOTSEL: {} (Board-ID {})",
        drive.mount_point.display(),
        drive.board_id
    ));

    rep.step(Step::Flash);
    let target = drive.mount_point.join("firmware.uf2");
    rep.log(format!("Копирую {} байт в {}", uf2.len(), target.display()));
    let drive_present = || {
        devices::find_bootsel_drives()
            .iter()
            .any(|d| d.mount_point == drive.mount_point)
    };
    let total = uf2.len().div_ceil(BLOCK_LEN) as u32;
    let mut file = File::create(&target).context("создание файла на диске BOOTSEL")?;
    let chunks = uf2.len().div_ceil(CHUNK);
    let mut done = 0;
    for (i, chunk) in uf2.chunks(CHUNK).enumerate() {
        if let Err(e) = file.write_all(chunk).and_then(|_| file.sync_data()) {
            // The bootloader reboots as soon as the last block is in, which may fail the final call.
            thread::sleep(Duration::from_millis(500));
            if i + 1 == chunks && !drive_present() {
                rep.log(format!(
                    "Последняя запись завершилась ошибкой ({e}), но диск исчез - прошивка принята"
                ));
                break;
            }
            bail!(
                "запись прервана на {}%: {e}. Не отпускайте Start до окончания записи. \
                 Повторите запись; если консоль не запускается, восстановите backup.uf2",
                done * 100 / uf2.len()
            );
        }
        done += chunk.len();
        rep.send(Event::Progress {
            done: done.div_ceil(BLOCK_LEN) as u32,
            total,
        });
    }
    drop(file);
    rep.send(Event::Progress { done: total, total });

    rep.step(Step::WaitRestart);
    rep.wait_for(
        Some(FLASH_TIMEOUT),
        "завершение прошивки (диск BOOTSEL не исчез)",
        || (!drive_present()).then_some(()),
    )?;
    rep.log("Диск BOOTSEL исчез: устройство прошито и перезапущено");

    rep.step(Step::Finished);
    Ok(Summary {
        path: PathBuf::from(&target),
        size: uf2.len(),
        crc32: CRC32.checksum(uf2),
        retries: 0,
        elapsed: rep.elapsed(),
    })
}
