use std::path::PathBuf;

use flasher_protocol::{USB_PID, USB_VID};
use serialport::SerialPortType;
use sysinfo::Disks;

#[derive(Debug, Clone)]
pub struct BootselDrive {
    pub mount_point: PathBuf,
    pub board_id: String,
}

/// Finds mounted RP2040 BOOTSEL mass storage drives (identified by `INFO_UF2.TXT`).
pub fn find_bootsel_drives() -> Vec<BootselDrive> {
    Disks::new_with_refreshed_list()
        .iter()
        .filter_map(|disk| {
            let mount_point = disk.mount_point().to_path_buf();
            let info = std::fs::read_to_string(mount_point.join("INFO_UF2.TXT")).ok()?;
            let board_id = info
                .lines()
                .find_map(|l| l.strip_prefix("Board-ID:"))
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|| "unknown".into());
            Some(BootselDrive { mount_point, board_id })
        })
        .collect()
}

pub fn find_bootsel_drives_first() -> Option<BootselDrive> {
    find_bootsel_drives().into_iter().next()
}

/// Finds the serial port exposed by the running payload (by USB VID/PID).
pub fn find_payload_port() -> Option<String> {
    serialport::available_ports()
        .ok()?
        .into_iter()
        .find(|p| matches!(&p.port_type, SerialPortType::UsbPort(u) if u.vid == USB_VID && u.pid == USB_PID))
        .map(|p| p.port_name)
}
