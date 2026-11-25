import { Room, RoomEvent, Track } from "livekit-client";
import { Maximize, Moon, Play, Volume2, VolumeX } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import "./style.css";

type View = "ready" | "connecting" | "live" | "reconnecting" | "ended" | "error";
const label: Record<View, string> = {
  ready: "Pronto para assistir", connecting: "Conectando…", live: "Ao vivo",
  reconnecting: "Reconectando…", ended: "Live encerrada", error: "Conexão indisponível",
};

function Viewer() {
  const [view, setView] = useState<View>("ready");
  const [hasVideo, setHasVideo] = useState(false);
  const [hasAudio, setHasAudio] = useState(false);
  const [sound, setSound] = useState(false);
  const [volume, setVolume] = useState(0.85);
  const [muted, setMuted] = useState(false);
  const video = useRef<HTMLVideoElement>(null);
  const audio = useRef<HTMLAudioElement>(null);
  const player = useRef<HTMLDivElement>(null);
  const room = useRef<Room | null>(null);
  const connecting = useRef(false);

  useEffect(() => () => { void room.current?.disconnect(); }, []);
  useEffect(() => {
    if (audio.current) {
      audio.current.volume = volume;
      audio.current.muted = muted;
    }
  }, [volume, muted, hasAudio]);

  async function watch() {
    if (connecting.current) return;
    connecting.current = true;
    setView("connecting");
    const session = new Room();
    room.current = session;
    const ifCurrent = (action: () => void) => { if (room.current === session) action(); };
    session.on(RoomEvent.TrackSubscribed, (track) => ifCurrent(() => {
      if (track.kind === Track.Kind.Video && video.current) { track.attach(video.current); setHasVideo(true); }
      if (track.kind === Track.Kind.Audio && audio.current) { track.attach(audio.current); setHasAudio(true); }
    }));
    session.on(RoomEvent.TrackUnsubscribed, (track) => ifCurrent(() => {
      if (track.kind === Track.Kind.Video && video.current) { track.detach(video.current); setHasVideo(false); }
      if (track.kind === Track.Kind.Audio && audio.current) { track.detach(audio.current); setHasAudio(false); }
    }));
    session.on(RoomEvent.AudioPlaybackStatusChanged, () => ifCurrent(() => setSound(session.canPlaybackAudio)));
    session.on(RoomEvent.Reconnecting, () => ifCurrent(() => setView("reconnecting")));
    session.on(RoomEvent.Reconnected, () => ifCurrent(() => setView("live")));
    session.on(RoomEvent.Disconnected, () => ifCurrent(() => {
      setHasVideo(false);
      setHasAudio(false);
      setView("ended");
    }));
    void session.startAudio().catch(() => {});
    try {
      const invite = location.pathname.split("/").filter(Boolean).at(-1) ?? "";
      const response = await fetch(`/api/assistir/${encodeURIComponent(invite)}`, { method: "POST", signal: AbortSignal.timeout(12_000) });
      if (response.status === 410) {
        room.current = null;
        setView("ended");
        return;
      }
      if (!response.ok) throw new Error("Convite indisponível");
      const { token, url } = await response.json() as { token: string; url: string };
      await session.connect(url, token);
      setSound(session.canPlaybackAudio);
      setView("live");
    } catch {
      room.current = null;
      await session.disconnect();
      setView("error");
    } finally {
      connecting.current = false;
    }
  }

  const overlay = view !== "live" || !hasVideo;
  return (
    <main>
      <header><span><Moon size={18} /> ksiezyclive</span><output aria-live="polite">{label[view]}</output></header>
      <div className="player" ref={player}>
        <video ref={video} autoPlay muted playsInline />
        <audio ref={audio} autoPlay />
        {overlay && <div className="overlay">
          <h1>{view === "live" ? "Aguardando vídeo…" : view === "ready" ? "Assistir à live" : label[view]}</h1>
          {view === "ready" || view === "error" ? <button className="watch" onClick={() => void watch()}><Play size={17} />{view === "error" ? "Tentar novamente" : "Assistir"}</button> : null}
        </div>}
        {hasVideo && <div className="controls">
          {hasAudio && (!sound ? <button onClick={() => void room.current?.startAudio().catch(() => {})}><Volume2 size={18} /> Ativar som</button> : <>
            <button aria-label={muted ? "Ativar som" : "Silenciar"} onClick={() => setMuted((value) => !value)}>{muted ? <VolumeX size={19} /> : <Volume2 size={19} />}</button>
            <input aria-label="Volume" type="range" min="0" max="1" step="0.01" value={volume} onChange={(event) => setVolume(Number(event.target.value))} />
          </>)}
          <button className="fullscreen" aria-label="Tela cheia" onClick={() => {
            if (document.fullscreenElement) void document.exitFullscreen().catch(() => {});
            else if (player.current?.requestFullscreen) void player.current.requestFullscreen().catch(() => {});
            else (video.current as (HTMLVideoElement & { webkitEnterFullscreen?(): void }) | null)?.webkitEnterFullscreen?.();
          }}><Maximize size={19} /></button>
        </div>}
      </div>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(<Viewer />);
