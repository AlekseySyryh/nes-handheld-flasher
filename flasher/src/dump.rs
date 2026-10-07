//! The dump workflow: BOOTSEL -> upload payload -> wait for payload -> download flash -> reboot.
//! Runs on a worker thread and reports to the UI through `Event`s.

use std::{
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use eframe::egui;
use flasher_protocol::{CRC32, Command, Kind, ParseError, Request, VERSION, parse_response};
use serialport::{ClearBuffer, SerialPort};

use crate::{devices, payload::PAYLOAD_UF2, uf2};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const PAYLOAD_START_TIMEOUT: Duration = Duration::from_secs(20);
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_BLOCK_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    WaitBootsel,
    UploadPayload,
    WaitPayload,
    Download,
    Reboot,
    Flash,
    WaitRestart,
    Finished,
}

impl Step {
    pub const ALL: [Step; 5] = [
        Step::WaitBootsel,
        Step::UploadPayload,
        Step::WaitPayload,
        Step::Download,
        Step::Reboot,
    ];

    pub const WRITE: [Step; 3] = [Step::WaitBootsel, Step::Flash, Step::WaitRestart];

    pub fn label(self) -> &'static str {
        match self {
            Step::WaitBootsel => "Ожидание устройства в режиме BOOTSEL",
            Step::UploadPayload => "Загрузка пейлоада",
            Step::WaitPayload => "Ожидание запуска пейлоада",
            Step::Download => "Чтение прошивки",
            Step::Reboot => "Перезагрузка устройства",
            Step::Flash => "Запись прошивки на устройство",
            Step::WaitRestart => "Прошивка и перезапуск устройства",
            Step::Finished => "Готово",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub path: PathBuf,
    pub size: usize,
    pub crc32: u32,
    pub retries: u32,
    pub elapsed: Duration,
}

pub enum Event {
    Step(Step),
    Log(String),
    Progress { done: u32, total: u32 },
    Finished(Result<Summary, String>),
}

pub(crate) struct Reporter {
    tx: Sender<Event>,
    ctx: egui::Context,
    start: Instant,
    cancel: Arc<AtomicBool>,
}

impl Reporter {
    pub(crate) fn new(tx: Sender<Event>, ctx: egui::Context, cancel: Arc<AtomicBool>) -> Self {
        Self {
            tx,
            ctx,
            start: Instant::now(),
            cancel,
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    pub(crate) fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }

    pub(crate) fn log(&self, msg: impl AsRef<str>) {
        self.send(Event::Log(format!(
            "[{:6.2}s] {}",
            self.start.elapsed().as_secs_f32(),
            msg.as_ref()
        )));
    }

    pub(crate) fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            bail!("отменено пользователем");
        }
        Ok(())
    }

