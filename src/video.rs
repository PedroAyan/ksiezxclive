use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::c_void,
    slice,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
            GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HMODULE, HWND, LPARAM, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                IDXGIDevice,
            },
            Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW},
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GW_OWNER, GWL_EXSTYLE, GetShellWindow, GetWindow, GetWindowLongPtrW,
            GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_EX_APPWINDOW,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        },
    },
    core::{BOOL, Interface, factory},
};

pub(crate) struct Monitor {
    handle: HMONITOR,
    pub name: String,
    pub width: i32,
    pub height: i32,
}

pub struct WindowChoice {
    pub id: String,
    pub title: String,
    pub minimized: bool,
}

pub(crate) struct Window {
    handle: HWND,
    pub choice: WindowChoice,
}

pub(crate) fn windows() -> Result<Vec<Window>> {
    unsafe extern "system" fn collect(handle: HWND, data: LPARAM) -> BOOL {
        let mut pid = 0;
        let mut title = [0u16; 512];
        // SAFETY: o HWND vem do callback síncrono de EnumWindows. O buffer e
        // o PID são graváveis, e data referencia o Vec exclusivo do chamador.
        unsafe {
            if !IsWindowVisible(handle).as_bool()
                || handle == GetShellWindow()
                || GetWindowThreadProcessId(handle, Some(&mut pid)) == 0
                || pid == std::process::id()
            {
                return true.into();
            }

            let style = GetWindowLongPtrW(handle, GWL_EXSTYLE) as u32;
            if style & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0 != 0
                || (style & WS_EX_APPWINDOW.0 == 0 && GetWindow(handle, GW_OWNER).is_ok())
            {
                return true.into();
            }

            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(
                handle,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            ).is_ok() && cloaked != 0
            {
                return true.into();
            }

            let count = GetWindowTextW(handle, &mut title);
            if count == 0 {
                return true.into();
            }
            let title = String::from_utf16_lossy(&title[..count as usize]);
            if matches!(title.as_str(), "Microsoft Text Input Application" | "NVIDIA GeForce Overlay" | "RzMonitorForegroundWindow") {
                return true.into();
            }
            (&mut *(data.0 as *mut Vec<Window>)).push(Window {
                handle,
                choice: WindowChoice {
                    id: format!("{:p}:{pid}", handle.0),
                    title,
                    minimized: IsIconic(handle).as_bool(),
                },
            });
        }
        true.into()
    }
    let mut windows: Vec<Window> = Vec::new();
    // SAFETY: o callback usa o ponteiro apenas durante a enumeração.
    unsafe { EnumWindows(Some(collect), LPARAM(&mut windows as *mut _ as isize))? };
    windows.sort_by_cached_key(|window| window.choice.title.to_lowercase());
    for group in windows.chunk_by_mut(|a, b| a.choice.title == b.choice.title) {
        if group.len() > 1 {
            for (index, window) in group.iter_mut().enumerate() {
                window.choice.title.push_str(&format!(" · Janela {}", index + 1));
            }
        }
    }
    Ok(windows)
}

pub(crate) fn monitors() -> Result<Vec<Monitor>> {
    unsafe extern "system" fn collect(
        handle: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: o cabeçalho informa o tamanho da estrutura completa. O callback
        // é síncrono e data aponta para o Vec exclusivo durante a enumeração.
        unsafe {
            if !GetMonitorInfoW(handle, &mut info.monitorInfo).as_bool() {
                return false.into();
            }
            let rect = info.monitorInfo.rcMonitor;
            let name_len = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            (&mut *(data.0 as *mut Vec<Monitor>)).push(Monitor {
                handle,
                name: String::from_utf16_lossy(&info.szDevice[..name_len]),
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
            });
        }
        true.into()
    }
    let mut monitors: Vec<Monitor> = Vec::new();
    // SAFETY: EnumDisplayMonitors só usa o ponteiro durante esta chamada.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut monitors as *mut _ as isize),
        )
    };
    ensure!(ok.as_bool(), "Falha ao enumerar os monitores.");
    monitors.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(monitors)
}

pub(crate) struct Video {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    item: GraphicsCaptureItem,
    closed_token: i64,
    closed: Arc<AtomicBool>,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    source_width: u32,
    source_height: u32,
    pub width: u32,
    pub height: u32,
    columns: Vec<usize>,
    rows: Vec<usize>,
    pending: Option<(i64, Direct3D11CaptureFrame)>,
}

