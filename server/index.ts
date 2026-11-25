import { randomBytes, timingSafeEqual } from "node:crypto";
import { resolve } from "node:path";
import fastifyStatic from "@fastify/static";
import Fastify, { LogController } from "fastify";
import { AccessToken, RoomServiceClient } from "livekit-server-sdk";

function required(name: string) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`Configure ${name} no servidor.`);
  return value;
}

const hostToken = Buffer.from(required("KSIEZYC_HOST_TOKEN"));
if (hostToken.length < 32) throw new Error("KSIEZYC_HOST_TOKEN precisa ter ao menos 32 caracteres.");
const apiKey = required("LIVEKIT_API_KEY");
const apiSecret = required("LIVEKIT_API_SECRET");
const publicBase = new URL(required("PUBLIC_BASE_URL"));
const livekitUrl = new URL(required("LIVEKIT_PUBLIC_URL"));
const local = (url: URL) => ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
if (!["http:", "https:"].includes(publicBase.protocol) || (!local(publicBase) && publicBase.protocol !== "https:")) {
  throw new Error("Use HTTPS para o endereço público do espectador.");
}
if (!["ws:", "wss:"].includes(livekitUrl.protocol) || (!local(livekitUrl) && livekitUrl.protocol !== "wss:")) {
  throw new Error("Use WSS para o endereço público do LiveKit.");
}
if (!local(publicBase) && (local(livekitUrl) || livekitUrl.protocol !== "wss:")) {
  throw new Error("Um convite público precisa de um endereço LiveKit público em WSS.");
}
if (!local(publicBase) && apiSecret === "ksiezyclive-apenas-desenvolvimento-local") {
  throw new Error("Defina uma chave e um segredo próprios do LiveKit para uso público.");
}
const rooms = new RoomServiceClient(required("LIVEKIT_SERVER_URL"), apiKey, apiSecret);
const app = Fastify({
  logger: { redact: ["req.headers.authorization"] },
  logController: new LogController({ disableRequestLogging: true }),
});

type Live = { room: string; invite: string; state: "preparing" | "active" | "closing"; createdAt: number };
const lives = new Map<string, Live>();
const invites = new Map<string, string>();

function hostAuthorized(authorization?: string) {
  if (!authorization?.startsWith("Bearer ")) return false;
  const received = Buffer.from(authorization.slice(7));
  return received.length === hostToken.length && timingSafeEqual(received, hostToken);
}

async function token(room: string, publisher: boolean) {
  const access = new AccessToken(apiKey, apiSecret, {
    identity: `${publisher ? "host" : "visitante"}-${randomBytes(12).toString("hex")}`,
    ttl: "90s",
  });
  access.addGrant({ room, roomJoin: true, canPublish: publisher, canSubscribe: !publisher, canPublishData: false });
  return access.toJwt();
}

async function deleteLive(id: string, live: Live) {
  live.state = "closing";
  invites.delete(live.invite);
  try {
    await rooms.deleteRoom(live.room);
  } catch (error) {
    if ((error as { status?: number }).status !== 404) throw error;
  }
  lives.delete(id);
}

app.get("/api/saude", async () => ({ ok: true }));

app.post("/api/lives", async (request, reply) => {
  if (!hostAuthorized(request.headers.authorization)) return reply.code(401).send({ message: "Acesso do anfitrião inválido." });
  if (lives.size >= 8) return reply.code(429).send({ message: "Há muitas salas em andamento." });
  const id = randomBytes(18).toString("base64url");
  const live: Live = { room: `live-${id}`, invite: randomBytes(24).toString("base64url"), state: "preparing", createdAt: Date.now() };
  lives.set(id, live);
  try {
    const publisherToken = await token(live.room, true);
    await rooms.createRoom({ name: live.room, emptyTimeout: 120, departureTimeout: 60 });
    return { id, url: livekitUrl.href, token: publisherToken };
  } catch (error) {
    lives.delete(id);
    request.log.error(error, "Falha ao criar sala");
    return reply.code(503).send({ message: "O servidor de vídeo está indisponível." });
  }
});

app.post<{ Params: { id: string } }>("/api/lives/:id/ready", async (request, reply) => {
  if (!hostAuthorized(request.headers.authorization)) return reply.code(401).send({ message: "Acesso do anfitrião inválido." });
  const live = lives.get(request.params.id);
  if (!live || live.state === "closing") return reply.code(410).send({ message: "A live foi encerrada." });
  live.state = "active";
  invites.set(live.invite, request.params.id);
  return { invite: new URL(`/assistir/${live.invite}`, publicBase).href };
});

app.delete<{ Params: { id: string } }>("/api/lives/:id", async (request, reply) => {
  if (!hostAuthorized(request.headers.authorization)) return reply.code(401).send({ message: "Acesso do anfitrião inválido." });
  const live = lives.get(request.params.id);
  if (!live) return reply.code(204).send();
  try {
    await deleteLive(request.params.id, live);
    return reply.code(204).send();
  } catch (error) {
    request.log.error(error, "Falha ao encerrar sala");
    return reply.code(503).send({ message: "O convite foi invalidado. Tente encerrar a sala novamente." });
  }
});

app.post<{ Params: { invite: string } }>("/api/assistir/:invite", async (request, reply) => {
  reply.header("Cache-Control", "no-store");
  const id = invites.get(request.params.invite);
  const live = id ? lives.get(id) : undefined;
  if (live?.state !== "active") return reply.code(410).send({ message: "A live foi encerrada." });
  const viewerToken = await token(live.room, false);
  if (live.state !== "active") return reply.code(410).send({ message: "A live foi encerrada." });
  return { token: viewerToken, url: livekitUrl.href };
});

let sweeping = false;
const reap = setInterval(() => {
  if (sweeping || lives.size === 0) return;
  sweeping = true;
  const snapshot = [...lives];
  void (async () => {
    for (const [id, live] of snapshot) {
      if (live.state === "closing" || (live.state === "preparing" && Date.now() - live.createdAt > 120_000)) {
        await deleteLive(id, live).catch((error) => app.log.error(error, "Falha ao limpar sala"));
      }
    }
    const active = snapshot.filter(([, live]) => live.state === "active");
    if (active.length === 0) return;
    const existing = new Set((await rooms.listRooms(active.map(([, live]) => live.room))).map((room) => room.name));
    for (const [id, live] of active) {
      if (!existing.has(live.room) && live.state === "active") {
        invites.delete(live.invite);
        lives.delete(id);
      }
    }
  })().catch((error) => app.log.error(error, "Falha ao verificar salas ativas")).finally(() => { sweeping = false; });
}, 30_000);
reap.unref();
app.addHook("onClose", () => { clearInterval(reap); });
const dist = resolve("dist");
await app.register(fastifyStatic, { root: resolve(dist, "assets"), prefix: "/assets/" });
app.get("/assistir/:invite", async (_request, reply) => reply
  .header("Referrer-Policy", "no-referrer")
  .header("Cache-Control", "no-store")
  .sendFile("assistir.html", dist));
await app.listen({ host: process.env.HOST ?? "127.0.0.1", port: Number(process.env.PORT ?? 3001) });
