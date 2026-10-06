mod devices;
mod payload;

use devices::{BootselDrive, UartPort};
use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([640.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RP2040 Flasher",
        options,
        Box::new(|_cc| Ok(Box::new(FlasherApp::new()))),
    )
}

#[derive(Default)]
struct FlasherApp {
    drives: Vec<BootselDrive>,
    ports: Vec<UartPort>,
    error: Option<String>,
}

impl FlasherApp {
    fn new() -> Self {
        let mut app = Self::default();
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.drives = devices::find_bootsel_drives();
        match devices::find_uart_ports() {
            Ok(ports) => {
                self.ports = ports;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Serial port scan failed: {e}")),
        }
    }
}

impl eframe::App for FlasherApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RP2040 Flasher");
                if ui.button("Refresh").clicked() {
                    self.refresh();
                }
            });
            ui.weak(format!("Embedded payload: {} bytes (UF2)", payload::PAYLOAD_UF2.len()));
            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::RED, err);
            }
            ui.separator();

            ui.label(egui::RichText::new("BOOTSEL drives").strong());
            if self.drives.is_empty() {
                ui.weak("No RP2040 in BOOTSEL mode found");
            }
            for d in &self.drives {
                ui.label(format!("{}  ({})", d.mount_point.display(), d.board_id));
            }
            ui.separator();

            ui.label(egui::RichText::new("UART ports").strong());
            if self.ports.is_empty() {
                ui.weak("No serial ports found");
            }
            for p in &self.ports {
                let text = format!("{}  {}", p.name, p.description);
                if p.is_rpi {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, text);
                } else {
                    ui.label(text);
                }
            }
        });
    }
}
