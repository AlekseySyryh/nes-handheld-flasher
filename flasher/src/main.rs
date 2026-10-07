mod catalog;
mod devices;
mod dump;
mod payload;
mod uf2;
mod write;

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
    eframe::run_native(
        "RP2040 Flasher",
        options,
        Box::new(|_cc| Ok(Box::new(FlasherApp::new()))),
    )
}

struct Run {
    rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Backup,
    Write,
    Games,
    Dump,
    Restore,
    About,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Dump,
    Backup,
    Write,
    Restore,
}

struct FlasherApp {
    screen: Screen,
    library_path: String,
    library: Option<Result<catalog::Library, String>>,
    selected: Option<usize>,
    status: Option<(bool, String)>,
    dirty: bool,
    confirm_flash: bool,
    confirm_restore: bool,
    out_path: String,
    backup_path: PathBuf,
    mode: Mode,
    write_uf2: Option<Arc<Vec<u8>>>,
    backup_kept: bool,
    backup_started: bool,
    notice: Option<String>,
    run: Option<Run>,
    step: Option<Step>,
    progress: (u32, u32),
    log: Vec<String>,
    result: Option<Result<Summary, String>>,
}

impl FlasherApp {
    fn new() -> Self {
        let dir = app_dir();
        let dump_path = dir.join("rp2040_dump.bin").display().to_string();
        let backup_path = dir.join("backup.uf2");
        let mut app = Self {
            screen: if backup_path.is_file() && std::path::Path::new(&dump_path).is_file() {
                Screen::Games
            } else {
                Screen::Backup
            },
            library_path: dump_path.clone(),
            library: None,
            selected: None,
            status: None,
            dirty: false,
            confirm_flash: false,
            confirm_restore: false,
            out_path: dump_path,
            backup_path,
            mode: Mode::Dump,
            write_uf2: None,
            backup_kept: false,
            backup_started: false,
            notice: None,
            run: None,
            step: None,
            progress: (0, 0),
            log: Vec::new(),
            result: None,
        };
        if std::path::Path::new(&app.library_path).exists() {
            app.load_library();
        }
        app
    }

    fn load_library(&mut self) {
        let path = self.library_path.trim();
        let lib = std::fs::read(path)
            .map_err(|e| format!("{path}: {e}"))
            .and_then(|data| catalog::Library::from_dump(data).map_err(|e| format!("{e:#}")));
        self.selected = matches!(&lib, Ok(l) if !l.entries.is_empty()).then_some(0);
        self.library = Some(lib);
        self.status = None;
        self.dirty = false;
    }

