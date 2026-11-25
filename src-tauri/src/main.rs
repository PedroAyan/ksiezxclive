#![windows_subsystem = "windows"]

mod live;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tauri::{AppHandle, Manager, State, WindowEvent};

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Idle,
    Starting,
    Live,
    Recording,
    Reconnecting,
    Stopping,
    Cleanup,
    Error,
    Saved,
}

#[derive(Clone, Serialize)]
struct Status {
    phase: Phase,
    elapsed: u64,
    invite: Option<String>,
    message: Option<String>,
    warning: Option<String>,
    saved: Option<String>,
}

impl Status {
    fn new(phase: Phase) -> Self {
        Self {
            phase,
            elapsed: 0,
            invite: None,
            message: None,
            warning: None,
            saved: None,
        }
    }
}

struct Session {
    status: Status,
    stop: Option<Arc<AtomicBool>>,
    began: Option<Instant>,
    cleanup: Option<live::Cleanup>,
    closing: bool,
}

impl Session {
    fn snapshot(&self) -> Status {
        let mut status = self.status.clone();
        status.elapsed = self.began.map_or(0, |t| t.elapsed().as_secs());
        status
    }
}

#[derive(Clone)]
struct Controller(Arc<Mutex<Session>>);

impl Controller {
    fn lock(&self) -> MutexGuard<'_, Session> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn phase(&self, phase: Phase) {
        let mut session = self.lock();
        if session.status.phase != Phase::Stopping {
            session.status.phase = phase;
            if session.began.is_none() && matches!(phase, Phase::Live | Phase::Recording) {
                session.began = Some(Instant::now());
            }
        }
    }

    fn invite(&self, invite: String) {
        let mut session = self.lock();
        if session.status.phase == Phase::Starting {
            session.status.invite = Some(invite);
            session.status.phase = Phase::Live;
            session.began = Some(Instant::now());
        }
    }

    fn warning(&self, warning: String) {
        self.lock().status.warning = Some(warning);
    }

    fn finish(
        &self,
        app: &AppHandle,
        result: Result<Option<PathBuf>>,
        cleanup: Option<live::Cleanup>,
    ) {
        let mut session = self.lock();
        let mut status = match result {
            Ok(Some(path)) => {
                let mut status = Status::new(Phase::Saved);
                status.saved = Some(path.to_string_lossy().into_owned());
                status
            }
            Ok(None) => Status::new(Phase::Idle),
            Err(error) => {
                let mut status = Status::new(Phase::Error);
                status.message = Some(format!("{error:#}"));
                status
            }
        };
        if cleanup.is_some() {
            status.phase = Phase::Cleanup;
        }
        session.status = status;
        session.cleanup = cleanup;
        session.stop = None;
        session.began = None;
        let close = session.closing && session.cleanup.is_none();
        session.closing = false;
        drop(session);
        if close {
            app.exit(0);
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Live,
    Record,
}

#[derive(Deserialize)]
struct Request {
    mode: Mode,
    screen: String,
    window: Option<String>,
    audio: Option<String>,
    folder: Option<PathBuf>,
    resolution: u32,
    fps: u32,
    server: Option<live::Config>,
}

#[derive(Serialize)]
struct Screen {
    id: String,
    width: i32,
    height: i32,
}

#[derive(Serialize)]
struct Audio {
    id: String,
    name: String,
    path: String,
}

#[derive(Serialize)]
struct Window {
    id: String,
    title: String,
    minimized: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Choices {
    screens: Vec<Screen>,
    windows: Vec<Window>,
    window_error: Option<String>,
    audio: Vec<Audio>,
    audio_error: Option<String>,
}

#[tauri::command]
async fn choices() -> Result<Choices, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let screens = capture::monitors()?
            .into_iter()
            .map(|item| Screen {
                id: item.id,
                width: item.width,
                height: item.height,
            })
            .collect();
        let (windows, window_error) = match capture::windows() {
            Ok(items) => (items.into_iter().map(|item| Window {
                id: item.id,
                title: item.title,
                minimized: item.minimized,
            }).collect(), None),
            Err(error) => (Vec::new(), Some(format!("Falha ao listar janelas. {error:#}"))),
        };
        let (audio, audio_error) = match capture::audio_sources() {
            Ok(items) => (
                items
                    .into_iter()
                    .map(|item| Audio {
                        id: item.id,
                        name: item.name,
                        path: item.path.to_string_lossy().into_owned(),
                    })
                    .collect(),
                None,
            ),
            Err(error) => (
                Vec::new(),
                Some(format!("As fontes de áudio estão indisponíveis. {error:#}")),
            ),
        };
        Ok::<_, anyhow::Error>(Choices {
            screens,
            windows,
            window_error,
            audio,
            audio_error,
        })
    })
    .await
    .map_err(message)?
    .map_err(message)
}

