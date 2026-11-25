use crate::TICKS_PER_SECOND;
use crate::sources::{DiscordExclusion, Handle, SourceTree};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    mem::ManuallyDrop,
    ptr, slice,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{S_OK, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Media::Audio::*,
        System::{
            Com::{BLOB, IAgileObject, IAgileObject_Impl, StructuredStorage::*},
            Threading::{CreateEventW, WaitForSingleObject},
            Variant::VT_BLOB,
        },
    },
    core::{HRESULT, Interface, Ref, implement},
};

pub(crate) const RATE: u32 = 44_100;
pub(crate) const CHANNELS: u32 = 2;
pub(crate) const BLOCK: usize = CHANNELS as usize * 2;
const MAX_PACKETS: usize = 128;

struct Activation {
    variant: PROPVARIANT,
    params: AUDIOCLIENT_ACTIVATION_PARAMS,
}

// SAFETY: o PROPVARIANT é somente leitura e seu único ponteiro aponta para
// params na mesma alocação Arc. O callback mantém a alocação viva mesmo
// se o prazo de espera expirar. O buffer nunca é limpo por PropVariantClear.
unsafe impl Send for Activation {}
// SAFETY: não há escrita na variante nem no blob após a construção.
unsafe impl Sync for Activation {}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Completion {
    signal: mpsc::SyncSender<()>,
    _activation: Arc<Activation>,
}

impl IAgileObject_Impl for Completion_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.signal.try_send(());
        Ok(())
    }
}

pub(crate) enum ProcessFilter {
    App(SourceTree),
    ExceptDiscord(DiscordExclusion),
}

impl ProcessFilter {
    fn target(&self) -> (u32, PROCESS_LOOPBACK_MODE) {
        match self {
            Self::App(tree) => (
                tree.pid(),
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            ),
            Self::ExceptDiscord(discord) => (
                discord.pid(),
                PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            ),
        }
    }

    fn verify(&mut self) -> Result<()> {
        match self {
            Self::App(tree) => tree.verify(),
            Self::ExceptDiscord(discord) => discord.verify(),
        }
    }
}

fn activate(filter: &mut ProcessFilter) -> Result<IAudioClient> {
    let (pid, mode) = filter.target();
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    };
    let mut activation = Arc::new(Activation {
        variant: PROPVARIANT::default(),
        params,
    });
    let inner = Arc::get_mut(&mut activation).unwrap();
    inner.variant = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: ptr::addr_of_mut!(inner.params).cast(),
                    },
                },
                ..Default::default()
            }),
        },
    };
    let (signal, completed) = mpsc::sync_channel(1);
    let callback: IActivateAudioInterfaceCompletionHandler = Completion {
        signal,
        _activation: activation.clone(),
    }
    .into();
    filter.verify()?;
    // SAFETY: COM está no MTA. O callback é ágil, e ele e o chamador
    // mantêm os parâmetros vivos até a conclusão assíncrona.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&activation.variant),
            &callback,
        )
    }
    .context("O Windows recusou o início da ativação do loopback por processo")?;
    completed
        .recv_timeout(Duration::from_secs(10))
        .context("A ativação do loopback por processo não foi concluída a tempo")?;
    let mut status: HRESULT = S_OK;
    let mut interface = None;
    // SAFETY: o callback sinalizou a conclusão e as saídas são graváveis.
    unsafe { operation.GetActivateResult(&mut status, &mut interface) }
        .context("Não foi possível obter o resultado da ativação do loopback por processo")?;
    status
        .ok()
        .context("O Windows recusou a ativação do loopback por processo")?;
    Ok(interface
        .context("O Windows não retornou o cliente de áudio.")?
        .cast()?)
}

pub(crate) struct Audio {
    capture: IAudioCaptureClient,
    client: IAudioClient,
    ready: Handle,
    filter: ProcessFilter,
    checked: Instant,
}

struct Packet {
    qpc: i64,
    pcm: Vec<u8>,
}

