# GUI, API local e linha de comando

_English: [../en/GUI.md](../en/GUI.md)_

O daemon serve uma GUI web pequena e uma API REST + SSE em **`http://127.0.0.1:4078`**, só no
loopback. A GUI é HTML, CSS e módulos ES embutidos no binário (sem etapa de build, sem CDN,
menos de 200 KB), em inglês e português do Brasil.

## Abrindo a GUI

```
spark-pearl-miner gui              # abre o navegador já logado
spark-pearl-miner gui --print-url  # só imprime a URL de login (para SSH)
```

No próprio Spark, logado com a conta que roda o daemon, basta abrir **`http://127.0.0.1:4078/`**:
sem token, sem tela de login (veja [Acesso local](#acesso-local-sem-token)).

O `gui` sobe o serviço do usuário se ele não estiver rodando. A URL leva o token da API no
fragmento (`#token=…`), que o navegador nunca manda ao servidor; a página troca o token por uma
sessão uma única vez e o apaga da barra de endereço. Também dá para colar o token de
`~/.config/spark-pearl-miner/api-token` no formulário de login.

**De outro computador**, encaminhe a mesma porta por SSH e abra lá a URL impressa:

```
ssh -L 4078:127.0.0.1:4078 usuario@spark
spark-pearl-miner gui --print-url   # no Spark
```

A porta precisa ser 4078 nas duas pontas: o daemon confere o cabeçalho `Host` (veja abaixo).

O atalho `packaging/spark-pearl-miner.desktop` roda `spark-pearl-miner gui`:

```
install -Dm0644 packaging/spark-pearl-miner.desktop ~/.local/share/applications/spark-pearl-miner.desktop
```

## Segurança da API local

| Medida | O que faz |
|---|---|
| Só loopback | O listener usa `127.0.0.1` (ou `::1`). `api.lan = true` é recusado: acesso pela rede local exige TLS, que esta versão não implementa. Use encaminhamento por SSH. |
| Arquivo de token | `~/.config/spark-pearl-miner/api-token`, 256 bits aleatórios, modo 0600, criado na primeira execução. Quem consegue lê-lo controla o minerador. |
| Cookie de sessão | `POST /api/v1/session {"token": …}` devolve um cookie `HttpOnly; SameSite=Strict; Path=/` (outro valor aleatório de 256 bits, guardado só na memória: reiniciar desloga todo mundo) e um valor CSRF. Tokens errados recebem resposta cada vez mais lenta. |
| CSRF | Todo `POST`/`PUT`/`DELETE` em `/api/` precisa levar o valor da sessão em `X-SPM-CSRF`; sem ele, **403**. |
| Host / Origin | `Host` precisa ser `127.0.0.1:4078`, `localhost:4078` ou `[::1]:4078`, e um `Origin`, se houver, a mesma origem; qualquer outra coisa é **403** (impede DNS rebinding). |
| Usuário local | Com `api.trust_local_user = true` (o padrão), uma conexão pelo loopback cujo socket pertence ao mesmo UID do daemon não precisa de token: `GET /api/v1/session` abre uma sessão sozinho e os outros `GET` respondem sem cookie. Mudanças continuam exigindo o cookie e o `X-SPM-CSRF`. Veja [Acesso local](#acesso-local-sem-token). |
| Sem sessão | Qualquer outra chamada `/api/*` sem cookie válido é **401**. |
| Cabeçalhos | `Content-Security-Policy: default-src 'self'; frame-ancestors 'none'`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store` na API. |
| Só texto | Textos vindos das pools (erros, ids de job) são mostrados com `textContent`; a GUI nunca usa `innerHTML`. |
| Auditoria | Toda mudança de configuração vai para `~/.local/state/spark-pearl-miner/audit.log` com a origem (`api`, `file`, `cli`); trocar a carteira de pagamento mostra um aviso até você confirmar. |

A taxa do desenvolvedor é somente leitura em todo lugar: `/api/v1/fee` não tem método de escrita,
e o `PUT /api/v1/config` recusa qualquer chave que pareça de taxa (`fee`, `dev…`, `donation…`) com
`422 fee_not_configurable`.

### Acesso local sem token

Na máquina que roda o daemon, a mesma conta de usuário entra sem o token: abrir
`http://127.0.0.1:4078/` leva direto ao painel. Para cada conexão vinda de `127.0.0.0/8` ou `::1`
sem cookie de sessão, o daemon procura o socket do cliente em `/proc/net/tcp` (ou `/proc/net/tcp6`)
e compara o UID dono dele com o seu. Só quando bate:

* `GET /api/v1/session` abre uma sessão exatamente como o login com token (cookie
  `HttpOnly; SameSite=Strict` mais um valor CSRF), então a GUI começa sem tela de login;
* os outros `GET` respondem sem cookie (prático para `curl` na própria máquina);
* `POST`/`PUT`/`DELETE` continuam exigindo o cookie de sessão **e** o `X-SPM-CSRF`. Uma página
  maliciosa aberta no seu próprio navegador também roda com o seu UID, então a checagem de UID só
  libera leitura.

É recusado quando o navegador diz que o pedido vem de outra página (`Sec-Fetch-Site` diferente de
`same-origin` ou `none`), quando o pedido traz `Forwarded`, `X-Forwarded-For` ou `X-Real-IP` (um
proxy), e sempre depois da lista de Host/Origin acima.

**Outras contas** na mesma máquina continuam precisando do token: o Spark é multiusuário e o
`127.0.0.1` é compartilhado por todas as contas, enquanto o arquivo do token só você consegue ler.
Acesso pela rede (rede local ou Tailscale, no futuro) também precisa do token. Um detalhe: um túnel
ou proxy que *você* roda com a sua própria conta (por exemplo `ssh -L` logado como você, ou um
`socat` que você iniciou) conecta com o seu UID, então quem puder usá-lo entra sem o token. No caso
do `ssh -L` isso não passa do que o seu login SSH já dá; não exponha um proxy desses a outras
pessoas.

Para exigir o token em todo lugar, coloque `trust_local_user = false` em `[api]` no
`~/.config/spark-pearl-miner/config.toml` e reinicie o daemon.

## Telas

**Assistente de configuração** (na primeira vez, ou *Configuração* no menu): idioma → carteira
(conferida na hora como endereço bech32m `prl1p…`) → nome do worker (`[A-Za-z0-9_-]{1,32}`) →
pools → aviso da taxa e aceite → perfil de energia (o Máximo exige digitar uma confirmação) → modo
de compartilhar a GPU → resumo → **Salvar e começar a minerar**.

O editor de pools (assistente e *Pools*) tem três posições em ordem de prioridade, cada uma com
uma predefinição (HeroMiners BR/US/US2/DE/FR, LuckyPool BR com a chave fixada, LuckyPool EU,
Kryptex 8048 TLS, ou personalizada), host, porta, TLS (`auto`, `on`, `off`, `pinned`) e uma parte
*Avançado* (rótulo, dialeto, membro `jsonrpc`, codificação da prova, senha, padrão do bloco de
hash). Dá para reordenar, adicionar (até três) e remover. **Testar conexão** confere só DNS, TCP e
TLS; marcando *testar também o login* ele pede confirmação e então faz um login com a sua carteira
e espera um trabalho (nada é enviado).

| Tela | Conteúdo |
|---|---|
| Painel | estado, pool ativa, hashrate creditado (10 s), shares aceitas/rejeitadas/obsoletas/descartadas, estado do worker de GPU, para quem a GPU trabalha (sua pool, fatia da taxa, ociosa), tempo ligado, carteira, alertas; Iniciar / Pausar / Retomar / Parar |
| Pools | um chip de estado por posição (ATIVA, em espera, aguardando para tentar de novo, login recusado…), o último erro em linguagem simples com o texto cru da pool, contadores, TLS e campo de prova aprendidos, **Trocar agora** / **Fixar** / **Soltar**, a linha do tempo do failover e o editor de pools com **Salvar e aplicar** |
| Failover | todos os limites do gerenciador de failover com o padrão; *Restaurar padrões* |
| Desempenho e energia | perfil de energia, as instruções do limite de clock (`spark-pearl-miner install-clock-cap`), modo de compartilhar a GPU, modo de início do worker, simulação na CPU, telemetria da GPU pelo `nvidia-smi` |
| Taxa | a linha da taxa, todas as constantes compiladas (somente leitura), o hash das constantes e o que foi medido: taxa até agora, últimas 24 h, tempo minerado para o desenvolvedor, dívida, próxima fatia, shares da taxa |
| Logs | log ao vivo (SSE), filtro de nível, **Exportar diagnóstico** (logs, estado, pools e configurações em JSON com os endereços de carteira ocultos) |
| Sobre | versão, commit, SHA-256 do binário em execução, hash das constantes da taxa, licenças, como verificar uma versão, a declaração de não afiliação |

## A demonstração de failover

Em qualquer máquina, sem pool nem GPU:

```
cargo run --release -p spark-pearl-miner --example failover_demo -- --port 4078
```

Ela sobe duas pools de teste no localhost (a pool 1 recusa conexões por 30 s), o daemon com o
worker simulado na CPU, e imprime a URL da GUI. A tela Pools mostra a pool 2 ATIVA em segundos,
shares aceitas e a volta para a pool 1 depois do ciclo de sondagem (encurtado na demonstração
para 20 s + 10 s; os padrões são 300 s + 60 s). O mesmo cenário roda no CI como
`crates/spm/tests/failover.rs`.

## Referência da API

Todos os caminhos ficam sob `/api/v1`. As pools são numeradas de **1 a 3** nas URLs.

| Método e caminho | Sessão | CSRF | O quê |
|---|---|---|---|
| `POST /session` `{"token"}` | – | – | login: grava o cookie, devolve `{"csrf"}` |
| `GET /session` | ✓ | – | o valor CSRF de novo (recarregar a página) |
| `DELETE /session` | ✓ | ✓ | sair |
| `GET /status` | ✓ | – | estado, pool, hashrate, shares, worker, fase da taxa, alertas |
| `GET /config`, `PUT /config` | ✓ | PUT | a configuração (veja [CONFIGURACAO.md](CONFIGURACAO.md)); o PUT valida, salva e aplica |
| `GET /pools` | ✓ | – | posições, estado do gerenciador, fixação, sondagem, linha do tempo |
| `POST /pools/test` `{"pool", "confirm"}` | ✓ | ✓ | DNS + TCP + TLS; com `confirm: true` também login + primeiro trabalho |
| `POST /pools/{i}/switch` | ✓ | ✓ | trocar agora (também fixa) |
| `POST /pools/{i}/pin` `{"pinned"}` | ✓ | ✓ | fixar ou soltar |
| `POST /mining/{start,stop,pause,resume}` | ✓ | ✓ | controles; Parar também libera o worker de GPU |
| `POST /wallet/ack` | ✓ | ✓ | dispensar o aviso de carteira alterada |
| `GET /fee` | ✓ | – | constantes da taxa (somente leitura), hash e valores medidos |
| `GET /gpu` | ✓ | – | dispositivo do worker e telemetria do `nvidia-smi` |
| `GET /logs?since=&limit=&redact=1` | ✓ | – | linhas de log recentes (INFO para cima) |
| `GET /about` | ✓ | – | versão, commit, SHA-256 do binário |
| `GET /events` | ✓ | – | Server-Sent Events, abaixo |

Eventos SSE: `stats` (o estado, uma vez por segundo), `share` (aceita/rejeitada, sua ou da taxa),
`fsm` (mudanças de estado do gerenciador e das posições), `timeline` (linhas de log do failover),
`fee` (PreWarm, StartSlice, EndSlice, Abort), `alert`, `log`, `config` (mudança de configuração e
a origem).

## Linha de comando

`spark-pearl-miner` (um link simbólico chamado `spm` funciona igual:
`ln -s ~/.local/bin/spark-pearl-miner ~/.local/bin/spm`).

| Comando | O quê |
|---|---|
| `daemon [--no-api]` | roda o daemon (o serviço do usuário faz isso) |
| `gpu-worker --attach <sock> [--sim]` | o worker de GPU; esta versão só tem a simulação na CPU (`--sim`); o worker CUDA é o marco M5 |
| `status [--json]` | o que o daemon está fazendo (socket de controle) |
| `start`, `stop`, `pause`, `resume` | controles (socket de controle) |
| `gui [--print-url]` | abre a GUI (veja acima) |
| `fee-test [--connect] [--pace-ms N]` | um ciclo da taxa do desenvolvedor pelo agendador real com tempo comprimido: PreWarm → StartSlice → EndSlice e a taxa medida. Sem rede por padrão; `--connect` faz o login de verdade na primeira pool da taxa que responder no PreWarm (nunca envia share) |
| `version`, `--version` | versão, commit, hash das constantes da taxa e a linha da taxa |
| `install-clock-cap` | imprime os comandos `sudo` do limite de clock de 2200 MHz no boot; não muda nada |

A CLI fala com o daemon por `$XDG_RUNTIME_DIR/spark-pearl-miner/control.sock` (0600 num diretório
0700; conexões de outros usuários são recusadas depois de conferir o `SO_PEERCRED`).

## Rodando como serviço

```
install -Dm0755 target/release/spark-pearl-miner ~/.local/bin/spark-pearl-miner
install -Dm0644 packaging/systemd/user/spark-pearl-miner.service ~/.config/systemd/user/spark-pearl-miner.service
systemctl --user daemon-reload
systemctl --user enable --now spark-pearl-miner
sudo loginctl enable-linger "$USER"   # continua rodando sem sessão gráfica
```

A unit reinicia o daemon se ele falhar. **Parar** na GUI faz o worker sair, o que libera o
contexto CUDA; um worker pausado por um minuto é liberado do mesmo jeito. Num DGX Spark com
`spark-modo`, o worker roda só como o runtime `miner`: veja
[`contrib/spark-modo/README.pt-BR.md`](../../contrib/spark-modo/README.pt-BR.md).

## Supervisão do worker de GPU

O daemon escuta em `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.sock` (0600). No modo de início
`spawn` ele mesmo sobe `spark-pearl-miner gpu-worker --attach <sock>`; no modo `external`
(spark-modo) ele espera o worker se conectar. O worker manda um sinal de vida a cada 500 ms; cinco
segundos de silêncio contam como falha. As falhas esperam 5 s, 30 s e depois 2 min, e três falhas
em 10 minutos param a mineração com o alerta "possível falha de hardware" até você apertar
Iniciar. Todo acerto é verificado localmente (pelo worker e de novo pelo daemon) antes de ser
enviado, e só na sessão cujo trabalho o produziu.