    fn start(&mut self, ctx: &egui::Context, backup: bool) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let keep_backup = backup && self.backup_path.is_file();
        let job = dump::Job {
            dump_path: PathBuf::from(self.out_path.trim()),
            uf2_path: (backup && !keep_backup).then(|| self.backup_path.clone()),
            reboot: true,
        };
        dump::spawn(tx, ctx.clone(), cancel.clone(), job);
        self.mode = if backup { Mode::Backup } else { Mode::Dump };
        self.backup_kept = keep_backup;
        self.run = Some(Run { rx, cancel });
        self.step = Some(Step::WaitBootsel);
        self.progress = (0, 0);
        self.log.clear();
        if keep_backup {
            self.log
                .push("backup.uf2 уже существует и не будет перезаписан".into());
        }
        self.result = None;
    }

    /// Builds the UF2 for the edited library and switches to the write screen.
    fn begin_write(&mut self, ctx: &egui::Context) {
        let Some(Ok(lib)) = &self.library else { return };
        if !self.backup_path.is_file() {
            self.status = Some((
                true,
                "Ошибка: нет резервной копии backup.uf2, запись запрещена".into(),
            ));
            return;
        }
        match lib.build() {
            Ok(image) => {
                let (start, end) = lib.write_range();
                self.write_uf2 = Some(Arc::new(uf2::from_flash_range(&image, start, end)));
                self.screen = Screen::Write;
                self.start_write(ctx);
            }
            Err(e) => self.status = Some((true, format!("Ошибка: {e:#}"))),
        }
    }

    fn start_write(&mut self, ctx: &egui::Context) {
        let Some(uf2) = self.write_uf2.clone() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        write::spawn(tx, ctx.clone(), cancel.clone(), uf2);
        self.mode = Mode::Write;
        self.run = Some(Run { rx, cancel });
        self.step = Some(Step::WaitBootsel);
        self.progress = (0, 0);
        self.log.clear();
        self.result = None;
    }

    fn start_restore(&mut self, ctx: &egui::Context) {
        let name = self.backup_path.display().to_string();
        let data = std::fs::read(&self.backup_path)
            .map_err(|e| format!("{name}: {e}"))
            .and_then(|d| {
                uf2::validate(&d)
                    .map(|_| d)
                    .map_err(|e| format!("{name} повреждён: {e}"))
            });
        match data {
            Ok(data) => {
                self.write_uf2 = Some(Arc::new(data));
                self.start_write(ctx);
                self.mode = Mode::Restore;
            }
            Err(e) => {
                self.step = None;
                self.result = Some(Err(e));
            }
        }
    }

    fn poll_events(&mut self) {
        let Some(run) = &self.run else { return };
        let mut restored = false;
        let mut finished = false;
        let mut reload = false;
        let mut written = false;
        while let Ok(ev) = run.rx.try_recv() {
            match ev {
                Event::Step(s) => self.step = Some(s),
                Event::Log(l) => self.log.push(l),
                Event::Progress { done, total } => self.progress = (done, total),
                Event::Finished(r) => {
                    if let Err(e) = &r {
                        self.log.push(format!("ОШИБКА: {e}"));
                    }
                    if let Ok(summary) = &r {
                        if self.mode == Mode::Restore {
                            restored = true;
                        } else if self.mode == Mode::Write {
                            written = true;
                        } else {
                            self.library_path = summary.path.display().to_string();
                            reload = true;
                        }
                    }
                    self.result = Some(r);
                    finished = true;
                }
            }
        }
        if reload {
            self.load_library();
            self.screen = Screen::Games;
            if self.mode == Mode::Dump {
                self.notice = Some(format!("Прошивка прочитана: {}", self.library_path));
            } else {
                let what = if self.backup_kept {
                    "Дамп rp2040_dump.bin обновлён, существующий backup.uf2 сохранён."
                } else {
                    "Резервная копия создана (backup.uf2 и rp2040_dump.bin)."
                };
                self.notice = Some(format!(
                    "{what}
Папка: {}
Консоль перезагружена, кнопку Start можно отпустить.",
                    app_dir().display()
                ));
            }
        }
        if written {
            self.screen = Screen::Games;
            self.dirty = false;
            self.notice = Some(format!(
                "Запись завершена, устройство перезапущено. Теперь можно отпустить Start и проверить консоль.
                 Если что-то не так, восстановите исходную прошивку: переведите консоль в режим BOOTSEL                  и скопируйте на появившийся диск файл {}",
                self.backup_path.display()
            ));
        }
        if restored {
            self.screen = Screen::Games;
            self.notice = Some(
                "Восстановление завершено, устройство перезапущено. Теперь можно отпустить Start."
                    .into(),
            );
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
        if self.screen == Screen::Backup && self.run.is_none() && !self.backup_started {
            self.backup_started = true;
            self.start(&ctx, true);
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RP2040 Flasher");
                if !matches!(self.screen, Screen::Backup | Screen::Write)
                    && !(self.screen == Screen::Restore && self.run.is_some())
                {
                    ui.separator();
                    ui.selectable_value(&mut self.screen, Screen::Games, "Игры");
                    ui.selectable_value(&mut self.screen, Screen::Dump, "Чтение прошивки");
                    ui.selectable_value(&mut self.screen, Screen::Restore, "Восстановление из резервной копии");
                    ui.selectable_value(&mut self.screen, Screen::About, "О программе");
                }
            });
            ui.separator();
            match self.screen {
                Screen::Backup => self.ui_backup(ui),
                Screen::Write => self.ui_write(ui),
                Screen::Games => self.ui_games(ui),
                Screen::Dump => self.ui_dump(ui),
                Screen::Restore => self.ui_restore(ui),
                Screen::About => {
                    ui.heading("О программе");
                    ui.label(format!("RP2040 Flasher, версия {}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(6.0);
                    ui.label("Инструмент предназначен для работы с собственным устройством и данными пользователя. Программа не содержит игр; за использование и распространение выгруженных ROM отвечает пользователь.");
                    ui.add_space(6.0);
                    ui.weak(format!("Сборка: {} ({})", env!("BUILD_DATE"), env!("BUILD_GIT_HASH")));
                    ui.weak(format!(
                        "Встроенный пейлоад: версия {}, {} байт (UF2)",
                        env!("PAYLOAD_VERSION"),
                        payload::PAYLOAD_UF2.len()
                    ));
                }
            }
        });
    }
}

