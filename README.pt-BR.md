# Spark Pearl Miner

**Ganhe Pearl (PRL) com o seu NVIDIA DGX Spark no tempo em que ele ficaria parado, com baixo consumo.**
Código aberto (Apache-2.0), feito só para o DGX Spark (GB10). Não oficial: sem afiliação,
patrocínio ou endosso da NVIDIA ou da Pearl Research Labs.
_English: [README.md](README.md)_

> **Status:** minerando em pools da mainnet (Kryptex, HeroMiners BR, LuckyPool BR) com shares
> aceitas · pré-lançamento **0.1.0-alpha.1**.

## Por quê

Um DGX Spark passa a maior parte do tempo esperando o próximo trabalho. Este minerador põe essa GPU
ociosa para minerar Pearl (PRL), uma moeda de prova de trabalho útil cujo "hash" é uma multiplicação
de matrizes INT8, exatamente o que os tensor cores do GB10 fazem bem.

No DGX Spark, com o limite de clock padrão de 2000 MHz, ele sustenta **73,9 T-MAC/s** de trabalho
creditado com **cerca de 63 W na GPU** e a GPU a **72 °C**: bem abaixo dos ~88–92 W em que o Spark
sabidamente desliga. A taxa creditada anda em degraus porque a pool credita tentativas inteiras
(7,04e13 MACs cada).

Quanto isso rende depende da dificuldade da rede e do preço do PRL, que mudam rápido. No **retrato de
2026-09-26** da [VIABILIDADE](docs/pt-BR/VIABILIDADE.md) (dificuldade 29,4 M, 0,0241 PRL por TH/s
por dia, PRL a US$1,30), 73,9 T-MAC/s dão cerca de **1,8 PRL/dia brutos**, antes da taxa de 2 % do
desenvolvedor e da taxa da própria pool. A dificuldade subiu 36 % nos 30 dias anteriores a esse
retrato, então espere menos com o tempo. O site da sua pool mostra o que você ganha de verdade. Isto
não é aconselhamento financeiro.

![O painel (valores simulados)](docs/images/pt-BR/dashboard-mining.png)
_O painel. Valores simulados: as imagens do manual vêm de um minerador simulado._

## Instalação

No DGX Spark, num terminal, com o seu usuário normal (não root):

**1. Instale.** Um comando:

```bash
curl -fsSL https://raw.githubusercontent.com/xXGuilasXx/spark-pearl-miner/main/packaging/install.sh | bash
```

O instalador verifica a máquina (aarch64, GB10, driver NVIDIA ≥ 580, runtime CUDA 13), baixa o
release mais novo e o confere com o `SHA256SUMS` dele (até o primeiro release ser publicado ele
compila do código-fonte, 5–15 minutos), instala `~/.local/bin/spark-pearl-miner`, um serviço
systemd do usuário e um atalho no menu de aplicativos, e inicia o serviço. Ele nunca executa `sudo`,
nunca mexe nas suas configurações, e uma instalação nova ainda não minera.

**2. Abra a GUI.** O instalador abre; senão use **Spark Pearl Miner** no menu de aplicativos, execute
`spark-pearl-miner gui` ou abra **http://127.0.0.1:4078/**.

**3. Três respostas.** Escolha o idioma, cole o endereço da sua carteira Pearl (`prl1p…`), aceite a
taxa de 2 % do desenvolvedor e clique em **Começar a minerar**.

![Passo 2 da configuração: sua carteira](docs/images/pt-BR/wizard-2-wallet.png)

**4. Opcional, recomendado: o limite de clock da GPU.** Um comando `sudo` limita o clock do SM a
2000 MHz em todo boot (a rede de segurança que mantém a GPU em cerca de 63 W; reversível):

```bash
sudo ~/.local/share/spark-pearl-miner/install-clockcap.sh --apply
```

Para continuar minerando depois de você sair da sessão e começar no boot antes de alguém entrar, o
instalador também imprime `sudo loginctl enable-linger $USER` quando é necessário.

- **Ele continua rodando.** O serviço sobe com a sua sessão (ou no boot, com lingering). Ele só
  minera depois da configuração e de **Começar**; **Parar** é lembrado entre reinícios.
- **Atualizar:** `~/.local/share/spark-pearl-miner/install.sh --upgrade` (as configurações são
  mantidas; se a versão nova recusar as suas configurações, a antiga é recolocada). Voltar:
  `~/.local/share/spark-pearl-miner/install.sh --rollback`.
