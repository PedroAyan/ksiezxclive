use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::PathBuf,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, S_FALSE, S_OK, WAIT_TIMEOUT},
        Media::Audio::*,
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance},
            Diagnostics::ToolHelp::*,
            Threading::{
                GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
                WaitForSingleObject,
            },
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
        },
    },
    core::{BOOL, HRESULT, Interface, PWSTR},
};

pub(crate) struct Handle(pub HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: somos donos do handle retornado pelo Windows e o fechamos uma vez.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceId {
    pub pid: u32,
    birth: u64,
}

impl SourceId {
    pub fn parse(value: &str) -> Result<Self> {
        let (pid, birth) = value
            .split_once('@')
            .context("Use o identificador completo que aparece em fontes.")?;
        let id = Self {
            pid: pid
                .parse()
                .context("PID inválido no identificador de áudio.")?,
            birth: u64::from_str_radix(birth, 16)
                .context("Identificador da instância de áudio inválido.")?,
        };
        ensure!(
            id.pid != 0 && id.birth != 0,
            "Identificador de áudio inválido."
        );
        Ok(id)
    }
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}@{:016x}", self.pid, self.birth)
    }
}

pub(crate) struct Source {
    pub id: SourceId,
    pub path: PathBuf,
    handle: Handle,
}

impl Source {
    fn open(pid: u32) -> Result<Self> {
        // SAFETY: o PID vem da enumeração. Acesso limitado à consulta e espera.
        let handle = Handle(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )?
        });
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let mut image = vec![0_u16; 32768];
        let mut length = image.len() as u32;
        // SAFETY: o handle está aberto e os ponteiros de saída são graváveis.
        unsafe {
            GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user)?;
            QueryFullProcessImageNameW(
                handle.0,
                PROCESS_NAME_WIN32,
                PWSTR(image.as_mut_ptr()),
                &mut length,
            )?;
        }
        use std::os::windows::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_wide(&image[..length as usize]));
        ensure!(
            path.is_absolute() && path.file_name().is_some(),
            "O executável do processo não pôde ser identificado."
        );
        let birth = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        let source = Self {
            id: SourceId { pid, birth },
            path,
            handle,
        };
        ensure!(source.running(), "O processo de áudio foi encerrado.");
        Ok(source)
    }

    fn running(&self) -> bool {
        // SAFETY: o handle de processo tem permissão de sincronização.
        unsafe { WaitForSingleObject(self.handle.0, 0) == WAIT_TIMEOUT }
    }
}

pub(crate) fn discover() -> Result<BTreeMap<u32, Source>> {
    let sessions = discover_sessions();
    let windows = discover_windows();
    match (sessions, windows) {
        (Ok(mut sessions), Ok(windows)) => {
            // Chrome e outros navegadores também podem expor o processo auxiliar
            // de áudio no WASAPI. A janela identifica a raiz a oferecer ao usuário.
            sessions.retain(|_, source| {
                browser_name(&source.path).is_none()
                    || !windows
                        .values()
                        .any(|window| same_image(&window.path, &source.path))
            });
            sessions.extend(windows);
            Ok(sessions)
        }
        (Ok(sources), Err(_)) | (Err(_), Ok(sources)) => Ok(sources),
        (Err(error), Err(_)) => Err(error),
    }
}