#[tauri::command]
fn status(controller: State<'_, Controller>) -> Status {
    controller.lock().snapshot()
}

#[tauri::command]
fn start(
    app: AppHandle,
    controller: State<'_, Controller>,
    request: Request,
) -> Result<Status, String> {
    if matches!(request.mode, Mode::Live) && request.window.as_ref().is_some_and(String::is_empty) {
        return Err("Escolha a janela que deseja compartilhar.".into());
    }
    if request.screen.is_empty() && (matches!(request.mode, Mode::Record) || request.window.is_none()) {
        return Err("Escolha a tela que deseja compartilhar.".into());
    }
    if matches!(request.mode, Mode::Live)
        && (!matches!(request.resolution, 480 | 720 | 1080) || !matches!(request.fps, 30 | 60))
    {
        return Err("Escolha uma resolução e uma taxa de quadros disponíveis.".into());
    }
    let controller = controller.inner().clone();
    let stop = Arc::new(AtomicBool::new(false));
    let initial = {
        let mut session = controller.lock();
        if session.stop.is_some() || session.cleanup.is_some() || session.closing {
            return Err("Encerre a sessão atual para abrir outra.".into());
        }
        session.stop = Some(stop.clone());
        session.status = Status::new(Phase::Starting);
        session.snapshot()
    };
    tauri::async_runtime::spawn(async move {
        match request.mode {
            Mode::Live => {
                let (result, cleanup) = live::run(request, &controller, stop).await;
                controller.finish(&app, result.map(|()| None), cleanup);
            }
            Mode::Record => {
                let events = controller.clone();
                let result = tauri::async_runtime::spawn_blocking(move || {
                    capture::record(
                        &request.screen,
                        request.audio.as_deref(),
                        request.folder,
                        None,
                        &stop,
                        |event| match event {
                            capture::RecordingEvent::Started => events.phase(Phase::Recording),
                            capture::RecordingEvent::AudioInterrupted(warning) => {
                                events.warning(warning)
                            }
                            capture::RecordingEvent::Finishing => events.phase(Phase::Stopping),
                        },
                    )
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
                controller.finish(&app, result.map(Some), None);
            }
        }
    });
    Ok(initial)
}

fn request_stop(app: AppHandle, controller: Controller, close: bool) -> Status {
    let mut session = controller.lock();
    session.closing |= close;
    if let Some(stop) = &session.stop {
        stop.store(true, Ordering::Release);
        session.status.phase = Phase::Stopping;
        session.status.invite = None;
    } else if let Some(cleanup) = session.cleanup.take() {
        session.stop = Some(Arc::new(AtomicBool::new(true)));
        session.status = Status::new(Phase::Stopping);
        let background = controller.clone();
        tauri::async_runtime::spawn(async move {
            match cleanup.close().await {
                Ok(()) => background.finish(&app, Ok(None), None),
                Err(error) => background.finish(&app, Err(error), Some(cleanup)),
            }
        });
    } else if close {
        let status = session.snapshot();
        drop(session);
        app.exit(0);
        return status;
    }
    session.snapshot()
}

#[tauri::command]
fn stop(app: AppHandle, controller: State<'_, Controller>) -> Status {
    request_stop(app, controller.inner().clone(), false)
}

#[tauri::command]
fn show_recording(controller: State<'_, Controller>) -> Result<(), String> {
    let open = || -> Result<()> {
        let Some(path) = &controller.lock().status.saved else {
            bail!("Nenhuma gravação finalizada.")
        };
        let path = PathBuf::from(path);
        let directory = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("A pasta da gravação ficou indisponível."))?;
        ensure!(
            directory.is_dir(),
            "A pasta da gravação ficou indisponível."
        );
        let system =
            std::env::var_os("SystemRoot").context("O Windows não informou a pasta do sistema.")?;
        std::process::Command::new(PathBuf::from(system).join("explorer.exe"))
            .arg(directory)
            .spawn()?;
        Ok(())
    };
    open().map_err(message)
}

fn message(error: impl std::fmt::Display) -> String {
    format!("{error:#}")
}

fn main() {
    let controller = Controller(Arc::new(Mutex::new(Session {
        status: Status::new(Phase::Idle),
        stop: None,
        began: None,
        cleanup: None,
        closing: false,
    })));
    tauri::Builder::default()
        .manage(controller)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            choices,
            status,
            start,
            stop,
            show_recording
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let controller = window.state::<Controller>().inner().clone();
                let active = {
                    let session = controller.lock();
                    session.stop.is_some() || session.cleanup.is_some()
                };
                if active {
                    api.prevent_close();
                    request_stop(window.app_handle().clone(), controller, true);
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("Não foi possível abrir o ksiezyclive.");
}
