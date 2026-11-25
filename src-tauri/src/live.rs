use crate::{Controller, Phase, Request};
use anyhow::{Context, Result, bail, ensure};
use capture::{Capture, PCM_CHANNELS, PCM_RATE, TICKS_PER_SECOND};
use livekit::{
    options::{TrackPublishOptions, VideoEncoding},
    prelude::*,
    webrtc::{
        audio_frame::AudioFrame,
        audio_source::{AudioSourceOptions, RtcAudioSource, native::NativeAudioSource},
        video_frame::{NV12Buffer, VideoFrame, VideoRotation},
        video_source::{RtcVideoSource, VideoResolution, native::NativeVideoSource},
    },
};
use reqwest::{Client, Method, Url};
use serde::Deserialize;
use std::{
    borrow::Cow,
    env,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    time,
};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(15);
const AUDIO_SAMPLES: u32 = PCM_RATE / 100;
const AUDIO_BYTES: usize = (AUDIO_SAMPLES * PCM_CHANNELS * 2) as usize;
const DELAY: i64 = TICKS_PER_SECOND / 20;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Config {
    api_url: String,
    host_token: String,
}

#[derive(Clone)]
struct Api {
    client: Client,
    base: Url,
    host_token: String,
}

#[derive(Deserialize)]
struct RoomTicket {
    id: String,
    url: String,
    token: String,
}

#[derive(Deserialize)]
struct Invitation {
    invite: String,
}

impl Api {
    fn load(override_config: Option<Config>) -> Result<Self> {
        let config = if let Some(config) = override_config {
            config
        } else if let Ok(host_token) = env::var("KSIEZYC_HOST_TOKEN") {
            Config {
                api_url: env::var("KSIEZYC_API_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:3001".into()),
                host_token,
            }
        } else {
            let path = match env::var_os("KSIEZYC_CONFIG") {
                Some(path) => PathBuf::from(path),
                None => env::current_exe()?.with_file_name("ksiezyclive.json"),
            };
            let contents = std::fs::read(path).context(
                "Configure o servidor da live no arquivo ksiezyclive.json ao lado do aplicativo",
            )?;
            serde_json::from_slice(&contents).context("O arquivo ksiezyclive.json está inválido")?
        };
        let host_token = config.host_token.trim().to_owned();
        ensure!(
            host_token.len() >= 32,
            "Configure o código do anfitrião fornecido pelo servidor."
        );
        let base =
            Url::parse(config.api_url.trim()).context("O endereço do servidor da live é inválido")?;
        ensure!(
            base.scheme() == "https" || (base.scheme() == "http" && loopback(&base)),
            "O servidor da live precisa usar HTTPS."
        );
        ensure!(
            base.username().is_empty() && base.password().is_none()
                && base.path() == "/" && base.query().is_none() && base.fragment().is_none(),
            "Informe apenas a origem da API, como https://live.exemplo.com."
        );
        let client = Client::builder()
            .timeout(NETWORK_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            base,
            host_token,
        })
    }

    async fn request(&self, method: Method, path: &str) -> Result<reqwest::Response> {
        let url = self.base.join(path)?;
        let response = self
            .client
            .request(method, url)
            .bearer_auth(&self.host_token)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Não foi possível acessar o servidor da live."))?;
        match response.status().as_u16() {
            200..=299 => Ok(response),
            401 | 403 => bail!("O servidor recusou o código do anfitrião."),
            429 => bail!("O servidor tem muitas lives em andamento."),
            _ => bail!(
                "O servidor da live não concluiu a operação (HTTP {}).",
                response.status().as_u16()
            ),
        }
    }
}

fn loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

pub(crate) struct Cleanup {
    api: Api,
    id: String,
}

impl Cleanup {
    pub async fn close(&self) -> Result<()> {
        self.api
            .request(Method::DELETE, &format!("/api/lives/{}", self.id))
            .await
            .context("O envio parou, mas o servidor ainda não confirmou o encerramento da sala")?;
        Ok(())
    }
}

struct Media {
    video: NativeVideoSource,
    pcm: Option<mpsc::Sender<AudioFrame<'static>>>,
}