impl FlasherApp {
    fn ui_games(&mut self, ui: &mut egui::Ui) {
        if let Some(notice) = &self.notice {
            let mut close = false;
            ui.group(|ui| {
                ui.colored_label(ok_color(ui), notice.as_str());
                ui.horizontal(|ui| {
                    close = ui.button("OK").clicked();
                    if ui.button("Открыть папку").clicked() {
                        open_folder(&app_dir());
                    }
                });
            });
            if close {
                self.notice = None;
            }
        }
        let mut load = false;
        let mut save = false;
        ui.horizontal(|ui| {
            ui.label("Файл дампа:");
            let width = (ui.available_width() - 270.0).max(120.0);
            ui.add(egui::TextEdit::singleline(&mut self.library_path).desired_width(width));
            if ui.button("Обзор…").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("BIN", &["bin"])
                    .pick_file()
            {
                self.library_path = path.display().to_string();
                load = true;
            }
            load |= ui.button("Загрузить").clicked();
            save = ui
                .add_enabled(
                    matches!(self.library, Some(Ok(_))),
                    egui::Button::new("Сохранить…"),
                )
                .clicked();
        });
        ui.horizontal(|ui| {
            ui.weak(format!("Резервная копия: {}", self.backup_path.display()));
            if ui.small_button("Открыть папку").clicked() {
                open_folder(&app_dir());
            }
        });
        ui.separator();
        if load {
            self.load_library();
        }

        let mut flash = false;
        let Self {
            library,
            selected,
            status,
            dirty,
            library_path,
            ..
        } = self;
        match library {
            None => {
                ui.weak("Укажите файл дампа и нажмите «Загрузить» (или скачайте его на вкладке «Чтение прошивки»).");
            }
            Some(Err(e)) => {
                ui.colored_label(err_color(ui), format!("Ошибка: {e}"));
            }
            Some(Ok(lib)) => {
                match games_editor(ui, lib, selected, dirty) {
                    Some(Action::Flash) => flash = true,
                    Some(action) => {
                        *status = Some(apply_action(action, lib, selected, dirty, library_path))
                    }
                    None => {}
                }
                if let Some((is_err, msg)) = status {
                    ui.colored_label(
                        if *is_err { err_color(ui) } else { ok_color(ui) },
                        msg.as_str(),
                    );
                }
            }
        }
        if save {
            self.save_library();
        }
        if flash {
            if self.dirty {
                self.confirm_flash = true;
            } else {
                self.begin_write(&ui.ctx().clone());
            }
        }
        if self.confirm_flash {
            let mut choice = None;
            egui::Window::new("Прошивка не сохранена")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label("В списке игр есть несохранённые изменения. Сохранить прошивку в файл перед записью на приставку?");
                    ui.horizontal(|ui| {
                        if ui.button("Сохранить и записать").clicked() {
                            choice = Some(0);
                        }
                        if ui.button("Записать без сохранения").clicked() {
                            choice = Some(1);
                        }
                        if ui.button("Отмена").clicked() {
                            choice = Some(2);
                        }
                    });
                });
            if choice.is_some() {
                self.confirm_flash = false;
            }
            if choice == Some(0) {
                self.save_library();
            }
            if matches!(choice, Some(0 | 1)) && (choice == Some(1) || !self.dirty) {
                self.begin_write(&ui.ctx().clone());
            }
        }
    }

    fn save_library(&mut self) {
        if let Some(Ok(lib)) = &mut self.library {
            self.status = Some(apply_action(
                Action::Save,
                lib,
                &mut self.selected,
                &mut self.dirty,
                &self.library_path,
            ));
        }
    }

    fn ui_write(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading("Запись на приставку");
        ui.label("Будут перезаписаны таблица игр и область ROM. Код эмулятора и настройки не затрагиваются.");
        ui.add_space(6.0);
        ui.group(|ui| {
            ui_bootsel_steps(ui);
            ui.add_space(4.0);
            ui.colored_label(
                warn_color(ui),
                "Держите Start до сообщения о завершении записи: если отпустить его раньше, консоль выйдет из режима BOOTSEL посреди записи.",
            );
        });
        ui.add_space(6.0);
        self.ui_progress(ui);
        let (mut retry, mut back) = (false, false);
        if self.run.is_none() && matches!(self.result, Some(Err(_))) {
            ui.horizontal(|ui| {
                retry = ui.button("Повторить").clicked();
                back = ui.button("Назад").clicked();
            });
        } else if let Some(run) = &self.run {
            let waiting = self.step == Some(Step::WaitBootsel);
            if ui
                .add_enabled(waiting, egui::Button::new("Отмена"))
                .clicked()
            {
                run.cancel.store(true, Ordering::Relaxed);
            }
        }
        if retry {
            self.start_write(&ctx);
        }
        if back {
            self.screen = Screen::Games;
        }
    }

    fn ui_restore(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading("Восстановление из резервной копии");
        ui.label(format!(
            "Полный образ {} будет записан на устройство.",
            self.backup_path.display()
        ));
        let has_backup = self.backup_path.is_file();
        if !has_backup {
            ui.colored_label(
                err_color(ui),
                "Файл backup.uf2 не найден рядом с программой.",
            );
        }
        if let Some(notice) = &self.notice {
            ui.colored_label(ok_color(ui), notice.as_str());
        }
        ui.add_space(6.0);
        ui.group(|ui| {
            ui_bootsel_steps(ui);
            ui.add_space(4.0);
            ui.colored_label(
                warn_color(ui),
                "Держите Start до сообщения о завершении записи: если отпустить его раньше, консоль выйдет из режима BOOTSEL посреди записи.",
            );
            ui.weak("Можно нажать «Восстановить» до этого: программа дождётся устройства.");
        });
        ui.add_space(6.0);
        if let Some(run) = &self.run {
            let waiting = self.step == Some(Step::WaitBootsel);
            if ui
                .add_enabled(waiting, egui::Button::new("Отмена"))
                .clicked()
            {
                run.cancel.store(true, Ordering::Relaxed);
            }
        } else if ui
            .add_enabled(has_backup, egui::Button::new("Восстановить"))
            .clicked()
        {
            self.confirm_restore = true;
        }
        if self.confirm_restore {
            let mut choice = None;
            egui::Window::new("Восстановить прошивку?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label("Вся прошивка приставки, включая настройки и сохранения, будет заменена содержимым backup.uf2. Игры, добавленные после резервной копии, будут потеряны.");
                    ui.horizontal(|ui| {
                        if ui.button("Восстановить").clicked() {
                            choice = Some(true);
                        }
                        if ui.button("Отмена").clicked() {
                            choice = Some(false);
                        }
                    });
                });
            if let Some(go) = choice {
                self.confirm_restore = false;
                if go {
                    self.notice = None;
                    self.start_restore(&ctx);
                }
            }
        }
        ui.separator();
        if self.mode == Mode::Restore {
            self.ui_progress(ui);
        }
    }

    fn ui_dump(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let running = self.run.is_some();
        ui.add_enabled_ui(!running, |ui| {
            ui.horizontal(|ui| {
                ui.label("Сохранить прошивку в:");
                let width = (ui.available_width() - 90.0).max(120.0);
                ui.add(egui::TextEdit::singleline(&mut self.out_path).desired_width(width));
                if ui.button("Обзор…").clicked() {
                    let mut dialog = rfd::FileDialog::new().add_filter("BIN", &["bin"]);
                    let current = std::path::Path::new(self.out_path.trim());
                    if let Some(dir) = current.parent().filter(|d| d.is_dir()) {
                        dialog = dialog.set_directory(dir);
                    }
                    dialog = dialog.set_file_name(current.file_name().map_or_else(
                        || "rp2040_dump.bin".into(),
                        |n| n.to_string_lossy().into_owned(),
                    ));
                    if let Some(path) = dialog.save_file() {
                        self.out_path = path.display().to_string();
                    }
                }
            });
        });
        ui.add_space(6.0);
        ui.group(|ui| {
            ui_bootsel_steps(ui);
            ui.weak("Можно нажать «Начать» до этого: программа дождётся устройства.");
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if let Some(run) = &self.run {
                if ui.button("Отмена").clicked() {
                    run.cancel.store(true, Ordering::Relaxed);
                }
            } else if ui.button("Начать").clicked() {
                self.start(&ctx, false);
            }
        });
        ui.separator();
        self.ui_progress(ui);
    }

    /// First-run screen: creates `backup.uf2` + `rp2040_dump.bin` without any button presses.
    fn ui_backup(&mut self, ui: &mut egui::Ui) {
        ui.heading("Резервная копия прошивки");
        ui.label("Перед редактированием нужно сохранить исходную прошивку, чтобы её можно было восстановить.");
        ui.add_space(6.0);
        ui.group(|ui| {
            ui_bootsel_steps(ui);
            ui.add_space(4.0);
            ui.label("Нажимать кнопки в программе не нужно: всё произойдёт автоматически.");
            ui.colored_label(
                warn_color(ui),
                "Не отпускайте Start, пока программа не сообщит, что это можно сделать.",
            );
        });
        ui.add_space(6.0);
        self.ui_progress(ui);
        if self.run.is_none() && matches!(self.result, Some(Err(_))) {
            if ui.button("Повторить").clicked() {
                self.backup_started = false;
            }
        } else if let Some(run) = &self.run
            && ui.button("Отмена").clicked()
        {
            run.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn steps(&self) -> &'static [Step] {
        match self.mode {
            Mode::Dump => &Step::ALL,
            Mode::Backup => &Step::ALL,
            Mode::Write | Mode::Restore => &Step::WRITE,
        }
    }

    /// Step list, progress bar, result line and the debug log of the current/last run.
    fn ui_progress(&self, ui: &mut egui::Ui) {
        if let Some(current) = self.step {
            for &step in self.steps() {
                let failed = matches!(self.result, Some(Err(_))) && step == current;
                let (mark, color) = if current == Step::Finished || step < current {
                    ("[x]", ok_color(ui))
                } else if failed {
                    ("[!]", err_color(ui))
                } else if step == current {
                    ("[>]", warn_color(ui))
                } else {
                    ("[ ]", dim_color(ui))
                };
                ui.label(RichText::new(format!("{mark} {}", step.label())).color(color));
            }
            if self.progress.1 > 0 {
                let (done, total) = self.progress;
                let text = if matches!(self.mode, Mode::Write | Mode::Restore) {
                    format!("{}% ({done}/{total} блоков UF2)", done * 100 / total)
                } else {
                    format!("{done}/{total} блоков")
                };
                ui.add(egui::ProgressBar::new(done as f32 / total as f32).text(text));
            }
        }

        match &self.result {
            Some(Ok(s)) => {
                ui.add_space(6.0);
                ui.colored_label(
                    ok_color(ui),
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
                ui.colored_label(err_color(ui), format!("Ошибка: {e}"));
            }
            None => {}
        }

        ui.separator();
        egui::CollapsingHeader::new("Отладка")
            .default_open(true)
            .show(ui, |ui| {
                ui.weak(format!(
                    "Встроенный пейлоад: {} байт (UF2)",
                    payload::PAYLOAD_UF2.len()
                ));
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(RichText::new(line).monospace().size(12.0));
                        }
                    });
            });
    }
}