    /// Polls `f` until it yields a value, the timeout expires or the user cancels.
    pub(crate) fn wait_for<T>(
        &self,
        timeout: Option<Duration>,
        what: &str,
        mut f: impl FnMut() -> Option<T>,
    ) -> Result<T> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            self.check_cancel()?;
            if let Some(v) = f() {
                return Ok(v);
            }
            if deadline.is_some_and(|d| Instant::now() > d) {
                bail!("таймаут ожидания: {what}");
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// What to do with the flash contents once they are read.
pub struct Job {
    pub dump_path: PathBuf,
    /// If set, also write the dump as a restorable UF2 file (written last, atomically).
    pub uf2_path: Option<PathBuf>,
    /// Send `Reboot` at the end. Must be off while the user still holds the BOOTSEL buttons.
    pub reboot: bool,
}

pub fn spawn(tx: Sender<Event>, ctx: egui::Context, cancel: Arc<AtomicBool>, job: Job) {
    thread::spawn(move || {
        let rep = Reporter::new(tx, ctx, cancel);
        let result = run(&rep, &job).map_err(|e| format!("{e:#}"));
        rep.send(Event::Finished(result));
    });
}

fn run(rep: &Reporter, job: &Job) -> Result<Summary> {
    let out_path = job.dump_path.as_path();
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

    rep.step(Step::UploadPayload);
    upload_payload(rep, &drive)?;

    rep.step(Step::WaitPayload);
    let port_name = rep.wait_for(
        Some(PAYLOAD_START_TIMEOUT),
        "порт пейлоада",
        devices::find_payload_port,
    )?;
    rep.log(format!("Найден порт пейлоада: {port_name}"));
    let mut port = open_port(rep, &port_name)?;

    rep.step(Step::Download);
    let (dump, retries) = download(rep, port.as_mut())?;
    let crc32 = CRC32.checksum(&dump);
    fs::write(out_path, &dump).with_context(|| format!("запись {}", out_path.display()))?;
    rep.log(format!(
        "Сохранено {} байт в {}, CRC-32 {crc32:08X}",
        dump.len(),
        out_path.display()
    ));

    if let Some(uf2_path) = &job.uf2_path {
        let image = uf2::from_flash_image(&dump);
        let tmp = uf2_path.with_extension("uf2.tmp");
        fs::write(&tmp, &image).with_context(|| format!("запись {}", tmp.display()))?;
        fs::rename(&tmp, uf2_path)
            .with_context(|| format!("переименование в {}", uf2_path.display()))?;
        rep.log(format!(
            "Резервная копия UF2: {} ({} байт)",
            uf2_path.display(),
            image.len()
        ));
    }

    if job.reboot {
        rep.step(Step::Reboot);
        match transact(
            port.as_mut(),
            Request {
                command: Command::Reboot,
                arg: 0,
            },
        )
        .and_then(|r| expect(r, Kind::Ok))
        {
            Ok(_) => rep.log("Устройство подтвердило перезагрузку"),
            Err(e) => rep.log(format!(
                "Предупреждение: перезагрузка не подтверждена: {e:#}"
            )),
        }
    } else {
        rep.log(
            "Перезагрузка не выполняется: пейлоад остаётся в RAM до переподключения устройства",
        );
    }

    rep.step(Step::Finished);
    Ok(Summary {
        path: out_path.to_owned(),
        size: dump.len(),
        crc32,
        retries,
        elapsed: rep.start.elapsed(),
    })
}

impl Reporter {
    pub(crate) fn step(&self, step: Step) {
        self.log(format!("== {}", step.label()));
        self.send(Event::Step(step));
    }
}

fn upload_payload(rep: &Reporter, drive: &devices::BootselDrive) -> Result<()> {
    let target = drive.mount_point.join("payload.uf2");
    rep.log(format!(
        "Копирую {} байт в {}",
        PAYLOAD_UF2.len(),
        target.display()
    ));
    if let Err(e) = fs::write(&target, PAYLOAD_UF2) {
        // The bootloader may reboot (and the drive vanish) before the write is reported as done.
        thread::sleep(Duration::from_millis(500));
        if devices::find_bootsel_drives()
            .iter()
            .any(|d| d.mount_point == drive.mount_point)
        {
            return Err(e).context("запись UF2 на диск BOOTSEL");
        }
        rep.log(format!(
            "Запись завершилась ошибкой ({e}), но диск исчез - считаю, что пейлоад принят"
        ));
    }
    Ok(())
}

fn open_port(rep: &Reporter, name: &str) -> Result<Box<dyn SerialPort>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match serialport::new(name, 115_200)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(mut p) => {
                let _ = p.write_data_terminal_ready(true);
                return Ok(p);
            }
            Err(e) if Instant::now() < deadline => {
                rep.log(format!("Порт {name} пока не открывается: {e}"));
                rep.check_cancel()?;
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(e).with_context(|| format!("открытие порта {name}")),
        }
    }
}