pub(crate) async fn run(
    mut request: Request,
    controller: &Controller,
    stop: Arc<AtomicBool>,
) -> (Result<()>, Option<Cleanup>) {
    let api = match Api::load(request.server.take()) {
        Ok(api) => api,
        Err(error) => return (Err(error), None),
    };
    let with_audio = request.audio.is_some();
    let fps = request.fps;
    let (dimensions, prepared) = oneshot::channel();
    let (start_media, media) = oneshot::channel();
    let (first_sent, first_frame) = oneshot::channel();
    let worker_stop = stop.clone();
    let worker_status = controller.clone();
    let mut worker = tokio::task::spawn_blocking(move || -> Result<()> {
        let target = match request.window.as_deref() {
            Some(id) => capture::VideoTarget::Window(id),
            None => capture::VideoTarget::Monitor(&request.screen),
        };
        let capture = match Capture::open(
            target,
            request.audio.as_deref(),
            request.resolution,
            &worker_stop,
        ) {
            Ok(capture) => capture,
            Err(error) => {
                let _ = dimensions.send(Err(error));
                return Ok(());
            }
        };
        if dimensions.send(Ok(capture.dimensions())).is_err() {
            return Ok(());
        }
        let Ok(media) = media.blocking_recv() else {
            return Ok(());
        };
        capture_loop(
            capture,
            media,
            fps,
            &worker_stop,
            &worker_status,
            first_sent,
        )
    });
    let mut room = None;
    let mut cleanup = None;
    let mut audio_task = None;
    let mut worker_finished = false;
    let result = async {
        let (width, height) = prepared.await.context("A captura foi encerrada antes de iniciar")??;
        if stop.load(Ordering::Acquire) { return Ok(()) }
        let ticket: RoomTicket = api.request(Method::POST, "/api/lives").await?
            .json().await.context("O servidor retornou uma sala inválida")?;
        ensure!(!ticket.id.is_empty() && ticket.id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
            "O servidor retornou um identificador de sala inválido.");
        cleanup = Some(Cleanup { api: api.clone(), id: ticket.id });
        if stop.load(Ordering::Acquire) { return Ok(()) }
        let endpoint = Url::parse(&ticket.url).context("O endereço de vídeo está inválido")?;
        ensure!(endpoint.scheme() == "wss" || (endpoint.scheme() == "ws" && loopback(&endpoint)),
            "O servidor de vídeo precisa usar WSS.");
        let mut options = RoomOptions::default();
        options.auto_subscribe = false;
        let (connected, mut events) = time::timeout(NETWORK_TIMEOUT, Room::connect(
            endpoint.as_str(), &ticket.token, options,
        )).await.context("O servidor de vídeo demorou para responder")?
            .context("Não foi possível entrar na sala de vídeo")?;
        let active = room.insert(connected);
        if stop.load(Ordering::Acquire) { return Ok(()) }
        let video = NativeVideoSource::new(VideoResolution { width, height }, true);
        let track = LocalVideoTrack::create_video_track("Tela", RtcVideoSource::Native(video.clone()));
        let max_bitrate = (u64::from(width) * u64::from(height) * u64::from(fps) / 8).max(900_000);
        time::timeout(NETWORK_TIMEOUT, active.local_participant().publish_track(
            LocalTrack::Video(track),
            TrackPublishOptions {
                source: TrackSource::Screenshare, simulcast: false,
                video_encoding: Some(VideoEncoding { max_bitrate, max_framerate: f64::from(fps) }),
                ..Default::default()
            },
        )).await.context("O vídeo demorou para iniciar")?.context("Não foi possível publicar o vídeo")?;
        if stop.load(Ordering::Acquire) { return Ok(()) }
        let pcm = if with_audio {
            let source = NativeAudioSource::new(AudioSourceOptions::default(), PCM_RATE, PCM_CHANNELS, 60);
            let track = LocalAudioTrack::create_audio_track("Aplicativo", RtcAudioSource::Native(source.clone()));
            time::timeout(NETWORK_TIMEOUT, active.local_participant().publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions { source: TrackSource::ScreenshareAudio, dtx: false, ..Default::default() },
            )).await.context("O áudio demorou para iniciar")?.context("Não foi possível publicar o áudio")?;
            let (send, mut receive) = mpsc::channel::<AudioFrame<'static>>(6);
            audio_task = Some(tokio::spawn(async move {
                while let Some(frame) = receive.recv().await {
                    source.capture_frame(&frame).await.context("O envio de áudio foi interrompido")?;
                }
                Ok::<(), anyhow::Error>(())
            }));
            Some(send)
        } else {
            None
        };
        start_media.send(Media { video, pcm }).map_err(|_| anyhow::anyhow!("A captura foi encerrada antes da transmissão."))?;
        time::timeout(NETWORK_TIMEOUT, first_frame).await.context("A live ficou sem quadros de vídeo")?
            .context("O primeiro quadro da live não chegou ao encoder")?;
        if stop.load(Ordering::Acquire) { return Ok(()) }
        let id = &cleanup.as_ref().unwrap().id;
        let link: Invitation = api.request(Method::POST, &format!("/api/lives/{id}/ready")).await?
            .json().await.context("O servidor não retornou um link de convite válido")?;
        controller.invite(link.invite);

        let mut tick = time::interval(Duration::from_millis(120));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if stop.load(Ordering::Acquire) { return Ok(()) }
                    if audio_task.as_ref().is_some_and(|task| task.is_finished()) {
                        if let Some(task) = audio_task.take() { task.await??; }
                    }
                }
                outcome = &mut worker => {
                    worker_finished = true;
                    return outcome.context("A captura do Windows foi interrompida")?;
                }
                event = events.recv() => match event {
                    Some(RoomEvent::Reconnecting) => controller.phase(Phase::Reconnecting),
                    Some(RoomEvent::Reconnected) => controller.phase(Phase::Live),
                    Some(RoomEvent::Disconnected { .. }) | None => bail!("A conexão com o servidor de vídeo foi encerrada."),
                    _ => {}
                }
            }
        }
    }.await;
    // O future acima é dono de start_media. Se a preparação falhar, o worker
    // sai do blocked receive quando o future descartar o remetente.
    let cancelled = stop.swap(true, Ordering::AcqRel);
    controller.phase(Phase::Stopping);
    if !worker_finished {
        let _ = worker.await;
    }
    if let Some(task) = audio_task {
        task.abort();
        let _ = task.await;
    }
    if let Some(room) = room {
        let _ = time::timeout(Duration::from_secs(5), room.close()).await;
    }
    if let Some(close) = cleanup {
        if let Err(error) = close.close().await {
            return (Err(error), Some(close));
        }
    }
    (if cancelled { Ok(()) } else { result }, None)
}

