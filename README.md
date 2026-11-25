# ksiezyclive

Aplicativo de Windows para compartilhar um monitor ou uma janela com quem tem o link da live. O anfitrião escolhe a imagem e a fonte de áudio separadamente. O espectador abre o convite no navegador e clica em **Assistir**. O aplicativo também grava um monitor em MP4 para reprodução offline.

## Experimentar no Windows

Baixe `ksiezyclive-windows-x64.zip` na [página de versões](https://github.com/PedroAyan/ksiezxclive/releases), extraia e abra `ksiezyclive.exe`. A distribuição é portátil. O alvo do projeto é Windows 10 22H2 x64, com Direct3D 11, componentes de mídia do Windows e [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/).

**Para gravar**, escolha **Gravar**, selecione o monitor, a fonte de áudio e a pasta de saída. Ao parar, aguarde a confirmação de que o MP4 foi salvo. Esse modo funciona offline.

**Para transmitir**, configure os serviços descritos abaixo. Em **Live**, escolha **Tela inteira** ou **Janela de aplicativo**. Restaure a janela se estiver minimizada e mantenha seu tamanho durante a transmissão. Em jogos, selecione a janela da partida. Ao aparecer **Ao vivo**, use **Copiar link** para compartilhar o convite.

A live oferece 480p, 720p e 1080p a 30 ou 60 fps. O padrão é 720p a 30 fps. Em **Áudio**, escolha **Sem áudio**, um aplicativo específico ou **Som do PC, exceto Discord**. Esta última opção exige uma única instância do Discord desktop. Vozes do Discord Web podem entrar pelo áudio do navegador.

## Como funciona

| Componente | Responsabilidade |
| --- | --- |
| Desktop · Tauri, React e TypeScript | Seleção da fonte, qualidade e controle da sessão |
| Captura · Rust | Windows Graphics Capture e Direct3D 11 para vídeo, WASAPI para áudio e Media Foundation para o MP4 |
| API · Fastify e TypeScript | Autenticação do anfitrião, criação da sala e emissão dos convites |
| Espectador · React e LiveKit | Reprodução das faixas WebRTC no navegador com permissão de espectador |

## Compilar a partir do código

No Windows x64, instale Node.js 22.18 ou posterior, Rust com a ferramenta MSVC, Visual Studio Build Tools com desenvolvimento desktop em C++ e o Windows SDK. Execute na raiz do repositório. O SDK Rust do LiveKit baixa a biblioteca WebRTC durante a compilação.

```powershell
npm ci
rustup target add x86_64-pc-windows-msvc
npm run desktop build -- --no-bundle --target x86_64-pc-windows-msvc
```

O executável fica em `src-tauri\target\x86_64-pc-windows-msvc\release\ksiezyclive.exe`. Para desenvolver a interface, execute `npm run desktop dev`.

## Configurar uma live local

1. Baixe o [LiveKit Server para Windows](https://github.com/livekit/livekit/releases). Coloque o executável na raiz do projeto e inicie o serviço em um terminal com `.\livekit-server.exe --config livekit.local.yaml`.
2. Em uma cópia nova do projeto, configure a API com os comandos abaixo no PowerShell 7. O arquivo `.env` guarda o código privado que identifica o anfitrião.

```powershell
Copy-Item .env.example .env
$codigo = [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
(Get-Content .env) -replace '^KSIEZYC_HOST_TOKEN=$', "KSIEZYC_HOST_TOKEN=$codigo" |
  Set-Content -Encoding utf8 .env
npm ci
npm run build
npm run servidor
```

3. No desktop, marque **Usar servidor personalizado**. Informe `http://127.0.0.1:3001` e o valor de `KSIEZYC_HOST_TOKEN`. Para reutilizar a configuração entre execuções, copie `ksiezyclive.example.json` para `ksiezyclive.json` ao lado do executável e preencha os campos.
4. Inicie a live e abra o convite completo no navegador do mesmo PC. O código no final do link muda a cada sessão.

## Usar um domínio próprio

Sirva a página e a API por HTTPS. Com Cloudflare Tunnel no mesmo PC da API, aponte o hostname público para `http://127.0.0.1:3001`. A mídia passa pelo LiveKit, que precisa de um endereço WSS acessível aos espectadores e de conectividade WebRTC. Pode-se usar LiveKit Cloud ou uma instância própria com as portas de mídia e TURN configurados. [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/) · [Rede do LiveKit](https://docs.livekit.io/transport/self-hosting/ports-firewall/)

No `.env`, defina `PUBLIC_BASE_URL` com a origem HTTPS do convite, `LIVEKIT_SERVER_URL` com a origem usada pela API para administrar as salas e `LIVEKIT_PUBLIC_URL` com o endpoint WSS. Configure `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET` e um `KSIEZYC_HOST_TOKEN` próprios. As chaves de exemplo destinam-se ao teste local. Em instâncias próprias, mantenha `room.auto_create: false`. Reinicie a API e informe o endereço HTTPS no desktop.

## Estado da validação

O executável Windows x64 foi compilado. O seletor foi conferido com Chrome e Steam, incluindo janelas minimizadas. A criação de sala, a abertura da página do espectador e a invalidação do convite foram verificadas localmente. Ainda faltam testes em um segundo PC, com um espectador em outra rede e com áudio simultâneo do jogo e do Discord para confirmar o isolamento de ponta a ponta.
