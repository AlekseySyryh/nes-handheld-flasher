//! RAM-resident payload: dumps the contents of the QSPI flash over USB CDC-ACM.
//! Protocol: see `docs/PROTOCOL.md` and the `flasher-protocol` crate.
#![no_std]
#![no_main]

use core::slice;

use cortex_m::singleton;
use flasher_protocol::{
    BLOCK_COUNT, BLOCK_SIZE, ErrorCode, FLASH_SIZE, Kind, MAX_RESPONSE_LEN, Command, Parsed,
    RequestParser, USB_PID, USB_VID, VERSION, encode_response,
};
use panic_halt as _;
use rp2040_hal::{self as hal, pac, rom_data, usb::UsbBus};
use usb_device::{class_prelude::UsbBusAllocator, prelude::*};
use usbd_serial::{SerialPort, USB_CLASS_CDC};

const XTAL_FREQ_HZ: u32 = 12_000_000;
const XIP_BASE: usize = 0x1000_0000;

type Serial = SerialPort<'static, UsbBus>;
type Device = UsbDevice<'static, UsbBus>;

#[hal::entry]
fn main() -> ! {
    // The bootloader leaves the flash out of XIP mode; map it at 0x1000_0000 for reading.
    unsafe {
        rom_data::connect_internal_flash();
        rom_data::flash_exit_xip();
        rom_data::flash_flush_cache();
        rom_data::flash_enter_cmd_xip();
    }

    let mut pac = pac::Peripherals::take().unwrap();
    let mut watchdog = hal::Watchdog::new(pac.WATCHDOG);
    let clocks = hal::clocks::init_clocks_and_plls(
        XTAL_FREQ_HZ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .unwrap();

    let bus = UsbBus::new(pac.USBCTRL_REGS, pac.USBCTRL_DPRAM, clocks.usb_clock, true, &mut pac.RESETS);
    let bus = singleton!(: UsbBusAllocator<UsbBus> = UsbBusAllocator::new(bus)).unwrap();
    let mut serial = SerialPort::new(bus);
    let mut dev = UsbDeviceBuilder::new(bus, UsbVidPid(USB_VID, USB_PID))
        .strings(&[StringDescriptors::default()
            .manufacturer("jeree")
            .product("Flash dumper")
            .serial_number("0001")])
        .unwrap()
        .device_class(USB_CLASS_CDC)
        .build();

    let mut parser = RequestParser::new();
    let mut frame = [0u8; MAX_RESPONSE_LEN];
    let mut rx = [0u8; 64];
    loop {
        if !dev.poll(&mut [&mut serial]) {
            continue;
        }
        let Ok(n) = serial.read(&mut rx) else { continue };
        for &byte in &rx[..n] {
            let Some(parsed) = parser.push(byte) else { continue };
            let (len, reboot) = respond(parsed, &mut frame);
            send(&mut dev, &mut serial, &frame[..len]);
            if reboot {
                reboot_to_flash();
            }
        }
    }
}

/// Builds the response for a request; returns the frame length and whether to reboot afterwards.
fn respond(parsed: Parsed, out: &mut [u8]) -> (usize, bool) {
    let error = |out: &mut [u8], code: ErrorCode| encode_response(out, Kind::Error, &[&[code as u8]]);
    match parsed {
        Parsed::UnknownCommand => (error(out, ErrorCode::UnknownCommand), false),
        Parsed::Request(req) => match req.command {
            Command::Hello => {
                let info = [&[VERSION][..], &FLASH_SIZE.to_le_bytes(), &(BLOCK_SIZE as u32).to_le_bytes()];
                (encode_response(out, Kind::Info, &info), false)
            }
            Command::GetBlock if req.arg < BLOCK_COUNT => {
                let addr = XIP_BASE + req.arg as usize * BLOCK_SIZE;
                // SAFETY: read-only XIP window, the block lies within the 2 MiB flash.
                let data = unsafe { slice::from_raw_parts(addr as *const u8, BLOCK_SIZE) };
                (encode_response(out, Kind::Block, &[&req.arg.to_le_bytes(), data]), false)
            }
            Command::GetBlock => (error(out, ErrorCode::BlockOutOfRange), false),
            Command::Reboot => (encode_response(out, Kind::Ok, &[]), true),
        },
    }
}

/// Blocking send: pushes `data` through the CDC endpoint and waits until it is fully flushed.
/// Gives up if the host has gone away.
fn send(dev: &mut Device, serial: &mut Serial, mut data: &[u8]) {
    while !data.is_empty() {
        dev.poll(&mut [serial]);
        if dev.state() != UsbDeviceState::Configured {
            return;
        }
        match serial.write(data) {
            Ok(n) => data = &data[n..],
            Err(UsbError::WouldBlock) => {}
            Err(_) => return,
        }
    }
    while serial.flush().is_err() {
        dev.poll(&mut [serial]);
        if dev.state() != UsbDeviceState::Configured {
            return;
        }
    }
}

/// Full chip reset (everything except the oscillators), so the bootrom boots the flash image.
fn reboot_to_flash() -> ! {
    // Let the host see the final ACK before the USB device disappears.
    cortex_m::asm::delay(12_500_000);
    // SAFETY: we are about to reset the chip; no other users of these registers exist.
    let pac = unsafe { pac::Peripherals::steal() };
    // Scratch4 holds the bootrom's "reboot into address" magic; make sure it is cleared.
    pac.WATCHDOG.scratch4().write(|w| unsafe { w.bits(0) });
    // Reset everything except XOSC (bit 0) and ROSC (bit 1), like pico-sdk's watchdog_reboot.
    pac.PSM.wdsel().write(|w| unsafe { w.bits(0x0001_ffff & !0b11) });
    pac.WATCHDOG.ctrl().write(|w| w.trigger().set_bit());
    loop {
        cortex_m::asm::nop();
    }
}