fn capture_loop(
    mut capture: Capture,
    media: Media,
    fps: u32,
    stop: &AtomicBool,
    status: &Controller,
    first: oneshot::Sender<()>,
) -> Result<()> {
    // Esvazia os pacotes da validação para iniciar a live no instante atual.
    if let Some(warning) = capture.poll_audio() {
        status.warning(warning)
    }
    capture.reset_live_clock()?;
    let (width, height) = capture.dimensions();
    let video_step = TICKS_PER_SECOND / i64::from(fps);
    let audio_step = TICKS_PER_SECOND / 100;
    let delay = if media.pcm.is_some() { DELAY } else { 0 };
    let mut next_video = 0;
    let mut next_audio = 0;
    let mut last_video = -TICKS_PER_SECOND;
    let mut first = Some(first);
    let mut image_pending = true;
    while !stop.load(Ordering::Acquire) {
        if let Some(warning) = capture.poll_audio() {
            status.warning(warning)
        }
        let elapsed = capture.elapsed_ticks()? - delay;
        if elapsed >= next_video {
            let target = elapsed - elapsed % video_step;
            image_pending |= capture
                .update_video(target)
                .context("A captura da tela foi interrompida")?;
            next_video = target + video_step;
            // Keep the last image available for late viewers during static scenes.
            if image_pending || target - last_video >= TICKS_PER_SECOND {
                let mut buffer = NV12Buffer::new(width, height);
                let (y, uv) = buffer.data_mut();
                let (input_y, input_uv) = capture
                    .image()
                    .split_at((u64::from(width) * u64::from(height)) as usize);
                y.copy_from_slice(input_y);
                uv.copy_from_slice(input_uv);
                let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
                frame.timestamp_us = capture.timestamp_us(target);
                if media.video.capture_frame(&frame) {
                    image_pending = false;
                    last_video = target;
                    if let Some(first) = first.take() {
                        let _ = first.send(());
                    }
                }
            }
        }
        if let Some(sender) = &media.pcm {
            while elapsed >= next_audio {
                let mut bytes = [0; AUDIO_BYTES];
                capture.render_pcm(&mut bytes);
                next_audio += audio_step;
                if elapsed - next_audio > TICKS_PER_SECOND / 10 {
                    continue;
                }
                let samples = bytes
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                let _ = sender.try_send(AudioFrame {
                    data: Cow::Owned(samples),
                    sample_rate: PCM_RATE,
                    num_channels: PCM_CHANNELS,
                    samples_per_channel: AUDIO_SAMPLES,
                });
            }
        }
        let next = if media.pcm.is_some() { next_audio.min(next_video) } else { next_video };
        let remaining = next - (capture.elapsed_ticks()? - delay);
        if remaining > 0 {
            thread::sleep(Duration::from_nanos(remaining.min(video_step) as u64 * 100));
        }
    }
    Ok(())
}
