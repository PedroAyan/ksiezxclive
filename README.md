# ksiezyclive

Fiz o ksiezyclive para conseguir transmitir um jogo para os meus amigos sem mandar junto o áudio da chamada do Discord. No app, eu escolho a janela que quero mostrar e, separadamente, o que a pessoa vai ouvir. Quem recebe o link abre no navegador e clica em **Assistir**. Também dá para gravar um monitor em MP4 quando eu só quero guardar um vídeo no PC.

## Como usar

Baixe o arquivo **ksiezyclive-windows-x64.zip**, extraia e abra `ksiezyclive.exe`. O app é portátil. Para assistir à live, basta o navegador. [Baixar a versão para Windows](https://github.com/PedroAyan/ksiezxclive/releases)

Se quiser experimentar primeiro, abra a aba **Gravar**, escolha o monitor e clique em **Iniciar gravação**. Quando parar, o aplicativo mostra onde salvou o MP4. Esse é o jeito mais rápido de testar porque a gravação funciona offline.

Para fazer uma live, você precisa de um servidor configurado. Na aba **Live**, escolha **Tela inteira** para mostrar um monitor ou **Janela de aplicativo** para mostrar só o jogo, o Chrome ou outra janela. Se o jogo tiver um launcher, espere a partida abrir e selecione a janela dela. Se a janela estiver minimizada, restaure antes de começar.

No campo **Áudio**, escolha o jogo para transmitir só o som dele. Também dá para usar **Sem áudio** ou **Som do PC, exceto Discord**. Para essa última opção, deixe uma única instância do Discord desktop aberta. Se a chamada estiver no Discord Web, a voz sai pelo navegador e pode entrar na transmissão.

A configuração inicial é 720p a 30 fps. Dá para escolher 480p, 720p ou 1080p, a 30 ou 60 fps. Depois de clicar em **Abrir live**, espere aparecer **Ao vivo** e copie o link. Cada live tem seu próprio convite.

## Como o projeto funciona

Usei Rust para capturar o vídeo e o áudio no Windows. A imagem vem do Windows Graphics Capture com Direct3D 11, o áudio vem do WASAPI e a gravação em MP4 usa o Media Foundation. A interface do desktop foi feita com Tauri e React.

Na live, o desktop publica o vídeo e o áudio no LiveKit. A API em Fastify cria a sala, gera o convite e encerra o acesso quando a transmissão termina. A página que o espectador abre também foi feita em React.

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

O link com `127.0.0.1` funciona no computador que está rodando o servidor. Para alguém assistir de outra rede, a página e a API precisam de um endereço HTTPS público. Uma opção é usar Cloudflare Tunnel e apontar o domínio para `http://127.0.0.1:3001`. O vídeo e o áudio passam pelo LiveKit, então ele também precisa estar acessível. Dá para usar LiveKit Cloud ou hospedar uma instância própria. [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/) · [Portas usadas pelo LiveKit](https://docs.livekit.io/transport/self-hosting/ports-firewall/)

No `.env`, configure `PUBLIC_BASE_URL` com o domínio dos convites, `LIVEKIT_SERVER_URL` com o endereço que a API usa para administrar as salas e `LIVEKIT_PUBLIC_URL` com o endereço WSS usado pelo desktop e pelo navegador. Use suas próprias credenciais em `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET` e `KSIEZYC_HOST_TOKEN`. Se você hospedar o LiveKit, mantenha `room.auto_create: false`. Depois, reinicie a API e coloque o endereço HTTPS no desktop.

## O que já foi testado

O build para Windows x64 passou. No teste local, Chrome e Steam apareceram no seletor, inclusive com janelas minimizadas. Também foram conferidos a criação da sala, a abertura da página e o encerramento do convite. Ainda falta testar em outro PC, assistir de fora da rede e conferir o áudio de uma partida com uma chamada do Discord acontecendo ao mesmo tempo.
