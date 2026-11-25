use crate::{
    FPS,
    audio::{BLOCK, CHANNELS, RATE},
};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
};
use windows::{
    Win32::{Media::MediaFoundation::*, Storage::FileSystem::MoveFileW, System::Com::CoCreateGuid},
    core::{GUID, PCWSTR},
};

struct Output {
    temporary: PathBuf,
    final_path: PathBuf,
    keep_partial: bool,
}

impl Drop for Output {
    fn drop(&mut self) {
        if !self.keep_partial {
            let _ = std::fs::remove_file(&self.temporary);
        }
    }
}

pub(crate) struct MediaFoundation;

impl MediaFoundation {
    pub fn start() -> Result<Self> {
        // SAFETY: o MTA já foi inicializado e os objetos de mídia serão
        // destruídos antes de encerrar a plataforma nesta thread.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .context("O Media Foundation está indisponível")?;
        Ok(Self)
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // SAFETY: corresponde a um MFStartup bem-sucedido nesta thread.
        let _ = unsafe { MFShutdown() };
    }
}

pub(crate) struct Mp4 {
    writer: IMFSinkWriter,
    video_index: u32,
    audio_index: Option<u32>,
    output: Output,
}

impl Mp4 {
    pub fn new(directory: &Path, width: u32, height: u32, audio: bool) -> Result<Self> {
        // SAFETY: o GUID é gerado localmente. Não há parâmetros de entrada.
        let id = unsafe { CoCreateGuid()? };
        let basename = format!("gravacao-{id:?}");
        let temporary = directory.join(format!("{basename}.parcial"));
        let final_path = directory.join(format!("{basename}.mp4"));
        let native = wide(temporary.as_os_str());
        // SAFETY: o caminho está terminado com NUL e permanece vivo durante
        // a chamada. O modo exclusivo evita substituir um arquivo existente.
        let stream = unsafe {
            MFCreateFile(
                MF_ACCESSMODE_WRITE,
                MF_OPENMODE_FAIL_IF_EXIST,
                MF_FILEFLAGS_NONE,
                PCWSTR(native.as_ptr()),
            )
        }
        .with_context(|| format!("Não foi possível criar {}", temporary.display()))?;
        let output = Output {
            temporary,
            final_path,
            keep_partial: false,
        };
        let configured = Self::configure(&stream, width, height, audio);
        drop(stream);
        let (writer, video_index, audio_index) =
            configured.context("Não foi possível configurar o MP4 e seus encoders")?;
        Ok(Self {
            writer,
            video_index,
            audio_index,
            output,
        })
    }

    fn configure(
        stream: &IMFByteStream,
        width: u32,
        height: u32,
        audio: bool,
    ) -> Result<(IMFSinkWriter, u32, Option<u32>)> {
        // SAFETY: os tipos de mídia e o stream pertencem à sessão atual.
        // As interfaces e seus parâmetros ficam vivos durante cada chamada.
        unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 1)?;
            let attributes =
                attributes.context("O Media Foundation não retornou os atributos do MP4.")?;
            attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            let writer = MFCreateSinkWriterFromURL(PCWSTR::null(), stream, &attributes)?;
            let h264 = video_type(width, height, &MFVideoFormat_H264)?;
            let pixels = width * height;
            h264.SetUINT32(&MF_MT_AVG_BITRATE, (pixels * 6).max(1_000_000))?;
            h264.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            let video_index = writer.AddStream(&h264)?;
            let nv12 = video_type(width, height, &MFVideoFormat_NV12)?;
            nv12.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
            nv12.SetUINT32(&MF_MT_SAMPLE_SIZE, pixels * 3 / 2)?;
            nv12.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
            nv12.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            writer.SetInputMediaType(video_index, &nv12, None)?;

