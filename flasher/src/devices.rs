use std::path::PathBuf;

use serialport::SerialPortType;
use sysinfo::Disks;

/// Raspberry Pi USB vendor ID.
pub const RPI_VID: u16 = 0x2E8A;

#[derive(Debug, Clone)]
pub struct BootselDrive {
    pub mount_point: PathBuf,
    pub board_id: String,
}

#[derive(Debug, Clone)]
pub struct UartPort {
    pub name: String,
    pub description: String,
    pub is_rpi: bool,
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

/// Lists available serial (UART / USB CDC) ports.
pub fn find_uart_ports() -> anyhow::Result<Vec<UartPort>> {
    Ok(serialport::available_ports()?
        .into_iter()
        .map(|p| {
            let (description, is_rpi) = match &p.port_type {
                SerialPortType::UsbPort(usb) => (
                    format!(
                        "USB {:04X}:{:04X} {}",
                        usb.vid,
                        usb.pid,
                        usb.product.as_deref().unwrap_or("")
                    ),
                    usb.vid == RPI_VID,
                ),
                SerialPortType::PciPort => ("PCI".into(), false),
                SerialPortType::BluetoothPort => ("Bluetooth".into(), false),
                SerialPortType::Unknown => ("Unknown".into(), false),
            };
            UartPort { name: p.port_name, description, is_rpi }
        })
        .collect())
}
