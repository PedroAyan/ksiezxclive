#![deny(unsafe_op_in_unsafe_fn)]

mod audio;
mod mp4;
mod sources;
mod video;

use anyhow::{Context, Result, bail, ensure};
use audio::{Audio, AudioTimeline, ProcessFilter};
use sources::{DiscordExclusion, SourceId, SourceTree};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    System::{
        Com::CoTaskMemFree,
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
    },
    UI::Shell::{FOLDERID_Videos, KF_FLAG_CREATE, SHGetKnownFolderPath},
};

pub const TICKS_PER_SECOND: i64 = 10_000_000;
pub const PCM_RATE: u32 = audio::RATE;
pub const PCM_CHANNELS: u32 = audio::CHANNELS;
pub const AUDIO_EXCEPT_DISCORD: &str = "exceto-discord";
const FPS: i64 = 30;
const AUDIO_LAG: i64 = 4;

struct Apartment;

impl Apartment {
    fn start() -> Result<Self> {
        // SAFETY: a guarda corresponde à inicialização do MTA nesta thread.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .context("Não foi possível inicializar o Windows Runtime")?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: RoInitialize foi bem-sucedido nesta mesma thread.
        unsafe { RoUninitialize() };
    }
}

pub struct MonitorChoice {
    pub id: String,
    pub width: i32,
    pub height: i32,
}

pub fn monitors() -> Result<Vec<MonitorChoice>> {
    Ok(video::monitors()?
        .into_iter()
        .map(|monitor| MonitorChoice {
            id: monitor.name,
            width: monitor.width,
            height: monitor.height,
        })
        .collect())
}

pub struct AudioChoice {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

pub fn audio_sources() -> Result<Vec<AudioChoice>> {
    let _apartment = Apartment::start()?;
    let mut eligible = Vec::new();
    for source in sources::discover()
        .context("Não foi possível listar as fontes de áudio")?
        .into_values()
    {
        let id = source.id.to_string();
        let path = source.path.clone();
        if SourceTree::new(source).verify().is_ok() {
            let name = sources::browser_name(&path)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                });
            eligible.push(AudioChoice { id, name, path });
        }
    }
    eligible.sort_by_cached_key(|source| source.name.to_lowercase());
    Ok(eligible)
}

struct Clock(i64);

impl Clock {
    fn new() -> Result<Self> {
        let mut frequency = 0;
        // SAFETY: frequency é uma saída i64 gravável.
        unsafe { QueryPerformanceFrequency(&mut frequency)? };
        ensure!(frequency > 0, "Frequência inválida do relógio de captura.");
        Ok(Self(frequency))
    }

    fn now(&self) -> Result<i64> {
        let mut ticks = 0;
        // SAFETY: ticks é uma saída i64 gravável.
        unsafe { QueryPerformanceCounter(&mut ticks)? };
        Ok((i128::from(ticks) * i128::from(TICKS_PER_SECOND) / i128::from(self.0)) as i64)
    }
}

/// A sessão é criada, usada e destruída exclusivamente na thread de captura.
pub struct Capture {
    video: video::Video,
    audio: Option<Audio>,
    timeline: Option<AudioTimeline>,
    clock: Clock,
    origin: i64,
    image: Vec<u8>,
    _apartment: Apartment,
}

pub enum VideoTarget<'a> {
    Monitor(&'a str),
    Window(&'a str),
}

pub fn windows() -> Result<Vec<video::WindowChoice>> {
    Ok(video::windows()?.into_iter().map(|window| window.choice).collect())
}

impl Capture {
    pub fn open(
        target: VideoTarget<'_>,
        audio_id: Option<&str>,
        quality: u32,
        stop: &AtomicBool,
    ) -> Result<Self> {
        ensure!(
            matches!(quality, 480 | 720 | 1080),
            "Qualidade de vídeo inválida."
        );
        let apartment = Apartment::start()?;
        let video = video::Video::from_target(target, quality)?;
        let audio = audio_id
            .map(open_audio)
            .transpose()
            .context("Atualize a lista de áudio ou tente novamente com Sem áudio")?;
        let clock = Clock::new()?;
        video.start()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let (origin, image) = loop {
            ensure!(!stop.load(Ordering::Acquire), "Captura cancelada.");
            if let Some(first) = video.first_frame()? {
                break first;
            }
            ensure!(
                Instant::now() < deadline,
                "O Windows não entregou o primeiro quadro. Mantenha a fonte aberta e visível."
            );
            thread::sleep(Duration::from_millis(3));
        };
        let timeline = audio.as_ref().map(|_| AudioTimeline::new(origin));
        Ok(Self {
            video,
            audio,
            timeline,
            clock,
            origin,
            image,
            _apartment: apartment,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.video.width, self.video.height)
    }

    pub fn elapsed_ticks(&self) -> Result<i64> {
        Ok(self.clock.now()? - self.origin)
    }

    pub fn timestamp_us(&self, elapsed_ticks: i64) -> i64 {
        (self.origin + elapsed_ticks) / 10
    }

    pub fn image(&self) -> &[u8] {
        &self.image
    }

    pub fn update_video(&mut self, time: i64) -> Result<bool> {
        self.video.update(self.origin + time, &mut self.image)
    }

    pub fn poll_audio(&mut self) -> Option<String> {
        let result = match (&mut self.audio, &mut self.timeline) {
            (Some(audio), Some(timeline)) => audio.poll(timeline),
            _ => return None,
        };
        result.err().map(|error| {
            self.audio = None;
            format!("Áudio interrompido. A sessão seguirá com silêncio. {error:#}")
        })
    }

    pub fn reset_live_clock(&mut self) -> Result<()> {
        self.origin = self.clock.now()?;
        if self.timeline.is_some() {
            self.timeline = Some(AudioTimeline::new(self.origin));
        }
        Ok(())
    }

    pub fn render_pcm(&mut self, bytes: &mut [u8]) {
        let frames = bytes.len() / audio::BLOCK;
        if let Some(timeline) = &mut self.timeline {
            if frames * audio::BLOCK == bytes.len() {
                timeline.render(frames, bytes);
                return;
            }
        }
        bytes.fill(0);
    }
}

fn open_audio(id: &str) -> Result<Audio> {
    if id == AUDIO_EXCEPT_DISCORD {
        return Audio::start(ProcessFilter::ExceptDiscord(DiscordExclusion::select()?));
    }
    let id = SourceId::parse(id)?;
    let source = sources::discover()
        .context("Falha ao verificar fontes de reprodução")?
        .remove(&id.pid)
        .filter(|source| source.id == id)
        .context("A instância selecionada ficou indisponível ou não é uma fonte elegível.")?;
    Audio::start(ProcessFilter::App(SourceTree::new(source)))
}

pub fn default_directory() -> Result<PathBuf> {
    // SAFETY: copiamos a string UTF-16 antes de liberar o buffer alocado por COM.
    unsafe {
        use std::os::windows::ffi::OsStringExt;
        let path = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_CREATE, None)
            .context("Não foi possível abrir a pasta Vídeos. Escolha outra pasta.")?;
        let directory = PathBuf::from(std::ffi::OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.0.cast()));
        Ok(directory)
    }
}

