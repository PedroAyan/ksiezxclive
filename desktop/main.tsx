import { invoke, isTauri } from "@tauri-apps/api/core";
import { Circle, Clipboard, FolderOpen, Monitor, Radio, RefreshCw, Square, Volume2 } from "lucide-react";
import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import "./style.css";

const appIcon = new URL("../src-tauri/icons/icon.ico", import.meta.url).href;

type Mode = "live" | "record";
const exceptDiscord = "exceto-discord";
type MonitorChoice = { id: string; width: number; height: number };
type AudioChoice = { id: string; name: string; path: string };
type WindowChoice = { id: string; title: string; minimized: boolean };
type Choices = { screens: MonitorChoice[]; windows: WindowChoice[]; windowError?: string; audio: AudioChoice[]; audioError?: string };
type Status = {
  phase: "idle" | "starting" | "live" | "recording" | "reconnecting" | "stopping" | "cleanup" | "error" | "saved";
  invite?: string;
  message?: string;
  warning?: string;
  saved?: string;
  elapsed: number;
};

const statusName: Record<Status["phase"], string> = {
  idle: "Pronto", starting: "Preparando", live: "Ao vivo", recording: "Gravando",
  reconnecting: "Reconectando", stopping: "Encerrando", cleanup: "Encerramento pendente",
  error: "Sessão interrompida", saved: "Gravação salva",
};