- **Desinstalar:** `~/.local/share/spark-pearl-miner/install.sh --uninstall` (acrescente `--purge`
  para apagar também as configurações, a carteira e o token).
- **Do código-fonte:** `git clone --recurse-submodules https://github.com/xXGuilasXx/spark-pearl-miner && cd spark-pearl-miner && ./packaging/install.sh --from-source`.
- **Sem tela / remoto:** `ssh -L 4078:127.0.0.1:4078 voce@seu-spark` e abra http://127.0.0.1:4078/
  no seu computador.

Todas as opções do instalador: `packaging/install.sh --help`.

## O que você vai ver

Uma página responde três perguntas: **está funcionando** (uma frase com um ponto colorido e um botão
Começar/Parar), **quanto** (a taxa creditada e as shares aceitas/rejeitadas) e **está seguro**
(potência da GPU, temperaturas da GPU e da placa, clock do SM e o limite de clock). Uma linha abaixo
mostra qual pool está em uso e se as reservas estão prontas. O ícone de engrenagem guarda as únicas
quatro configurações que você pode querer mudar: carteira, nome do worker, idioma e as três pools.
`spark-pearl-miner status` faz o mesmo num terminal.

O **[manual ilustrado](docs/pt-BR/MANUAL.md)** explica cada botão, mensagem e configuração, com uma
imagem de cada tela.

## Taxa do desenvolvedor (divulgada)
`dev fee 2.00% → prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n @ br.pearl.herominers.com:1200 (HeroMiners), worker "devfee", fatias de 120 s, só enquanto minera`
Todas as constantes da taxa vivem em um único arquivo, `crates/spm-fee/src/lib.rs`; o CI falha se este README divergir dele. **A carteira da taxa não é configurável**: não há flag, variável de ambiente, chave de config nem API que a altere, e a GUI a mostra somente para leitura. Os tarballs de release vêm com um arquivo SHA256SUMS; builds reprodutíveis e atestados estão planejados, mas ainda não existem. Sem configuração remota, sem ofuscação, sem binários empacotados. A taxa se desliga sozinha quando a sua carteira é a carteira da taxa.

## Doações
Se este projeto for útil para você, doações em PRL são bem-vindas no mesmo endereço:

`prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n`

## Pools
O minerador vem com três pools, nesta ordem, e failover automático entre elas: **Kryptex**
(`prl-br.kryptex.network:8048`, TLS), **HeroMiners BR** (`br.pearl.herominers.com:1200`) e
**LuckyPool BR** (`pearl-br.luckypool.io:3360`, certificado fixado). As três tiveram shares aceitas
e 0 rejeitadas em testes ao vivo. Se a pool em uso falhar, o minerador passa para a próxima em cerca
de 1 s; ela verifica a pool preferida de novo a cada 5 minutos e volta para ela depois que ela ficar
estável por 60 s. O painel mostra isso numa linha de status. Troque as pools pelo ícone de engrenagem (veja `docs/pt-BR/CONFIGURACAO.md` para
todas as opções de pool). Se ainda precisa escolher uma: eu já minerei PRL na **Kryptex** e nunca tive problema com os pagamentos dela. Cadastrar-se pelo meu link de referência não custa nada e ajuda este projeto:

https://pool.kryptex.com/?ref=b2cfe3e2 (link de referência)

## Segurança

- **Energia.** O GB10 não tem limite de potência por software e algumas unidades desligam
  abruptamente por volta de 88–92 W na GPU. O perfil padrão **Equilibrado** mira 75 W e para em
  85 W, com o limite de clock de 2000 MHz (medido: cerca de 63 W). Um governador lê a GPU 10 vezes
  por segundo e pausa a mineração quando a GPU passa de 83 °C, a placa passa de 95 °C ou a potência
  fica acima da parada por 3 leituras (pausa de 60 s). Sem leituras de energia, não minera. O perfil
  Máximo (2200 MHz) mediu 83–87 W e a placa a 97,5 °C, então ele só existe no arquivo de
  configurações atrás de uma confirmação explícita e não é recomendado. Detalhes:
  [ENERGIA-TERMICA](docs/pt-BR/ENERGIA-TERMICA.md).
- **Memória.** O worker da GPU usa no máximo 2 GiB. Ele só começa com 22 GiB livres e libera a GPU
  na hora abaixo de 16 GiB ou sob pressão de memória, então os seus outros programas vêm primeiro.
