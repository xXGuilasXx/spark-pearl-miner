# Convivência com um servidor de LLM

Um DGX Spark normalmente já tem um trabalho: servir um modelo. O spark-pearl-miner foi feito para minerar só enquanto esse trabalho está ocioso e sair da frente rápido. Código: `crates/spm-coexist`. Números: `docs/_data/facts.toml` (`[coexist]`). Inglês: [`docs/en/COEXISTENCE.md`](../en/COEXISTENCE.md).

## 1. Por que o minerador tem que ceder

- **Time-slicing.** Dois contextos CUDA na mesma GPU dividem o tempo. Com os dois ocupados cada um fica com mais ou menos metade da GPU, então minerar ao lado de um vLLM ocupado custa a ele cerca de **50 %** da vazão e aumenta a latência de cada requisição, e o minerador perde o mesmo.
- **Memória unificada.** O GB10 divide um único pool LPDDR5X entre CPU e GPU. Sob pressão de memória a máquina não faz um OOM limpo, ela trava. Na máquina do autor a orquestração também mata o vLLM quando o `MemAvailable` cai abaixo de 12 GiB.
- **Energia.** Somar a nossa carga à do vLLM pode levar a GPU para a faixa de desligamento ([ENERGIA-TERMICA](ENERGIA-TERMICA.md)). Os cortes do governor contam também a potência de outros processos.

## 2. Modos

| Modo | Quem inicia e para o worker | Enquanto o servidor de LLM está ocupado | Para quê |
|---|---|---|---|
| `spark-modo` | o runtime `miner` do `spark-modo` | o worker não está rodando (o runtime foi trocado) | máquinas geridas pelo `spark-modo` (a do autor) |
| `yield` | o daemon | worker **pausado**, contexto CUDA e memória mantidos | retomada rápida, memória sobrando |
| `yield-release` | o daemon | worker **liberado**: o processo sai, contexto e memória liberados; um novo sobe quando fica ocioso | memória apertada, ou o servidor precisa da GPU inteira |
| `exclusive` (padrão) | o daemon | ignorado: o minerador continua | máquinas dedicadas; uma máquina com servidor de LLM deve escolher um dos modos acima |

O modo é o `coexistence.mode` do `config.toml` ([CONFIGURACAO](CONFIGURACAO.md)); uma instalação nova começa em `exclusive`, o assistente de configuração oferece os outros, e a troca vale na hora. A guarda de memória (seção 5) e o governor de energia valem em todos os modos.

O que o daemon faz aparece na API de status, `status.coexist`: o modo, a decisão (`mine`, `pause`, `release`, `external`), quem controla o worker (`spark-modo`), o último sinal (`vllm` com as contagens de requisições rodando e esperando, `nvml` ou `nvidia-smi` com a utilização de SM dos outros processos, ou `unavailable` com o erro das métricas), o tempo ocioso até agora e o necessário, o número de transições, a guarda de memória (estado, `MemAvailable`, PSI) e o protocolo de pausa (último ACK, latências de pausa e retomada, escaladas). Cada transição vai para o log, para a linha do tempo e sai como evento SSE `coexist`; enquanto a decisão ou a guarda seguram o worker, o `pause_reason` é `yield` ou `memory`.

## 3. O modelo de runtimes do spark-modo

No Spark do autor, o `spark-modo` entrega um lease exclusivo da GPU a um *runtime* por vez (o servidor vLLM, um modo de treino e assim por diante). O minerador é mais um runtime, o `miner`:

- O worker só roda como esse runtime, iniciado pela unit de sistema que o `contrib/spark-modo/` instala. Nenhum processo CUDA fica residente fora dele.
- Quando uma requisição precisa de um modelo, o `spark-modo` tira o minerador: o worker para, o contexto é liberado e o modelo carrega. O worker v0 para em ~10 ms.
- O modo de treino nunca inicia o minerador.
- Neste modo o daemon não controla nada e nunca sobe o worker, seja qual for o `worker.launch` (ele vale como `external`). Ele mantém pools, taxa e estatísticas, espera o runtime `miner` no `worker.sock` e informa "controlado pelo spark-modo" (`coexist.controlled_by`, decisão `external`). A guarda de memória e o governor de energia continuam valendo para um worker conectado.

Os arquivos de integração ficam em `contrib/spark-modo/` (M9); o dono revisa e instala com sudo.

## 4. O sinal do vLLM (`yield`, `yield-release`)

O vLLM expõe métricas Prometheus sem autenticação em `GET http://127.0.0.1:8001/metrics` (cerca de 58 KB; uma leitura leva ~6 ms nesta máquina). O daemon lê dois gauges e soma todas as séries (engine, modelo):

```text
vllm:num_requests_running{engine="0",model_name="..."} 0.0
vllm:num_requests_waiting{engine="0",model_name="..."} 0.0
```

Os nomes têm que bater exatamente (`vllm:num_requests_waiting_by_reason` é outra métrica). O cliente é um `GET` HTTP/1.1 mínimo sobre tokio: só `http://`, `Connection: close`, corpo com `Content-Length`, chunked ou até o fechamento, limite de 4 MiB e timeout de 250 ms.

Regras:

- Consulta a cada **200 ms** (`coexistence.poll_ms`, entre 100 e 250 ms) em `coexistence.metrics_url`, enquanto a mineração está iniciada.
- Qualquer requisição rodando ou esperando → pausa (ou libera) **na hora**.
- Volta a minerar só depois de **5 s** (`coexistence.idle_s`) seguidos com `running == 0 && waiting == 0`. Os 5 s recomeçam na última consulta ocupada.
- O daemon começa sem minerar e também precisa desses 5 s de silêncio (depois de Iniciar, e de novo depois de Parar e Iniciar).
- O `yield-release` no modo spawn sobe um worker novo quando a decisão libera de novo; com `worker.launch = external` quem sobe é o lançador.