function App() {
  const [mode, setMode] = useState<Mode>("live");
  const [screens, setScreens] = useState<MonitorChoice[]>([]);
  const [audioChoices, setAudioChoices] = useState<AudioChoice[]>([]);
  const [screen, setScreen] = useState("");
  const [videoSource, setVideoSource] = useState<"monitor" | "window">("monitor");
  const [windowId, setWindowId] = useState("");
  const [windows, setWindows] = useState<WindowChoice[]>([]);
  const [windowError, setWindowError] = useState("");
  const [audio, setAudio] = useState("");
  const [resolution, setResolution] = useState(720);
  const [fps, setFps] = useState(30);
  const [folder, setFolder] = useState("");
  const [customServer, setCustomServer] = useState(false);
  const [apiUrl, setApiUrl] = useState("");
  const [hostToken, setHostToken] = useState("");
  const [status, setStatus] = useState<Status>({ phase: "idle", elapsed: 0 });
  const [error, setError] = useState("");
  const [audioError, setAudioError] = useState("");
  const [loading, setLoading] = useState(true);
  const [pending, setPending] = useState(false);
  const [copied, setCopied] = useState("");
  const running = ["starting", "live", "recording", "reconnecting", "stopping"].includes(status.phase);
  const locked = running || pending || status.phase === "cleanup";
  const shareWindow = mode === "live" && videoSource === "window";

  async function refresh() {
    setLoading(true);
    try {
      const choices = await invoke<Choices>("choices");
      setScreens(choices.screens);
      setScreen((current) => choices.screens.some((item) => item.id === current) ? current : choices.screens[0]?.id ?? "");
      setWindows(choices.windows);
      setWindowId((current) => choices.windows.some((item) => item.id === current) ? current : "");
      setWindowError(choices.windowError ?? "");
      setAudioChoices(choices.audio);
      setAudio((current) => current === exceptDiscord || choices.audio.some((item) => item.id === current) ? current : "");
      setAudioError(choices.audioError ?? "");
    } catch (cause) {
      setError(String(cause));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    if (!isTauri()) {
      setLoading(false);
      setError("Abra o executável do ksiezyclive para selecionar sua tela.");
      return;
    }
    void refresh();
    let live = true;
    const timer = window.setInterval(() => {
      void invoke<Status>("status").then((value) => {
        if (live) setStatus((current) =>
          current.phase === value.phase && current.elapsed === value.elapsed &&
          current.invite === value.invite && current.message === value.message &&
          current.warning === value.warning && current.saved === value.saved ? current : value);
      }).catch(() => {});
    }, 350);
    return () => { live = false; window.clearInterval(timer); };
  }, []);

  async function action(command: string, args?: Record<string, unknown>) {
    setError("");
    setPending(true);
    try {
      setStatus(await invoke<Status>(command, args));
    } catch (cause) {
      setError(String(cause));
    } finally {
      setPending(false);
    }
  }

  async function chooseFolder() {
    try {
      const selected = await invoke<string | null>("plugin:dialog|open", { options: { directory: true, multiple: false } });
      if (selected) setFolder(selected);
    } catch (cause) {
      setError(String(cause));
    }
  }

  async function copyInvite(invite: string) {
    try {
      await invoke("plugin:clipboard-manager|write_text", { text: invite });
      setCopied(invite);
    } catch (cause) {
      setError(String(cause));
    }
  }

  const elapsed = [Math.floor(status.elapsed / 60), status.elapsed % 60].map((n) => String(n).padStart(2, "0")).join(":");

  return (
    <main className="app">
      <header className="top">
        <div className="brand"><img className="app-icon" src={appIcon} alt="" width={31} height={31} />ksiezyclive</div>
        <output className="indicator" data-phase={status.phase} aria-live="polite">
          <span />{statusName[status.phase]}{running && status.elapsed > 0 && <time>{elapsed}</time>}
        </output>
      </header>
      <div className="workspace">
        <div className="heading">
          <div><h1>{mode === "live" ? "Transmitir tela" : "Gravar tela"}</h1>
            <p>{mode === "live" ? "Escolha o que compartilhar e envie o link." : "Salve um MP4 no seu computador."}</p>
          </div>
          <div className="mode" aria-label="Modo">
            <button type="button" aria-pressed={mode === "live"} disabled={locked} onClick={() => setMode("live")}><Radio size={16} /> Live</button>
            <button type="button" aria-pressed={mode === "record"} disabled={locked} onClick={() => setMode("record")}><Circle size={15} /> Gravar</button>
          </div>
        </div>
        <form className="settings" onSubmit={(event) => {
          event.preventDefault();
          if (!locked) {
            const server = mode === "live" && customServer ? { apiUrl, hostToken } : null;
            void action("start", { request: { mode, screen, window: shareWindow ? windowId : null, audio: audio || null, resolution, fps, folder: folder || null, server } });
          }
        }}>
          <fieldset disabled={locked}>
            {mode === "live" && <div className="row">
              <div className="field-title"><Monitor size={19} /><label htmlFor="video-source">Compartilhar</label></div>
              <select id="video-source" value={videoSource} onChange={(event) => {
                const source = event.target.value === "window" ? "window" : "monitor";
                setVideoSource(source);
                if (source === "window") void refresh();
              }}>
                <option value="monitor">Tela inteira</option>
                <option value="window">Janela de aplicativo</option>
              </select>
            </div>}
            {shareWindow ? <div className="row audio-row">
              <div className="field-title"><label htmlFor="window">Janela</label></div>
              <div>
                <div className="select-action">
                  <select id="window" value={windowId} disabled={loading || !windows.length} onChange={(event) => setWindowId(event.target.value)} aria-describedby="window-help">
                    <option value="">{loading ? "Buscando janelas…" : windows.length ? "Selecione uma janela" : "Nenhuma janela disponível"}</option>
                    {windows.map((item) => <option key={item.id} value={item.id}>{item.title}{item.minimized ? " (minimizada)" : ""}</option>)}
                  </select>
                  <button type="button" className="icon-button" aria-label="Atualizar janelas" title="Atualizar janelas" disabled={loading || !isTauri()} onClick={() => void refresh()}><RefreshCw size={17} /></button>
                </div>
                <p className="hint" id="window-help">{windowError || "Restaure a janela se estiver minimizada. Em jogos, use o modo janela sem bordas."}</p>
              </div>
            </div> : <div className="row">
              <div className="field-title"><Monitor size={19} /><label htmlFor="screen">Tela inteira</label></div>
              <select id="screen" value={screen} disabled={loading || !screens.length} onChange={(event) => setScreen(event.target.value)}>
                {!screens.length && <option value="">{loading ? "Buscando telas…" : "Nenhuma tela disponível"}</option>}
                {screens.map((item, index) => <option key={item.id} value={item.id}>Tela {item.id.match(/\d+$/)?.[0] ?? index + 1} · {item.width} × {item.height}</option>)}
              </select>
            </div>}
            <div className="row audio-row">
              <div className="field-title"><Volume2 size={19} /><label htmlFor="audio">Áudio</label></div>
              <div>
                <div className="select-action">
                  <select id="audio" value={audio} onChange={(event) => setAudio(event.target.value)} aria-describedby="audio-help">
                    <option value="">Sem áudio</option>
                    <option value={exceptDiscord}>Som do PC, exceto Discord</option>
                    {audioChoices.map((item) => <option title={item.path} value={item.id} key={item.id}>
                      {item.name}{audioChoices.some((other) => other.id !== item.id && other.name === item.name) ? ` (${item.id.split("@")[0]})` : ""}
                    </option>)}
                  </select>
                  <button type="button" className="icon-button" title="Atualizar telas e fontes de áudio" aria-label="Atualizar telas e fontes de áudio" disabled={loading || !isTauri()} onClick={() => { setError(""); void refresh(); }}>
                    <RefreshCw size={17} />
                  </button>
                </div>
                <p className="hint" id="audio-help">{audioError || (audio === exceptDiscord
                  ? "Deixe uma instância do Discord desktop aberta. Se ela fechar ou mudar, o áudio para."
                  : "Abra o aplicativo ou inicie a reprodução nele e use o botão de atualizar.")}</p>
                {audio && <p className="hint">{audio === exceptDiscord
                  ? "Chamadas do Discord no navegador entram no som do PC."
                  : "Se usar Discord Web, deixe o navegador da chamada fora da seleção."}</p>}
              </div>
            </div>
            {mode === "live" ? (
              <div className="row quality-row">
                <span className="field-title">Qualidade</span>
                <div className="quality">
                  <label><span>Resolução</span><select value={resolution} onChange={(event) => setResolution(Number(event.target.value))}>
                    <option value={480}>480p</option><option value={720}>720p</option><option value={1080}>1080p</option>
                  </select></label>
                  <label><span>Quadros por segundo</span><select value={fps} onChange={(event) => setFps(Number(event.target.value))}>
                    <option value={30}>30 fps</option><option value={60}>60 fps</option>
                  </select></label>
                </div>
              </div>
            ) : (
              <div className="row">
                <span className="field-title">Salvar em</span>
                <button className="folder" type="button" onClick={() => void chooseFolder()}><FolderOpen size={17} /><span>{folder || "Vídeos"}</span><small>Alterar</small></button>
              </div>
            )}
            {mode === "live" && <div className="row">
              <span className="field-title">Servidor</span>
              <div className="server-settings">
                <label><input type="checkbox" checked={customServer} onChange={(event) => setCustomServer(event.target.checked)} /> Usar servidor personalizado</label>
                {customServer && <>
                  <label>Endereço da API<input type="url" value={apiUrl} required placeholder="https://live.exemplo.com" onChange={(event) => setApiUrl(event.target.value)} /></label>
                  <label>Código do anfitrião<input type="password" value={hostToken} required minLength={32} autoComplete="off" onChange={(event) => setHostToken(event.target.value)} /></label>
                  <p className="hint">Use a URL da sua API com HTTPS. O domínio do convite é definido no servidor.</p>
                </>}
              </div>
            </div>}
          </fieldset>
          {(error || status.message || status.warning) && <div className="notice" role="status">{error || status.message || status.warning}</div>}
          {status.saved && <div className="result"><FolderOpen size={18} /><span>{status.saved}</span><button type="button" onClick={() => void invoke("show_recording").catch((cause) => setError(String(cause)))}>Abrir pasta</button></div>}
          {status.invite && status.phase === "live" && <div className="result">
            <input aria-label="Link para assistir" readOnly value={status.invite} onFocus={(event) => event.target.select()} />
            <button type="button" onClick={() => void copyInvite(status.invite!)}><Clipboard size={16} />{copied === status.invite ? "Link copiado" : "Copiar link"}</button>
          </div>}
          <footer>
            {locked ? <button className="primary stop" type="button" disabled={pending || status.phase === "stopping"} onClick={() => void action("stop")}><Square size={16} />{status.phase === "cleanup" ? "Tentar encerrar novamente" : status.phase === "starting" ? "Cancelar" : status.phase === "stopping" ? "Encerrando…" : mode === "live" ? "Encerrar live" : "Parar gravação"}</button>
              : <button className="primary" type="submit" disabled={!(shareWindow ? windowId : screen) || loading}>{mode === "live" ? <Radio size={19} /> : <Circle size={16} />}{mode === "live" ? "Abrir live" : "Iniciar gravação"}</button>}
          </footer>
        </form>
      </div>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