- **Shares.** Toda share é verificada no Spark com o código de referência oficial da Pearl antes de
  ser enviada.
- **Acesso.** A GUI só escuta em 127.0.0.1. O seu próprio usuário não precisa de senha; outras
  contas e usuários remotos precisam do token (`spark-pearl-miner gui --print-url`); expor na rede
  local é recusado.

## Arquivo de configurações

Tudo o que não está na GUI fica num arquivo só, `~/.config/spark-pearl-miner/config.toml`, já
ajustado para o Spark e comentado. Edite só se souber por quê: o minerador aplica edições válidas em
segundos, ignora as inválidas (o painel mostra um alerta), e `spark-pearl-miner config check` diz o
que está errado. Todas as chaves: [CONFIGURACAO](docs/pt-BR/CONFIGURACAO.md).

## Perguntas frequentes

**Ele deixa o meu trabalho de IA mais lento?** Por padrão a GPU é do minerador enquanto ele minera:
clique em **Parar** (a GPU é liberada na hora) antes de rodar um modelo e em **Começar** depois. Para
ele sair da frente sozinho enquanto o vLLM estiver ocupado, coloque `coexistence.mode = "yield"` no
arquivo de configurações ([COEXISTENCIA](docs/pt-BR/COEXISTENCIA.md)). Em qualquer modo ele libera a
GPU quando a memória fica curta.

**E se a pool cair?** Ele passa para a próxima pool em cerca de 1 s e volta quando a primeira ficar
saudável por 60 s (ela é verificada a cada 5 minutos). Se as três caírem, ele continua tentando e o painel avisa.

**Como paro, atualizo ou desinstalo?** Parar: o botão no painel, ou `spark-pearl-miner stop`.
Atualizar, voltar versão e desinstalar: veja [Instalação](#instalação).

**Onde fica a configuração? E os logs?** `~/.config/spark-pearl-miner/config.toml`
(`spark-pearl-miner config path`). Logs: `journalctl --user -u spark-pearl-miner -f`, ou **Exportar
diagnóstico** no rodapé do painel.

**Preciso de root?** Só para os dois passos opcionais: o limite de clock e o lingering.

**Por que a taxa anda em degraus?** A pool credita tentativas inteiras de 7,04e13 MACs.

**Posso mudar a taxa ou a carteira da taxa?** Não. Ela é compilada no programa e mostrada só para
leitura.

**Como pauso ou fixo uma pool?** Pela linha de comando ou pela API; veja o
[manual](docs/pt-BR/MANUAL.md#cli).

**Isto é aconselhamento financeiro?** Não. Renda de mineração é tributável em muitos países (no
Brasil, IN RFB 2291/2025).

## Requisitos

Um NVIDIA DGX Spark (GB10) com DGX OS 7.x (driver ≥ 580 e runtime CUDA 13, ambos pré-instalados) e
um endereço de carteira Pearl (`prl1p…`) que você controla. O Rust só é necessário para compilar do
código-fonte (fixado em 1.98.1, instalado sozinho pelo instalador).

## Documentação

Para usuários: [Manual (ilustrado)](docs/pt-BR/MANUAL.md) · [Configuração](docs/pt-BR/CONFIGURACAO.md) ·
[Energia e temperatura](docs/pt-BR/ENERGIA-TERMICA.md) · [Coexistência com um servidor de LLM](docs/pt-BR/COEXISTENCIA.md) ·
[Viabilidade](docs/pt-BR/VIABILIDADE.md)

## Desenvolvimento

[Arquitetura](docs/pt-BR/ARQUITETURA.md) · [Contrato do kernel (EN)](docs/en/KERNEL.md) ·
[Benchmarks](docs/pt-BR/BENCHMARKS.md) · [Protocolos das pools (EN)](docs/protocol/) ·
[Decisões](docs/pt-BR/DECISOES.md) · [Dual mining](docs/pt-BR/DUAL-MINING.md) ·
[Referência da API e da segurança](docs/pt-BR/GUI.md) · [TODO](TODO.pt-BR.md) · [Contribuir (EN)](CONTRIBUTING.md)

As capturas de tela são geradas de novo com `tools/screenshots.sh`; o instalador é testado com
`tools/test-install.sh`, e os tarballs de release são gerados com `packaging/make-release.sh`.

## Licença
Apache-2.0 — veja [LICENSE](LICENSE) e [NOTICE](NOTICE) (ISC: Pearl Research Labs e The Decred developers; BSD-3: NVIDIA CUTLASS).