fn ui_bootsel_steps(ui: &mut egui::Ui) {
    ui.label(RichText::new("Переведите консоль в режим BOOTSEL:").strong());
    ui.label("1. Выключите консоль (долгое нажатие Start). Если она включена, войти в BOOTSEL не получится.");
    ui.label("2. Подключите консоль к компьютеру по USB.");
    ui.label("3. Нажмите и удерживайте Menu.");
    ui.label("4. Удерживая Menu, нажмите Start.");
    ui.label("5. Отпустите Menu, но продолжайте удерживать Start.");
}

fn ok_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::LIGHT_GREEN
    } else {
        Color32::from_rgb(0, 110, 40)
    }
}

fn err_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::LIGHT_RED
    } else {
        Color32::from_rgb(190, 20, 20)
    }
}

fn warn_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::YELLOW
    } else {
        Color32::from_rgb(160, 90, 0)
    }
}

fn dim_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::GRAY
    } else {
        Color32::from_rgb(110, 110, 110)
    }
}

fn open_folder(dir: &std::path::Path) {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(dir).spawn();
}

fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Clone, Copy)]
enum Action {
    Add,
    Replace,
    Remove,
    Move { up: bool },
    Export,
    Save,
    Flash,
}

/// Draws the game list with the button column; returns the action requested by the user.
fn games_editor(
    ui: &mut egui::Ui,
    lib: &mut catalog::Library,
    selected: &mut Option<usize>,
    dirty: &mut bool,
) -> Option<Action> {
    let (used, cap) = (lib.used(), lib.capacity());
    ui.horizontal(|ui| {
        ui.label(format!(
            "Игр: {} из {}",
            lib.entries.len(),
            lib.max_entries()
        ));
        if *dirty {
            ui.colored_label(warn_color(ui), "(есть несохранённые изменения)");
        }
    });
    ui.add(
        egui::ProgressBar::new(used as f32 / cap as f32).text(format!(
            "Занято {} из {} КиБ (свободно {} КиБ)",
            used / 1024,
            cap / 1024,
            cap.saturating_sub(used) / 1024
        )),
    );
    ui.add_space(6.0);

    let offsets = lib.offsets();
    let list_h = (ui.available_height() - 80.0).max(120.0);
    let mut action = None;
    let mut clicked = None;
    ui.horizontal_top(|ui| {
        let list_w = (ui.available_width() - 190.0).max(300.0);
        ui.allocate_ui(egui::vec2(list_w, list_h), |ui| {
            egui::ScrollArea::vertical()
                .id_salt("games")
                .auto_shrink(false)
                .show(ui, |ui| {
                    egui::Grid::new("games")
                        .striped(true)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            for h in [
                                "Название",
                                "Адрес",
                                "Размер",
                                "PRG",
                                "CHR",
                                "Mapper",
                                "Видео",
                            ] {
                                ui.label(RichText::new(h).strong());
                            }
                            ui.end_row();
                            for (i, entry) in lib.entries.iter().enumerate() {
                                let sel = *selected == Some(i);
                                let (prg, chr, mapper) = match entry.ines() {
                                    Some(n) => (
                                        format!("{} КиБ", n.prg_kib),
                                        if n.chr_kib == 0 {
                                            "RAM".into()
                                        } else {
                                            format!("{} КиБ", n.chr_kib)
                                        },
                                        n.mapper.to_string(),
                                    ),
                                    None => ("не iNES".into(), String::new(), String::new()),
                                };
                                let cells = [
                                    lib.numbered_name(i),
                                    format!("0x{:06X}", offsets[i]),
                                    format!("{} КиБ", entry.rom.len().div_ceil(1024)),
                                    prg,
                                    chr,
                                    mapper,
                                ];
                                for text in cells {
                                    if ui.selectable_label(sel, text).clicked() {
                                        clicked = Some(i);
                                    }
                                }
                                let video = if entry.pal {
                                    RichText::new("PAL!").color(warn_color(ui)).strong()
                                } else {
                                    RichText::new("NTSC")
                                };
                                if ui.selectable_label(sel, video).clicked() {
                                    clicked = Some(i);
                                }
                                ui.end_row();
                            }
                        });
                });
        });
        ui.vertical(|ui| {
            let has_sel = selected.is_some();
            let mut button = |ui: &mut egui::Ui, text: &str, enabled: bool, a: Action| {
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(text).min_size(egui::vec2(170.0, 28.0)),
                    )
                    .clicked()
                {
                    action = Some(a);
                }
            };
            button(ui, "Добавить…", true, Action::Add);
            button(ui, "Изменить…", has_sel, Action::Replace);
            button(ui, "Выгрузить в .nes…", has_sel, Action::Export);
            button(ui, "Удалить", has_sel, Action::Remove);
            ui.add_space(8.0);
            button(
                ui,
                "Вверх",
                selected.is_some_and(|i| i > 0),
                Action::Move { up: true },
            );
            button(
                ui,
                "Вниз",
                selected.is_some_and(|i| i + 1 < lib.entries.len()),
                Action::Move { up: false },
            );
            ui.add_space(8.0);
            button(ui, "Записать на приставку", true, Action::Flash);
        });
    });
    if clicked.is_some() {
        *selected = clicked;
    }

    if let Some(i) = *selected {
        ui.horizontal(|ui| {
            ui.label(format!("Название: {:02}", i + 1));
            let name = &mut lib.entries[i].name;
            if ui.text_edit_singleline(name).changed() {
                *name = catalog::clean_name(name, false);
                *dirty = true;
            }
            ui.weak(format!("{}/{}", name.len(), catalog::MAX_BASE_NAME));
        });
    }
    action
}