            let audio_index = if audio {
                let aac = audio_type(&MFAudioFormat_AAC)?;
                aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000)?;
                aac.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
                let index = writer.AddStream(&aac)?;
                let pcm = audio_type(&MFAudioFormat_PCM)?;
                pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE * BLOCK as u32)?;
                pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, BLOCK as u32)?;
                writer.SetInputMediaType(index, &pcm, None)?;
                Some(index)
            } else {
                None
            };
            writer.BeginWriting()?;
            Ok((writer, video_index, audio_index))
        }
    }

    pub fn video(&mut self, bytes: &[u8], start: i64, end: i64) -> Result<()> {
        self.output.keep_partial = true;
        self.sample(self.video_index, bytes, start, end)
            .with_context(|| {
                format!(
                    "Falha ao gravar vídeo. Arquivo parcial em {}",
                    self.output.temporary.display()
                )
            })
    }

    pub fn audio(&self, bytes: &[u8], start: i64, end: i64) -> Result<()> {
        let index = self
            .audio_index
            .context("A faixa de áudio não está ativa.")?;
        self.sample(index, bytes, start, end).with_context(|| {
            format!(
                "Falha ao gravar áudio. Arquivo parcial em {}",
                self.output.temporary.display()
            )
        })
    }

    fn sample(&self, index: u32, bytes: &[u8], start: i64, end: i64) -> Result<()> {
        let size = u32::try_from(bytes.len())?;
        let mut destination = ptr::null_mut();
        // SAFETY: alocamos o tamanho necessário. Lock dá acesso exclusivo
        // para copiar exatamente esses bytes e Unlock precede o envio.
        unsafe {
            let buffer = MFCreateMemoryBuffer(size)?;
            buffer.Lock(&mut destination, None, None)?;
            ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(size)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(start)?;
            sample.SetSampleDuration(end - start)?;
            self.writer.WriteSample(index, &sample)?;
        }
        Ok(())
    }

    pub fn finish(self) -> Result<PathBuf> {
        let Self {
            writer, mut output, ..
        } = self;
        if !output.keep_partial {
            drop(writer);
            bail!("A gravação terminou antes do primeiro quadro.");
        }
        // SAFETY: BeginWriting foi bem-sucedido e nenhuma outra thread chama o writer.
        let finalized = unsafe { writer.Finalize() };
        drop(writer);
        finalized.with_context(|| {
            format!(
                "Falha ao finalizar o MP4. Arquivo parcial em {}",
                output.temporary.display()
            )
        })?;
        let from = wide(output.temporary.as_os_str());
        let to = wide(output.final_path.as_os_str());
        // SAFETY: ambos os caminhos terminam em NUL e o stream foi fechado.
        // MoveFileW não substitui o destino se o nome já estiver em uso.
        unsafe { MoveFileW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr())) }.with_context(|| {
            format!(
                "O MP4 foi finalizado, mas permanece em {}",
                output.temporary.display()
            )
        })?;
        let final_path = std::mem::take(&mut output.final_path);
        output.keep_partial = true;
        Ok(final_path)
    }
}

fn wide(path: &OsStr) -> Vec<u16> {
    path.encode_wide().chain(Some(0)).collect()
}

fn ratio(numerator: u32, denominator: u32) -> u64 {
    (u64::from(numerator) << 32) | u64::from(denominator)
}

fn video_type(width: u32, height: u32, subtype: &GUID) -> Result<IMFMediaType> {
    // SAFETY: MFStartup permanece ativo. Cada atributo recebe o tipo
    // e o tamanho esperados pelo Media Foundation.
    unsafe {
        let media = MFCreateMediaType()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media.SetUINT64(&MF_MT_FRAME_SIZE, ratio(width, height))?;
        media.SetUINT64(&MF_MT_FRAME_RATE, ratio(FPS as u32, 1))?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, ratio(1, 1))?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        media.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        media.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(media)
    }
}

fn audio_type(subtype: &GUID) -> Result<IMFMediaType> {
    // SAFETY: MFStartup permanece ativo e as dimensões descrevem o PCM
    // negociado no WASAPI ou o AAC correspondente.
    unsafe {
        let media = MFCreateMediaType()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
        media.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)?;
        media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        Ok(media)
    }
}
