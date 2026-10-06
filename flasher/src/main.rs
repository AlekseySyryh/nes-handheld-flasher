mod devices;
mod dump;
mod payload;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
};

use dump::{Event, Step, Summary};
use eframe::egui::{self, Color32, RichText};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([720.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native("RP2040 Flasher", options, Box::new(|_cc| Ok(Box::new(FlasherApp::new()))))
}

struct Run {
    rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
}

struct FlasherApp {
    out_path: String,
    run: Option<Run>,
    step: Option<Step>,
    progress: (u32, u32),
    log: Vec<String>,
    result: Option<Result<Summary, String>>,
}

impl FlasherApp {
    fn new() -> Self {
        Self {
            out_path: "rp2040_dump.bin".into(),
            run: None,
            step: None,
            progress: (0, 0),
            log: Vec::new(),
            result: None,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        dump::spawn(tx, ctx.clone(), cancel.clone(), PathBuf::from(self.out_path.trim()));
        self.run = Some(Run { rx, cancel });
        self.step = Some(Step::WaitBootsel);
        self.progress = (0, 0);
        self.log.clear();
        self.result = None;
    }

    fn poll_events(&mut self) {
        let Some(run) = &self.run else { return };
        let mut finished = false;
        while let Ok(ev) = run.rx.try_recv() {
            match ev {
                Event::Step(s) => self.step = Some(s),
                Event::Log(l) => self.log.push(l),
                Event::Progress { done, total } => self.progress = (done, total),
                Event::Finished(r) => {
                    if let Err(e) = &r {
                        self.log.push(format!("ОШИБКА: {e}"));
                    }
                    self.result = Some(r);
                    finished = true;
                }
            }
        }
        if finished {
            self.run = None;
        }
    }
}

impl eframe::App for FlasherApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_events();
        let ctx = ui.ctx().clone();
        let running = self.run.is_some();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("RP2040 Flasher");
            ui.separator();

            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Сохранить прошивку в:");
                    ui.text_edit_singleline(&mut self.out_path);
                });
            });
            if !running && self.step.is_none() {
                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.label("Переведите устройство в режим BOOTSEL:");
                    ui.label("1. Отключите устройство от USB.");
                    ui.label("2. Зажмите кнопку BOOTSEL.");
                    ui.label("3. Подключите USB и отпустите кнопку.");
                    ui.weak("Можно нажать «Начать» до этого: программа дождётся устройства.");
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if let Some(run) = &self.run {
                    if ui.button("Отмена").clicked() {
                        run.cancel.store(true, Ordering::Relaxed);
                    }
                } else if ui.button("Начать").clicked() {
                    self.start(&ctx);
                }
            });
            ui.separator();

            if let Some(current) = self.step {
                for step in Step::ALL {
                    let failed = matches!(self.result, Some(Err(_))) && step == current;
                    let (mark, color) = if current == Step::Finished || step < current {
                        ("[x]", Color32::LIGHT_GREEN)
                    } else if failed {
                        ("[!]", Color32::LIGHT_RED)
                    } else if step == current {
                        ("[>]", Color32::YELLOW)
                    } else {
                        ("[ ]", Color32::GRAY)
                    };
                    ui.label(RichText::new(format!("{mark} {}", step.label())).color(color));
                }
                if self.progress.1 > 0 {
                    let (done, total) = self.progress;
                    ui.add(
                        egui::ProgressBar::new(done as f32 / total as f32)
                            .text(format!("{done}/{total} блоков")),
                    );
                }
            }

            match &self.result {
                Some(Ok(s)) => {
                    ui.add_space(6.0);
                    ui.colored_label(
                        Color32::LIGHT_GREEN,
                        format!(
                            "Готово: {} ({} байт, CRC-32 {:08X}), повторов {}, {:.1} с",
                            s.path.display(),
                            s.size,
                            s.crc32,
                            s.retries,
                            s.elapsed.as_secs_f32()
                        ),
                    );
                }
                Some(Err(e)) => {
                    ui.add_space(6.0);
                    ui.colored_label(Color32::LIGHT_RED, format!("Ошибка: {e}"));
                }
                None => {}
            }

            ui.separator();
            egui::CollapsingHeader::new("Отладка").default_open(true).show(ui, |ui| {
                ui.weak(format!("Встроенный пейлоад: {} байт (UF2)", payload::PAYLOAD_UF2.len()));
                egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                    for line in &self.log {
                        ui.label(RichText::new(line).monospace().size(12.0));
                    }
                });
            });
        });
    }
}
