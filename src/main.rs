#![deny(unsafe_op_in_unsafe_fn)]

use anyhow::{Context, Result, bail, ensure};
use ksiezyclive::{self as capture, RecordingEvent};
use std::{
    ffi::OsString,
    path::PathBuf,
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
};
use windows::{
    Win32::{
        System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler},
        UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
    },
    core::BOOL,
};
static STOP: AtomicBool = AtomicBool::new(false);
const WEB_AUDIO_WARNING: &str = "Se usar Discord Web, escolha outro aplicativo. O som de abas na mesma árvore do navegador é capturado junto.";

const HELP: &str = "Gravador local para Windows 10 22H2 x64

  ksiezyclive monitores
  ksiezyclive fontes
  ksiezyclive gravar --monitor DISPOSITIVO [--audio ID|nenhum|exceto-discord] [--segundos N] [--saida PASTA]

Use o dispositivo exibido em monitores e o identificador PID@INSTÂNCIA exibido em fontes.
Sem --audio, o MP4 contém apenas vídeo. A pasta padrão é Vídeos.
Use Ctrl+C para finalizar ou defina --segundos.";

struct StopHandler;

unsafe extern "system" fn stop_handler(event: u32) -> BOOL {
    if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        STOP.store(true, Ordering::Release);
        true.into()
    } else {
        false.into()
    }
}

impl StopHandler {
    fn register() -> Result<Self> {
        // SAFETY: o callback tem ABI de console e só escreve em uma atomic estática.
        unsafe { SetConsoleCtrlHandler(Some(stop_handler), true)? };
        Ok(Self)
    }
}

impl Drop for StopHandler {
    fn drop(&mut self) {
        // SAFETY: removemos o mesmo callback estático registrado na inicialização.
        let _ = unsafe { SetConsoleCtrlHandler(Some(stop_handler), false) };
    }
}

struct Config {
    monitor: String,
    audio: Option<String>,
    seconds: Option<u32>,
    directory: Option<PathBuf>,
}

fn parse(args: impl Iterator<Item = OsString>) -> Result<Config> {
    let mut monitor = None;
    let mut audio = None;
    let mut seconds = None;
    let mut directory = None;
    let mut args = args;
    while let Some(key) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("Informe um valor para {}.", key.to_string_lossy()))?;
        let option_text = || value.to_str().context("A opção exige texto válido.");
        match key.to_str() {
            Some("--monitor") if monitor.is_none() => monitor = Some(option_text()?.to_owned()),
            Some("--audio") if audio.is_none() => {
                let value = option_text()?;
                audio = Some(if value == "nenhum" {
                    None
                } else {
                    Some(value.to_owned())
                });
            }
            Some("--segundos") if seconds.is_none() => {
                let duration: u32 = option_text()?
                    .parse()
                    .context("Informe a duração em segundos inteiros.")?;
                ensure!(duration > 0, "A duração deve ser maior que zero.");
                seconds = Some(duration);
            }
            Some("--saida") if directory.is_none() => {
                ensure!(!value.is_empty(), "Informe a pasta de saída.");
                directory = Some(PathBuf::from(value));
            }
            _ => bail!(
                "Opção desconhecida ou repetida: {}. Consulte --help.",
                key.to_string_lossy()
            ),
        }
    }
    Ok(Config {
        monitor: monitor.context("Informe --monitor com o dispositivo exibido em monitores.")?,
        audio: audio.flatten(),
        seconds,
        directory,
    })
}

fn list_sources() -> Result<()> {
    let choices = capture::audio_sources()?;
    for source in &choices {
        println!(
            "{}  {}  ({})",
            source.id,
            source.name,
            source.path.display()
        );
    }
    if choices.is_empty() {
        println!(
            "Nenhuma fonte elegível. Inicie a reprodução no aplicativo e execute fontes novamente."
        );
    }
    println!("{WEB_AUDIO_WARNING}");
    Ok(())
}

fn record(config: Config) -> Result<()> {
    let _stop = StopHandler::register()?;
    let path = capture::record(
        &config.monitor,
        config.audio.as_deref(),
        config.directory,
        config.seconds,
        &STOP,
        |event| match event {
            RecordingEvent::Started => println!("Gravando. Pressione Ctrl+C para finalizar."),
            RecordingEvent::AudioInterrupted(message) => eprintln!("{message}"),
            RecordingEvent::Finishing => println!("Finalizando o MP4..."),
        },
    )?;
    println!("Gravação salva em {}", path.display());
    Ok(())
}

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next();
    match command.as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("monitores") => {
            ensure!(args.next().is_none(), "monitores não recebe opções.");
            for monitor in capture::monitors()? {
                println!("{}  {} × {}", monitor.id, monitor.width, monitor.height);
            }
        }
        Some("fontes") => {
            ensure!(args.next().is_none(), "fontes não recebe opções.");
            list_sources()?;
        }
        Some("gravar") => {
            let config = parse(args)?;
            record(config)?;
        }
        Some("--help" | "-h" | "ajuda") => println!("{HELP}"),
        _ if command.is_none() => println!("{HELP}"),
        _ => bail!("Comando desconhecido. Consulte --help."),
    }
    Ok(())
}

fn main() -> ExitCode {
    // SAFETY: este processo ainda não criou janelas. A consciência de DPI
    // é usada para exibir as dimensões físicas na enumeração dos monitores.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Erro: {err:#}");
            ExitCode::FAILURE
        }
    }
}