pub enum RecordingEvent {
    Started,
    AudioInterrupted(String),
    Finishing,
}

pub fn record(
    monitor: &str,
    audio_id: Option<&str>,
    directory: Option<PathBuf>,
    seconds: Option<u32>,
    stop: &AtomicBool,
    mut notify: impl FnMut(RecordingEvent),
) -> Result<PathBuf> {
    let _apartment = Apartment::start()?;
    let _platform = mp4::MediaFoundation::start()?;
    let directory = match directory {
        Some(path) => std::path::absolute(path).context("Pasta de saída inválida")?,
        None => default_directory()?,
    };
    let mut capture = Capture::open(VideoTarget::Monitor(monitor), audio_id, 1080, stop)?;
    let (width, height) = capture.dimensions();
    let mut output = mp4::Mp4::new(&directory, width, height, audio_id.is_some())?;
    notify(RecordingEvent::Started);
    let mut frame = 0;
    let mut audio_frame = 0;
    let mut capture_error = None;
    let mut stop_time = seconds.map(|seconds| i64::from(seconds) * TICKS_PER_SECOND);
    let time = |frame: i64| frame * TICKS_PER_SECOND / FPS;
    loop {
        let elapsed = capture.elapsed_ticks()?;
        if stop.load(Ordering::Acquire) && stop_time.is_none_or(|limit| elapsed < limit) {
            stop_time = Some(elapsed);
        }
        if let Some(message) = capture.poll_audio() {
            notify(RecordingEvent::AudioInterrupted(message));
        }
        let start = time(frame);
        let stopping = stop_time.filter(|limit| elapsed >= *limit);
        if stopping.is_some_and(|limit| start >= limit && frame != 0) {
            break;
        }
        if frame == 0 || stopping.is_some() || elapsed >= start + time(1) {
            if frame != 0 {
                if let Err(error) = capture.update_video(start) {
                    capture_error = Some(error);
                    break;
                }
            }
            output.video(capture.image(), start, time(frame + 1))?;
            frame += 1;
            if audio_id.is_some() {
                write_audio(&mut capture, &output, &mut audio_frame, frame - AUDIO_LAG)?;
            }
        } else {
            thread::sleep(Duration::from_millis(3));
        }
    }
    notify(RecordingEvent::Finishing);
    // Pare de ler WASAPI enquanto o MP4 usa as amostras já recebidas.
    capture.audio = None;
    if audio_id.is_some() {
        write_audio(&mut capture, &output, &mut audio_frame, frame)?;
    }
    drop(capture);
    let path = output.finish()?;
    if let Some(error) = capture_error {
        bail!(
            "A captura da tela foi interrompida. Trecho finalizado em {}. {error:#}",
            path.display()
        );
    }
    Ok(path)
}

fn write_audio(
    capture: &mut Capture,
    output: &mp4::Mp4,
    index: &mut i64,
    target: i64,
) -> Result<()> {
    const BYTES: usize = PCM_RATE as usize / FPS as usize * audio::BLOCK;
    let mut pcm = [0; BYTES];
    while *index < target {
        capture.render_pcm(&mut pcm);
        let start = *index * TICKS_PER_SECOND / FPS;
        *index += 1;
        output.audio(&pcm, start, *index * TICKS_PER_SECOND / FPS)?;
    }
    Ok(())
}