fn discover_sessions() -> Result<BTreeMap<u32, Source>> {
    // SAFETY: COM está inicializado no MTA desta thread. O enumerador é
    // usado somente para ler sessões de reprodução de endpoints ativos.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let endpoints = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut candidates = BTreeMap::new();
        let mut shared = HashSet::new();
        let mut available = 0;
        let devices = endpoints.GetCount()?;
        for index in 0..devices {
            let sessions = (|| {
                let device = endpoints.Item(index)?;
                let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
                manager.GetSessionEnumerator()
            })();
            let Ok(sessions) = sessions else { continue };
            let Ok(count) = sessions.GetCount() else {
                continue;
            };
            available += 1;
            for i in 0..count {
                let Ok(session) = sessions.GetSession(i) else {
                    continue;
                };
                let Ok(state) = session.GetState() else {
                    continue;
                };
                if state != AudioSessionStateActive && state != AudioSessionStateInactive {
                    continue;
                }
                let Ok(session) = session.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                if session.IsSystemSoundsSession() != S_FALSE {
                    continue;
                }
                // O wrapper comum descarta códigos de sucesso como
                // AUDCLNT_S_NO_SINGLE_PROCESS. Só S_OK identifica um único PID.
                let mut pid = 0;
                let status = (Interface::vtable(&session).GetProcessId)(
                    Interface::as_raw(&session),
                    &mut pid,
                );
                if status == HRESULT(0x0889_000d) {
                    // AUDCLNT_S_NO_SINGLE_PROCESS
                    shared.insert(pid);
                }
                if status != S_OK || pid == 0 || candidates.contains_key(&pid) {
                    continue;
                }
                if let Ok(source) = Source::open(pid) {
                    if eligible_image(&source) {
                        candidates.insert(pid, source);
                    }
                }
            }
        }
        candidates.retain(|pid, _| !shared.contains(pid));
        ensure!(
            devices == 0 || available > 0,
            "Não foi possível enumerar as sessões de reprodução."
        );
        Ok(candidates)
    }
}

fn eligible_image(source: &Source) -> bool {
    let name = source
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_ascii_lowercase();
    !is_discord_path(&source.path)
        && !matches!(
            name.as_str(),
            "svchost.exe"
                | "audiodg.exe"
                | "services.exe"
                | "explorer.exe"
                | "applicationframehost.exe"
                | "shellexperiencehost.exe"
                | "startmenuexperiencehost.exe"
                | "searchapp.exe"
                | "searchhost.exe"
                | "systemsettings.exe"
                | "taskhostw.exe"
                | "rundll32.exe"
                | "dllhost.exe"
                | "msedgewebview2.exe"
                | "ksiezyclive.exe"
        )
}

pub(crate) fn browser_name(path: &std::path::Path) -> Option<&'static str> {
    match path
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "chrome.exe" => Some("Google Chrome"),
        "msedge.exe" => Some("Microsoft Edge"),
        "firefox.exe" => Some("Mozilla Firefox"),
        "brave.exe" => Some("Brave"),
        "opera.exe" => Some("Opera"),
        "vivaldi.exe" => Some("Vivaldi"),
        _ => None,
    }
}

fn same_image(a: &std::path::Path, b: &std::path::Path) -> bool {
    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
}

fn discover_windows() -> Result<BTreeMap<u32, Source>> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: EnumWindows invoca o callback de forma síncrona, com um
        // HWND válido e data apontando para o conjunto exclusivo do chamador.
        unsafe {
            if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
                let mut pid = 0;
                if GetWindowThreadProcessId(hwnd, Some(&mut pid)) != 0 && pid != 0 {
                    (&mut *(data.0 as *mut HashSet<u32>)).insert(pid);
                }
            }
        }
        true.into()
    }
    let mut pids = HashSet::<u32>::new();
    // SAFETY: o callback não retém o ponteiro e só o usa nesta chamada.
    unsafe { EnumWindows(Some(collect), LPARAM(&mut pids as *mut _ as isize))? };
    let snapshot = process_snapshot()?;
    let entries: HashMap<_, _> = snapshot.iter().map(|entry| (entry.pid, entry)).collect();
    let mut candidates = BTreeMap::new();
    for pid in pids {
        let Some(entry) = entries.get(&pid) else {
            continue;
        };
        let Ok(source) = Source::open(pid) else {
            continue;
        };
        if !source
            .path
            .file_name()
            .unwrap()
            .eq_ignore_ascii_case(&entry.executable)
            || !eligible_image(&source)
        {
            continue;
        }
        let source = window_root(source, &entries);
        candidates.insert(source.id.pid, source);
    }
    Ok(candidates)
}