impl Video {
    pub fn from_target(target: crate::VideoTarget<'_>, short_side: u32) -> Result<Self> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item = match target {
            crate::VideoTarget::Monitor(id) => {
                let monitor = monitors()?.into_iter()
                    .find(|monitor| monitor.name.eq_ignore_ascii_case(id))
                    .context("A tela selecionada ficou indisponível. Atualize a lista.")?;
                // SAFETY: o HMONITOR foi enumerado acima e é validado pela API.
                unsafe { interop.CreateForMonitor(monitor.handle) }
                    .context("Não foi possível abrir o monitor selecionado")?
            }
            crate::VideoTarget::Window(id) => {
                let window = windows()?.into_iter()
                    .find(|window| window.choice.id == id)
                    .context("A janela selecionada ficou indisponível. Atualize a lista.")?;
                ensure!(
                    !window.choice.minimized,
                    "Restaure a janela selecionada antes de iniciar a live."
                );
                // SAFETY: o HWND e o PID foram revalidados na enumeração acima.
                unsafe { interop.CreateForWindow(window.handle) }
                    .context("Não foi possível capturar a janela selecionada")?
            }
        };
        Self::new(item, short_side)
    }

    pub fn new(item: GraphicsCaptureItem, short_side: u32) -> Result<Self> {
        ensure!(
            GraphicsCaptureSession::IsSupported()?,
            "O Windows Graphics Capture está indisponível."
        );
        let size = item.Size()?;
        ensure!(
            size.Width >= 2 && size.Height >= 2,
            "Dimensões da fonte de vídeo inválidas."
        );
        let source_width = size.Width as u32;
        let source_height = size.Height as u32;
        let (width, height) = output_size(source_width, source_height, short_side);
        let mut device = None;
        let mut context = None;
        // SAFETY: os parâmetros opcionais são nulos e as saídas apontam para
        // armazenamento gravável. O contexto imediato será usado só nesta thread.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .context("Não foi possível criar o dispositivo Direct3D 11")?;
        let device: ID3D11Device = device.context("Direct3D 11 não retornou o dispositivo.")?;
        let context = context.context("Direct3D 11 não retornou o contexto.")?;
        let dxgi = device.cast::<IDXGIDevice>()?;
        // SAFETY: dxgi é uma interface válida do dispositivo D3D11.
        let direct3d: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: source_width,
            Height: source_height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let mut staging = None;
        // SAFETY: desc especifica uma textura de leitura sem dados iniciais.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging))? };
        let staging = staging.context("Direct3D 11 não retornou a textura de leitura.")?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            3,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        session.SetIsCursorCaptureEnabled(true)?;
        let closed = Arc::new(AtomicBool::new(false));
        let notify = closed.clone();
        let closed_token = item.Closed(&TypedEventHandler::new(move |_, _| {
            notify.store(true, Ordering::Release);
            Ok(())
        }))?;
        let columns = (0..width)
            .map(|x| (u64::from(x) * u64::from(source_width) / u64::from(width)) as usize * 4)
            .collect();
        let rows = (0..height)
            .map(|y| (u64::from(y) * u64::from(source_height) / u64::from(height)) as usize)
            .collect();
        Ok(Self {
            session,
            pool,
            item,
            closed_token,
            closed,
            context,
            staging,
            source_width,
            source_height,
            width,
            height,
            columns,
            rows,
            pending: None,
        })
    }

    pub fn start(&self) -> Result<()> {
        self.session
            .StartCapture()
            .context("Não foi possível iniciar a captura da fonte de vídeo")
    }

    fn next_frame(&self) -> Result<Option<(i64, Direct3D11CaptureFrame)>> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "A fonte de vídeo selecionada ficou indisponível."
        );
        // SAFETY: a vtable recebe o ponteiro válido do pool e uma saída inicializada.
        // A ABI retorna S_OK com ponteiro nulo quando a fila está vazia.
        let frame: Option<Direct3D11CaptureFrame> = unsafe {
            let mut raw = std::ptr::null_mut::<c_void>();
            (Interface::vtable(&self.pool).TryGetNextFrame)(
                Interface::as_raw(&self.pool),
                &mut raw,
            )
            .ok()?;
            (!raw.is_null()).then(|| Interface::from_raw(raw))
        };
        let Some(frame) = frame else { return Ok(None) };
        let size = frame.ContentSize()?;
        if size.Width != self.source_width as i32 || size.Height != self.source_height as i32 {
            bail!("O tamanho da fonte de vídeo mudou. Inicie uma nova sessão.");
        }
        Ok(Some((frame.SystemRelativeTime()?.Duration, frame)))
    }

    pub fn first_frame(&self) -> Result<Option<(i64, Vec<u8>)>> {
        let Some((time, frame)) = self.next_frame()? else {
            return Ok(None);
        };
        Ok(Some((time, self.read(&frame)?)))
    }

    pub fn update(&mut self, time: i64, image: &mut Vec<u8>) -> Result<bool> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "O monitor selecionado ficou indisponível."
        );
        let mut latest = None;
        for _ in 0..3 {
            let next = match self.pending.take() {
                Some(frame) => Some(frame),
                None => self.next_frame()?,
            };
            let Some((timestamp, frame)) = next else {
                break;
            };
            if timestamp > time {
                self.pending = Some((timestamp, frame));
                break;
            }
            latest = Some(frame);
        }
        if let Some(frame) = latest {
            self.read_into(&frame, image)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn read(&self, frame: &Direct3D11CaptureFrame) -> Result<Vec<u8>> {
        let mut image = Vec::new();
        self.read_into(frame, &mut image)?;
        Ok(image)
    }

    fn read_into(&self, frame: &Direct3D11CaptureFrame, image: &mut Vec<u8>) -> Result<()> {
        let access = frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: a superfície fica viva durante a cópia. A caixa está contida
        // nas duas texturas e o contexto imediato só é usado nesta thread.
        unsafe {
            let source: ID3D11Texture2D = access.GetInterface()?;
            source.GetDesc(&mut desc);
            ensure!(
                desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM && desc.SampleDesc.Count == 1,
                "O Windows entregou a textura num formato inesperado."
            );
            ensure!(
                desc.Width >= self.source_width && desc.Height >= self.source_height,
                "A textura entregue pelo Windows é menor que a área do monitor."
            );
            let region = D3D11_BOX {
                right: self.source_width,
                bottom: self.source_height,
                back: 1,
                ..Default::default()
            };
            self.context.CopySubresourceRegion(
                &self.staging,
                0,
                0,
                0,
                0,
                &source,
                0,
                Some(&region),
            );
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let pitch = mapped.RowPitch as usize;
        let row_bytes = self.source_width as usize * 4;
        let valid = !mapped.pData.is_null() && pitch >= row_bytes;
        // SAFETY: Map foi bem-sucedido. Dentro da conversão, lemos somente
        // os pixels de cada linha copiada e evitamos o padding do driver.
        unsafe {
            if valid {
                let row =
                    |y| slice::from_raw_parts(mapped.pData.cast::<u8>().add(y * pitch), row_bytes);
                bgra_to_nv12(row, &self.columns, &self.rows, image);
            }
            self.context.Unmap(&self.staging, 0);
        }
        ensure!(valid, "O Direct3D 11 retornou um mapa de textura inválido.");
        Ok(())
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.pending.take();
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn output_size(width: u32, height: u32, short_side: u32) -> (u32, u32) {
    let long_side = short_side * 16 / 9;
    let (max_width, max_height) = if width >= height {
        (long_side, short_side)
    } else {
        (short_side, long_side)
    };
    let scale = (f64::from(max_width) / f64::from(width))
        .min(f64::from(max_height) / f64::from(height))
        .min(1.0);
    let even = |n| ((f64::from(n) * scale).round() as u32 & !1).max(2);
    (even(width), even(height))
}

fn bgra_to_nv12<'a>(
    row: impl Fn(usize) -> &'a [u8],
    columns: &[usize],
    rows: &[usize],
    nv12: &mut Vec<u8>,
) {
    let width = columns.len();
    let area = width * rows.len();
    nv12.resize(area + area / 2, 0);
    let (luma, chroma) = nv12.split_at_mut(area);
    for (y, pair) in rows.chunks_exact(2).enumerate() {
        let top = row(pair[0]);
        let bottom = row(pair[1]);
        for (x, col) in columns.chunks_exact(2).enumerate() {
            let x = x * 2;
            let mut rgb = [0_i32; 3];
            for (dy, row) in [top, bottom].into_iter().enumerate() {
                for (dx, &offset) in col.iter().enumerate() {
                    let [b, g, r] = [row[offset], row[offset + 1], row[offset + 2]].map(i32::from);
                    luma[(2 * y + dy) * width + x + dx] =
                        (16 + ((47 * r + 156 * g + 16 * b + 128) >> 8)) as u8;
                    rgb[0] += r;
                    rgb[1] += g;
                    rgb[2] += b;
                }
            }
            let [r, g, b] = rgb;
            chroma[y * width + x] = (128 + ((-26 * r - 86 * g + 112 * b + 512) >> 10)) as u8;
            chroma[y * width + x + 1] = (128 + ((112 * r - 102 * g - 10 * b + 512) >> 10)) as u8;
        }
    }
}