**Plano B.** Quando as métricas não estão disponíveis (conexão recusada, timeout, não é um vLLM), o daemon usa a utilização de SM por processo do NVML (ou o `nvidia-smi pmon` quando o NVML não carrega), lida pela thread de amostragem do governor de energia: a maior leitura de cada processo nos últimos 3 s, somada sobre os *outros* processos de computação (o nosso worker fica de fora pelo pid, tanto o filho que subimos quanto o par do `worker.sock`; processos só gráficos como o compositor são ignorados). A partir de 10 % (`coexistence.busy_sm_pct`) conta como ocupado. Se nenhuma das fontes responder, a GPU conta como ocupada: o minerador nunca minera às cegas ao lado de um servidor de LLM.

**Custo em latência.** Uma requisição que chega enquanto mineramos espera no máximo uma consulta (200 ms) + a leitura (~6 ms) + a pausa (≤ 10 ms no v0) até o vLLM ter a GPU só para ele, e nesse meio tempo roda dividindo a GPU conosco, não bloqueada.

## 5. Guarda de memória

Lida de `/proc/meminfo` (`MemAvailable`) e `/proc/pressure/memory` (`some avg10`):

- **Iniciar** só se `MemAvailable − orçamento do worker ≥ 20 GiB`. O orçamento do worker é fixo, ≤ 2 GiB. Um início com `some avg10 > 10 %` também é recusado (o worker teria que sair na hora).
- **Sair** (liberar o worker) se `MemAvailable < 16 GiB` ou `some avg10 > 10 %`.
- A diferença entre 20 e 16 GiB é a histerese: depois de uma saída por memória o worker só volta quando a condição de início valer de novo.
- Um kernel sem PSI só perde a regra de pressão; um `MemAvailable` ilegível recusa o início.

O daemon confere a cada segundo e de novo logo antes de subir qualquer worker; uma recusa ou uma saída gera um alerta, e o worker fica segurado (`pause_reason` `memory`, `coexist.memory.state` `refused`, `exit_low_memory` ou `exit_pressure`) até a condição de início valer.

Esses limites ficam bem acima dos 12 GiB em que a orquestração do dono mata o vLLM.

## 6. Protocolo de pausa e retomada

Para controladores fora do daemon (scripts do spark-modo, um shell) o worker fala por sinais POSIX:

- **SIGUSR1 = pausa.** O worker deixa terminar o pedaço de GEMM em andamento (um ponto quiescente, ≤ 10 ms no v0), para de enviar trabalho à GPU, mantém contexto e memória e confirma. Sem nada em andamento ele está quiescente na hora.
- **SIGUSR2 = retoma.** Imediato, confirmado.
- **ACK:** uma linha `paused <seq>` ou `running <seq>`, com `seq` crescendo a cada ACK (recomeçando de 1 num processo novo do worker), gravada de forma atômica em `$XDG_RUNTIME_DIR/spark-pearl-miner/worker.ack`, onde o daemon também a lê. Repetir um comando reenvia o ACK, então um controlador que perdeu um ACK só precisa pedir de novo.
- Sinais se acumulam: o worker guarda só o último recebido entre duas voltas do seu loop, então vale o último comando.
- **Escalada:** sem ACK de pausa em 100 ms → SIGTERM (o worker sai no próximo ponto quiescente e o contexto é liberado); ainda vivo 3 s depois → SIGKILL. Uma retomada sem ACK em 1 s é reenviada até 3 vezes e depois reportada.

Os dois lados são máquinas de estado puras em `spm_coexist::handshake`, testadas sem processos.

**O daemon como controlador.** O daemon comanda o próprio worker pelo IPC em vez de sinais: os quadros `Pause`/`Resume` fazem o papel de SIGUSR1/SIGUSR2, e o worker confirma do mesmo jeito, no `worker.ack` (o IPC v1 não tem quadro de ACK, então o arquivo é o canal). O supervisor do worker lê o arquivo a cada 10 ms enquanto há um pedido pendente. Uma pausa sem ACK em 100 ms vira um `Release` pelo IPC (o passo do SIGTERM); um worker ainda presente 3 s depois é morto (modo spawn) ou desconectado (externo), o que conta como falha do worker. Retomadas são reenviadas até 3 vezes. O worker simulado implementa o lado do worker tanto pelo IPC quanto por SIGUSR1/SIGUSR2.

## 7. Situação

Pronto e com testes: o parser Prometheus (sobre uma exposição construída no formato do vLLM), o cliente HTTP (contra servidores locais: `Content-Length`, chunked, até o fechamento, leituras quebradas, erros, timeout), a decisão de ceder, o plano B por utilização de SM (sobre saída de `nvidia-smi pmon` desta máquina), a guarda de memória (sobre fixtures de `/proc`) e os dois lados do protocolo. `cargo run --release -p spm-coexist --example vllm_load` consulta o endpoint real e mostra o que a decisão faria.

Ligado ao daemon e testado sem GPU (`crates/spm/tests/coexist.rs`): `yield` contra um servidor de métricas falso (ocupado → worker pausado bem abaixo de 300 ms, retomado depois de `idle_s`, sem sinal nenhum → pausado), `yield-release` (worker liberado, um novo sobe quando fica ocioso), a guarda de memória recusando um início sobre uma fixture e liberando sob pressão, o `spark-modo` nunca subindo o worker e uma pausa sem ACK escalada para liberação.

Ainda em aberto (TODO M11): yield e yield-release sob carga real do vLLM, a pausa de ≤ 10 ms medida com o worker real, o `spark-recurso` subindo o vLLM depois de uma liberação e a guarda de memória recusando um início na máquina.