fn window_root(mut source: Source, entries: &HashMap<u32, &Entry>) -> Source {
    while let Some(parent) = entries
        .get(&source.id.pid)
        .and_then(|entry| entries.get(&entry.parent))
    {
        if parent.pid == source.id.pid
            || !source
                .path
                .file_name()
                .unwrap()
                .eq_ignore_ascii_case(&parent.executable)
        {
            break;
        }
        let Ok(process) = Source::open(parent.pid) else {
            break;
        };
        if process.id.birth >= source.id.birth || !same_image(&process.path, &source.path) {
            break;
        }
        source = process;
    }
    source
}

fn discord_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    stem == "discord"
        || stem.starts_with("discordptb")
        || stem.starts_with("discordcanary")
        || stem.starts_with("discorddevelopment")
        || stem.starts_with("discordhelper")
        || stem.starts_with("discordsquirrel")
}

fn is_discord_path(path: &std::path::Path) -> bool {
    path.components()
        .any(|component| discord_name(&component.as_os_str().to_string_lossy()))
}

pub(crate) struct DiscordExclusion {
    root: Source,
}

impl DiscordExclusion {
    pub fn select() -> Result<Self> {
        Ok(Self {
            root: Self::current_root()?,
        })
    }

    pub fn pid(&self) -> u32 {
        self.root.id.pid
    }

    pub fn verify(&self) -> Result<()> {
        ensure!(
            self.root.running() && Self::current_root()?.id == self.root.id,
            "O processo do Discord mudou. O som do PC foi silenciado."
        );
        Ok(())
    }

    fn current_root() -> Result<Source> {
        let snapshot =
            process_snapshot().context("Não foi possível verificar o Discord desktop")?;
        let mut instances = BTreeMap::new();
        for entry in snapshot
            .iter()
            .filter(|entry| discord_name(&entry.executable))
        {
            let proc = Source::open(entry.pid)
                .context("Não foi possível confirmar um processo do Discord.")?;
            ensure!(
                proc.path
                    .file_name()
                    .unwrap()
                    .eq_ignore_ascii_case(&entry.executable),
                "A identidade de um processo do Discord mudou."
            );
            instances.insert(entry.pid, (entry.parent, proc));
        }
        let mut roots = instances.iter().filter(|(_, (parent, proc))| {
            instances
                .get(parent)
                .is_none_or(|(_, parent)| parent.id.birth >= proc.id.birth)
        });
        let (&pid, (_, root)) = roots
            .next()
            .context("Abra o Discord desktop para usar Som do PC, exceto Discord.")?;
        ensure!(
            roots.next().is_none(),
            "Há mais de uma instância do Discord desktop. Feche as instâncias extras para usar esse modo."
        );
        ensure!(
            matches!(
                root.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .as_str(),
                "discord.exe" | "discordptb.exe" | "discordcanary.exe" | "discorddevelopment.exe"
            ),
            "Não foi encontrado o processo principal do Discord desktop."
        );
        Ok(instances.remove(&pid).unwrap().1)
    }
}

struct Entry {
    pid: u32,
    parent: u32,
    executable: String,
}

