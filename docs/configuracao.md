# Configurar o ksiezyclive

## Como compilar

O alvo do projeto é Windows 10 22H2 x64. Para rodar o app, o PC precisa de Direct3D 11, dos componentes de mídia do Windows e do [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/). Em edições Windows N, pode ser necessário instalar o Media Feature Pack.

Para compilar o código, use Node.js 22.18 ou posterior, Rust com MSVC, Visual Studio Build Tools com a opção de desenvolvimento desktop em C++ e o Windows SDK. Rode estes comandos na pasta do projeto.

```powershell
npm ci
rustup target add x86_64-pc-windows-msvc
npm run desktop build -- --no-bundle --target x86_64-pc-windows-msvc
```

O executável fica em `src-tauri\target\x86_64-pc-windows-msvc\release\ksiezyclive.exe`. Para abrir em modo de desenvolvimento, use `npm run desktop dev`.

## Como testar a live no próprio PC

O teste local usa dois terminais. No primeiro, coloque o executável do [LiveKit Server](https://github.com/livekit/livekit/releases) na pasta do projeto e inicie o serviço.

```powershell
.\livekit-server.exe --config livekit.local.yaml
```

No segundo, abra o PowerShell 7 na mesma pasta. Na primeira vez, copie o arquivo de exemplo e gere o código privado que o desktop vai usar para abrir as salas.

```powershell
Copy-Item .env.example .env
$codigo = [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
(Get-Content .env) -replace '^KSIEZYC_HOST_TOKEN=$', "KSIEZYC_HOST_TOKEN=$codigo" |
  Set-Content -Encoding utf8 .env
npm ci
npm run build
npm run servidor
```

No desktop, marque **Usar servidor personalizado**, coloque `http://127.0.0.1:3001` e use o código de `KSIEZYC_HOST_TOKEN` do seu `.env`. Abra uma live e cole o convite completo no navegador desse mesmo PC. Nos próximos testes, basta iniciar o LiveKit e rodar `npm run servidor` novamente.

## Como usar um domínio próprio

O link com `127.0.0.1` funciona no computador que está rodando o servidor. Para alguém assistir de outra rede, a página e a API precisam de um endereço HTTPS público. Uma opção é usar Cloudflare Tunnel e apontar o domínio para `http://127.0.0.1:3001`. O túnel atende a página do espectador e a API de convites. Durante a live, o desktop publica áudio e vídeo diretamente no LiveKit por WebRTC, e o navegador recebe as faixas desse servidor. Por isso o LiveKit também precisa estar acessível aos participantes. Dá para usar LiveKit Cloud ou hospedar uma instância própria. [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/) · [Portas usadas pelo LiveKit](https://docs.livekit.io/transport/self-hosting/ports-firewall/)

No `.env`, configure `PUBLIC_BASE_URL` com o domínio dos convites, `LIVEKIT_SERVER_URL` com o endereço que a API usa para administrar as salas e `LIVEKIT_PUBLIC_URL` com o endereço WSS usado pelo desktop e pelo navegador. Use suas próprias credenciais em `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET` e `KSIEZYC_HOST_TOKEN`. Se você hospedar o LiveKit, mantenha `room.auto_create: false`. Depois, reinicie a API e coloque o endereço HTTPS no desktop.