fn pick_nes() -> Option<Result<(String, Vec<u8>), String>> {
    let path = rfd::FileDialog::new()
        .add_filter("NES ROM", &["nes"])
        .pick_file()?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(
        std::fs::read(&path)
            .map(|d| (name, d))
            .map_err(|e| format!("{}: {e}", path.display())),
    )
}

/// Executes the action; returns a status line (`true` = error).
fn apply_action(
    action: Action,
    lib: &mut catalog::Library,
    selected: &mut Option<usize>,
    dirty: &mut bool,
    current_file: &str,
) -> (bool, String) {
    let err = |e: &dyn std::fmt::Display| (true, format!("Ошибка: {e}"));
    match action {
        Action::Add | Action::Replace => {
            let (name, data) = match pick_nes() {
                None => return (false, "Отменено".into()),
                Some(Err(e)) => return err(&e),
                Some(Ok(v)) => v,
            };
            let replace = matches!(action, Action::Replace);
            let result = match (replace, *selected) {
                (true, Some(i)) => lib.replace(i, &name, &data).map(|_| i),
                _ => {
                    let at = selected.map_or(lib.entries.len(), |i| i + 1);
                    lib.insert(at, &name, &data).map(|_| at)
                }
            };
            match result {
                Ok(i) => {
                    *selected = Some(i);
                    *dirty = true;
                    let mut msg = format!(
                        "{}: {}",
                        if replace {
                            "Заменено"
                        } else {
                            "Добавлено"
                        },
                        lib.numbered_name(i)
                    );
                    if lib.entries[i].pal {
                        msg.push_str(". Внимание: PAL-версия, эмулятор приставки рассчитан на NTSC, музыка и скорость игры будут быстрее");
                    }
                    (false, msg)
                }
                Err(e) => err(&format!("{e:#}")),
            }
        }
        Action::Remove => {
            let Some(i) = *selected else {
                return (false, String::new());
            };
            let name = lib.numbered_name(i);
            lib.remove(i);
            *selected = (!lib.entries.is_empty()).then(|| i.min(lib.entries.len() - 1));
            *dirty = true;
            (false, format!("Удалено: {name}"))
        }
        Action::Move { up } => {
            if let Some(j) = selected.and_then(|i| lib.move_entry(i, up)) {
                *selected = Some(j);
                *dirty = true;
            }
            (false, "Порядок изменён, номера обновлены".into())
        }
        Action::Flash => (false, String::new()),
        Action::Export => {
            let Some(entry) = selected.and_then(|i| lib.entries.get(i)) else {
                return (false, String::new());
            };
            let file_name: String = entry
                .name
                .chars()
                .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
                .collect();
            let Some(path) = rfd::FileDialog::new()
                .add_filter("NES ROM", &["nes"])
                .set_file_name(format!("{}.nes", file_name.trim()))
                .save_file()
            else {
                return (false, "Отменено".into());
            };
            let data = entry.ines_file();
            match std::fs::write(&path, data) {
                Ok(()) => (
                    false,
                    format!("Выгружено: {} ({} байт)", path.display(), data.len()),
                ),
                Err(e) => err(&e),
            }
        }
        Action::Save => {
            let image = match lib.build() {
                Ok(v) => v,
                Err(e) => return err(&format!("{e:#}")),
            };
            let mut dialog = rfd::FileDialog::new().add_filter("BIN", &["bin"]);
            let current = std::path::Path::new(current_file.trim());
            if let Some(dir) = current.parent().filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            dialog = dialog.set_file_name(current.file_name().map_or_else(
                || "rp2040_dump.bin".into(),
                |n| n.to_string_lossy().into_owned(),
            ));
            let Some(path) = dialog.save_file() else {
                return (false, "Отменено".into());
            };
            match std::fs::write(&path, &image) {
                Ok(()) => {
                    *dirty = false;
                    let crc = flasher_protocol::CRC32.checksum(&image);
                    (
                        false,
                        format!(
                            "Сохранено: {} ({} байт, CRC-32 {crc:08X})",
                            path.display(),
                            image.len()
                        ),
                    )
                }
                Err(e) => err(&e),
            }
        }
    }
}