fn download(rep: &Reporter, port: &mut dyn SerialPort) -> Result<(Vec<u8>, u32)> {
    let info = (0..10)
        .find_map(|attempt| {
            let r = transact(
                port,
                Request {
                    command: Command::Hello,
                    arg: 0,
                },
            )
            .and_then(|r| expect(r, Kind::Info));
            if let Err(e) = &r {
                rep.log(format!("Hello, попытка {}: {e:#}", attempt + 1));
                thread::sleep(POLL_INTERVAL);
            }
            r.ok()
        })
        .context("пейлоад не отвечает на Hello")?;
    if info.len() != 9 {
        bail!("некорректный ответ Info ({} байт)", info.len());
    }
    let version = info[0];
    let flash_size = u32::from_le_bytes(info[1..5].try_into().unwrap());
    let block_size = u32::from_le_bytes(info[5..9].try_into().unwrap());
    rep.log(format!(
        "Info: протокол v{version}, flash {flash_size} байт, блок {block_size} байт"
    ));
    if version != VERSION {
        bail!("версия протокола {version} не поддерживается (ожидается {VERSION})");
    }
    if block_size == 0 || flash_size % block_size != 0 {
        bail!("некорректные размеры: flash {flash_size}, блок {block_size}");
    }

    let total = flash_size / block_size;
    let mut dump = Vec::with_capacity(flash_size as usize);
    let mut retries = 0;
    let started = Instant::now();
    for index in 0..total {
        let mut attempt = 0;
        loop {
            rep.check_cancel()?;
            match get_block(port, index, block_size as usize) {
                Ok(data) => {
                    dump.extend_from_slice(&data);
                    break;
                }
                Err(e) => {
                    attempt += 1;
                    retries += 1;
                    rep.log(format!(
                        "Блок {index}: попытка {attempt}/{MAX_BLOCK_ATTEMPTS} не удалась: {e:#}"
                    ));
                    if attempt >= MAX_BLOCK_ATTEMPTS {
                        bail!("блок {index} не удалось прочитать");
                    }
                }
            }
        }
        if index % 8 == 7 || index + 1 == total {
            rep.send(Event::Progress {
                done: index + 1,
                total,
            });
        }
    }
    let secs = started.elapsed().as_secs_f32();
    rep.log(format!(
        "Прочитано {total} блоков за {secs:.2}s ({:.0} КиБ/с), повторов: {retries}",
        dump.len() as f32 / 1024.0 / secs.max(0.001)
    ));
    Ok((dump, retries))
}

fn get_block(port: &mut dyn SerialPort, index: u32, block_size: usize) -> Result<Vec<u8>> {
    let payload = expect(
        transact(
            port,
            Request {
                command: Command::GetBlock,
                arg: index,
            },
        )?,
        Kind::Block,
    )?;
    if payload.len() != 4 + block_size {
        bail!("неверная длина блока: {}", payload.len());
    }
    let got = u32::from_le_bytes(payload[..4].try_into().unwrap());
    if got != index {
        bail!("получен блок {got} вместо {index}");
    }
    Ok(payload[4..].to_vec())
}

fn expect((kind, payload): (Kind, Vec<u8>), want: Kind) -> Result<Vec<u8>> {
    match kind {
        k if k == want => Ok(payload),
        Kind::Error => bail!("устройство вернуло ошибку, код {:?}", payload.first()),
        k => bail!("неожиданный тип ответа {k:?}"),
    }
}

/// Sends one request and reads exactly one valid response frame.
fn transact(port: &mut dyn SerialPort, req: Request) -> Result<(Kind, Vec<u8>)> {
    port.clear(ClearBuffer::Input)?;
    port.write_all(&req.encode())?;
    port.flush()?;

    let mut buf = Vec::with_capacity(4200);
    let mut tmp = [0u8; 1024];
    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    loop {
        match parse_response(&buf) {
            Ok((kind, payload, _)) => return Ok((kind, payload.to_vec())),
            Err(ParseError::Incomplete) => {}
            Err(e) => bail!("поврежденный ответ: {e:?}"),
        }
        if Instant::now() > deadline {
            bail!("таймаут ответа ({} байт получено)", buf.len());
        }
        match port.read(&mut tmp) {
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
    }
}