impl Audio {
    pub fn start(mut filter: ProcessFilter) -> Result<Self> {
        let client = activate(&mut filter)?;
        let format = WAVEFORMATEX {
            wFormatTag: 1, // WAVE_FORMAT_PCM
            nChannels: CHANNELS as u16,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * BLOCK as u32,
            nBlockAlign: BLOCK as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // SAFETY: a estrutura PCM permanece válida durante Initialize. O
        // cliente foi ativado com o filtro de processos escolhido.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                0,
                0,
                &format,
                None,
            )
        }
        .context("O Windows não inicializou o loopback por processo em PCM estéreo")?;
        // SAFETY: o evento é de reset automático, sem nome e com
        // segurança padrão. Mantemos o handle aberto até parar o cliente.
        let ready = Handle(unsafe { CreateEventW(None, false, false, None)? });
        // SAFETY: o cliente foi inicializado e o evento continuará válido.
        let capture: IAudioCaptureClient = unsafe {
            client.SetEventHandle(ready.0)?;
            client.GetService()?
        };
        filter.verify()?;
        // SAFETY: captura, evento e identidade do processo foram verificados.
        unsafe { client.Start() }.context("O Windows não iniciou o loopback por processo")?;
        Ok(Self {
            capture,
            client,
            ready,
            filter,
            checked: Instant::now(),
        })
    }

    pub fn poll(&mut self, timeline: &mut AudioTimeline) -> Result<()> {
        // SAFETY: o evento permanece aberto e esta chamada não bloqueia.
        let packets = match unsafe { WaitForSingleObject(self.ready.0, 0) } {
            WAIT_OBJECT_0 => self.drain()?,
            WAIT_TIMEOUT => Vec::new(),
            _ => bail!("Falha ao aguardar amostras do loopback por processo."),
        };
        if !packets.is_empty() || self.checked.elapsed() >= Duration::from_millis(100) {
            // Verifica depois da leitura e antes de liberar o lote para o encoder.
            // Em falha, todo o lote ainda em memória é descartado.
            if let Err(error) = self.filter.verify() {
                if matches!(self.filter, ProcessFilter::ExceptDiscord(_)) {
                    timeline.clear_pending();
                }
                return Err(error);
            }
            self.checked = Instant::now();
            for packet in packets {
                timeline.push(packet);
            }
        }
        Ok(())
    }

    fn drain(&self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        for _ in 0..MAX_PACKETS {
            let mut data = ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            let mut qpc = 0;
            // SAFETY: o cliente de captura está ativo nesta thread. As saídas
            // são graváveis. Toda leitura bem-sucedida é seguida de ReleaseBuffer.
            unsafe {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(packets);
                }
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let invalid_size = frames > RATE;
                let invalid_data = frames != 0
                    && data.is_null()
                    && flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0;
                if !invalid_size
                    && frames != 0
                    && !invalid_data
                    && flags
                        & (AUDCLNT_BUFFERFLAGS_SILENT.0 | AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0)
                            as u32
                        == 0
                {
                    // WASAPI possui o ponteiro até ReleaseBuffer; copiamos o
                    // pacote PCM de acordo com o block align negociado.
                    let bytes = slice::from_raw_parts(data, frames as usize * BLOCK);
                    packets.push(Packet {
                        qpc: qpc as i64,
                        pcm: bytes.to_vec(),
                    });
                }
                self.capture.ReleaseBuffer(frames)?;
                ensure!(
                    !invalid_size && !invalid_data,
                    "O WASAPI retornou um pacote de áudio inválido."
                );
            }
        }
        bail!("A fila do loopback por processo não pôde ser esvaziada.")
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // SAFETY: Start foi bem-sucedido. O evento continua vivo nesta chamada.
        let _ = unsafe { self.client.Stop() };
    }
}

pub(crate) struct AudioTimeline {
    origin: i64,
    pub written: i64,
    packets: VecDeque<(i64, Vec<u8>)>,
}

impl AudioTimeline {
    fn clear_pending(&mut self) {
        self.packets.clear();
    }

    pub fn new(origin: i64) -> Self {
        Self {
            origin,
            written: 0,
            packets: VecDeque::new(),
        }
    }

    fn push(&mut self, packet: Packet) {
        let scaled = (i128::from(packet.qpc) - i128::from(self.origin)) * i128::from(RATE);
        let nearest = scaled + i128::from(TICKS_PER_SECOND / 2) * scaled.signum();
        let start = (nearest / i128::from(TICKS_PER_SECOND)) as i64;
        if self.packets.len() == MAX_PACKETS {
            self.packets.pop_front();
        }
        self.packets.push_back((start, packet.pcm));
    }

    // Cada trecho é inicialmente silêncio. Sobrepõe apenas amostras que
    // pertencem ao intervalo; descarta sobreposição e limita atraso acumulado.
    pub fn render(&mut self, frames: usize, output: &mut [u8]) {
        output.fill(0);
        let end = self.written + frames as i64;
        let mut cursor = self.written;
        while let Some((start, pcm)) = self.packets.front() {
            let start = *start;
            let packet_end = start + (pcm.len() / BLOCK) as i64;
            if start >= end {
                break;
            }
            let from = cursor.max(start);
            let to = end.min(packet_end);
            if from < to {
                let source = (from - start) as usize * BLOCK;
                let dest = (from - self.written) as usize * BLOCK;
                let bytes = (to - from) as usize * BLOCK;
                output[dest..dest + bytes].copy_from_slice(&pcm[source..source + bytes]);
                cursor = to;
            }
            if packet_end > end {
                break;
            }
            self.packets.pop_front();
        }
        self.written = end;
    }
}