fn process_snapshot() -> Result<Vec<Entry>> {
    // SAFETY: o snapshot é de processos, seu handle é próprio e entry declara
    // o tamanho exigido pelas funções de enumeração.
    unsafe {
        let snapshot = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut items = Vec::new();
        Process32FirstW(snapshot.0, &mut entry)?;
        loop {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            items.push(Entry {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                executable: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
            if let Err(err) = Process32NextW(snapshot.0, &mut entry) {
                if err.code() == HRESULT::from_win32(18) {
                    // ERROR_NO_MORE_FILES
                    return Ok(items);
                }
                return Err(err.into());
            }
        }
    }
}

pub(crate) struct SourceTree {
    root: Source,
    ancestry_verified: bool,
    // O handle mantém a identidade dos intermediários conhecidos, inclusive
    // quando eles terminam antes dos próprios filhos.
    descendants: HashMap<u32, Source>,
}

impl SourceTree {
    pub fn new(root: Source) -> Self {
        Self {
            root,
            ancestry_verified: false,
            descendants: HashMap::new(),
        }
    }

    pub fn pid(&self) -> u32 {
        self.root.id.pid
    }

    pub fn verify(&mut self) -> Result<()> {
        ensure!(
            self.root.running(),
            "O aplicativo selecionado foi encerrado."
        );
        let entries =
            process_snapshot().context("Não foi possível verificar a árvore de áudio.")?;
        let by_pid: HashMap<_, _> = entries.iter().map(|entry| (entry.pid, entry)).collect();
        if !self.ancestry_verified {
            verify_ancestry(&self.root, &by_pid)?;
            self.ancestry_verified = true;
        }
        let mut children: HashMap<u32, Vec<&Entry>> = HashMap::new();
        for entry in &entries {
            children.entry(entry.parent).or_default().push(entry);
        }

        let mut queue = VecDeque::from([(self.pid(), self.root.id.birth)]);
        for (&pid, source) in &self.descendants {
            // Um PID conhecido que desapareceu do snapshot ainda pode ser pai
            // de um descendente vivo. Se reaparecer, confirme a instância.
            if by_pid.contains_key(&pid) && !source.running() {
                if let Ok(current) = Source::open(pid) {
                    ensure!(
                        current.id == source.id,
                        "A árvore de áudio ficou ambígua após a reutilização de um PID."
                    );
                } else if children.contains_key(&pid) {
                    bail!(
                        "Não foi possível confirmar um processo intermediário da árvore de áudio."
                    );
                }
            }
            queue.push_back((pid, source.id.birth));
        }

        while let Some((parent, birth)) = queue.pop_front() {
            for entry in children.get(&parent).into_iter().flatten() {
                if entry.pid == self.pid() || self.descendants.contains_key(&entry.pid) {
                    continue;
                }
                let child = Source::open(entry.pid)
                    .context("Não foi possível confirmar um subprocesso da fonte de áudio.")?;
                // Um PPID que já foi reutilizado pode apontar para um processo
                // mais novo que o próprio filho. Esse filho não é da árvore.
                if child.id.birth < birth {
                    continue;
                }
                let child_name = child.path.file_name().unwrap().to_string_lossy();
                ensure!(
                    child_name.eq_ignore_ascii_case(&entry.executable),
                    "O executável de um subprocesso mudou durante a verificação."
                );
                ensure!(
                    !discord_name(&entry.executable) && !is_discord_path(&child.path),
                    "A árvore da fonte contém um cliente desktop do Discord."
                );
                ensure!(
                    self.descendants.len() < 2048,
                    "A árvore da fonte excedeu o limite de processos monitorados."
                );
                queue.push_back((entry.pid, child.id.birth));
                self.descendants.insert(entry.pid, child);
            }
        }
        ensure!(
            self.root.running(),
            "O aplicativo selecionado foi encerrado."
        );
        Ok(())
    }
}

fn verify_ancestry(root: &Source, entries: &HashMap<u32, &Entry>) -> Result<()> {
    let mut instance = root.id;
    let mut visited = HashSet::from([instance.pid]);
    while let Some(entry) = entries.get(&instance.pid) {
        let Some(parent) = entries.get(&entry.parent).filter(|entry| entry.pid != 0) else {
            break;
        };
        ensure!(
            visited.insert(parent.pid),
            "O Windows retornou uma cadeia de processos ambígua."
        );
        ensure!(
            !discord_name(&parent.executable),
            "A fonte é um subprocesso de um cliente desktop do Discord."
        );
        let process = Source::open(parent.pid)
            .context("Não foi possível confirmar um processo pai da fonte.")?;
        if process.id.birth > instance.birth {
            break;
        }
        ensure!(
            !is_discord_path(&process.path),
            "A fonte é um subprocesso de um cliente desktop do Discord."
        );
        instance = process.id;
    }
    Ok(())
}
