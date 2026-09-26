# TODO — lista piloto (resumo em português; checklist completa e canônica em [TODO.md](TODO.md))

> **P0 (pedido do usuário, literal, 2026-09-26):** _"GUI do minerador para configurar o endereço da carteira e até 3 endereços de pool e portas; para caso o usuário tenha colocado mais de uma e a primeira falhar ele vai automaticamente para a segunda."_ → carteira + até 3 pools (host, porta, TLS) editáveis na GUI; failover automático 1 → 2 → 3 com retorno automático à pool 1.

## Marcos

| Id | Título | Dias | Depende de | Portão |
|---|---|---|---|---|
| BT | Correção do Bluetooth/teclado (prioridade do usuário) | 0,1 | — | ✅ aplicado 2026-09-26 16:31–16:36 UTC |
| M0 | Aprovações, toolchain (rustup ≥ 1.88, gh), esqueleto do repo, zk-pow/pearl-blake3 fixados em `3fe2267`, submódulo CUTLASS, probe IMMA compila | 1 | — | ✅ 2026-09-26 |
| M1 | Núcleo de oráculo: `spm-pow` (padrão 8×16, config52, seeds V3, compact/bound, camadas Merkle + irmãos, PlainProof, verificação) + `spm-cpuref` (pipeline CPU exato) | 1,5 | M0 | ✅ 2026-09-26 (oráculo cpuref bit-exato mesclado) |
| M2 | Ferramentas de captura (`spm-probe.py`, `spm-proxy.py`) + sondagens **só de authorize** (HeroMiners BR H1→H1b→H2→H3; LuckyPool/Kryptex após aprovação) | 1 | M0 | ✅ 2026-09-26 (3 dialetos capturados) |
| M3 | (opcional) share aceita via CPPminer em CPU pela proxy (LuckyPool porta CPU → HeroMiners BR), nice 19, máx. 3 h | 1 | M0, M2 | |
| M4 | Host: `spm-proto` (NDJSON, TLS auto, dialetos object/kryptex + stubs), `spm-work`, `spm-ipc`, `spm-mockpool` com injeção de falhas, teste ponta-a-ponta em CI | 3 | M1, M2 | ✅ 2026-09-26 |
| M5a | Janela de microbenchmark no GB10 (MB0–MB2: pico IMMA a 1800/2000/2200 MHz, QMMA FP8, ldmatrix/cp.async/TMA, L2/DRAM) — avisar antes de parar o vLLM; clock travado | 1 | M0 | ✅ 2026-09-26 (MB1: 108,6 / 96,0 T-MAC/s) |
| M5 | `libspm_cuda` v0 + `gpu-worker` **bit-exato** (128×256×64, cp.async 3 estágios, tile de hash 8×16 em registradores, epílogo BLAKE3, KAT, canário, cancelamento por época) | 6–10 | M1, M5a | **G0** — 🟡 kernel C (TMA+mbarrier) bit-exato em 54 problemas, 87,5 T-MAC/s a 2300 MHz; processo gpu-worker pronto (`crates/spm-worker`: KAT, canário, provas verificadas, cancelamento por época em < 0,3 ms, 85–86 T-MAC/s ponta a ponta no formato padrão); selftest/bench a 2200 MHz pendentes |
| M6 | Primeiras shares aceitas com o nosso minerador (LuckyPool BR → HeroMiners BR/TLS, 1 h: ≥ 20 aceitas, 0 invalid, stale < 1 %) | 1 | M4, M5 | **G2** |
| M7 | Máquina de estados de failover de 3 pools (redutor puro, ≥ 25 cenários, proptest, mockpool, teste ao vivo < 15 s) | 3 | M4 | redutor `spm-pool` + 42 cenários + proptest ✅; mockpool e teste ao vivo pendentes — ✅ 2026-09-26 (50 testes) |
| M8 | Dev fee 2 % transparente (`spm-fee`: constantes únicas, débito 200/9800, fatias 120 s, worker `devfee`, HeroMiners→LuckyPool→Kryptex, auto-off se carteira = dev, banner/log/API/`fee-test`, guarda de CI README×constantes) | 1,5 | M6, M7 | ✅ 2026-09-26 (29 testes; constantes imutáveis) |
| M9 | API + GUI web EN/PT-BR (assistente, dashboard, 3 slots de pool com failover ao vivo, energia, fee, logs, sobre; segurança token→cookie+CSRF), unit `systemd --user`, `.desktop`, `contrib/spark-modo` (runtime `miner`) | 4 | M7 | 🟡 em execução |
| M7 | Máquina de estados de failover de 3 pools (redutor puro, ≥ 25 cenários, proptest, mockpool, teste ao vivo < 15 s) | 3 | M4 | redutor `spm-pool` + 42 cenários + proptest ✅; mockpool e teste ao vivo pendentes |
| M8 | Dev fee 2 % transparente (`spm-fee`: constantes únicas, débito 200/9800, fatias 120 s, worker `devfee`, HeroMiners→LuckyPool→Kryptex, auto-off se carteira = dev, banner/log/API/`fee-test`, guarda de CI README×constantes) | 1,5 | M6, M7 | |
| M9 | API + GUI web EN/PT-BR (assistente, dashboard, 3 slots de pool com failover ao vivo, energia, fee, logs, sobre; segurança token→cookie+CSRF), unit `systemd --user`, `.desktop`, `contrib/spark-modo` (runtime `miner`) | 4 | M7 | daemon, API, GUI, unit/.desktop e contrib prontos ✅ (demo de failover local em teste); aplicar o spark-modo com sudo, teste por `ssh -L`, prints e o contexto CUDA (M5) pendentes |

| M10 | Kernel v1 (persistente 48 CTAs, TMA p/ B, raster L2, A' duplo-buffer; ≥ 85 % do pico IMMA; meta ≥ 76 TH/s a ≤ 85 W) | 5 | M5, M6 | |
| M11 | Governor de energia (Eco/Balanced/Max), unit de clock-cap, coexistência (yield/yield-release), guarda de memória, soaks 60 min + 24 h | 2 | M6 | **G1** — 🟡 código pronto e ligado ao daemon (M9b: governor, marcador, coexistência, guarda de memória, protocolo de pausa); soaks, demo de SIGKILL ao vivo e testes sob carga do vLLM pendentes |
| M12 | Spike FP8/V4 (`zk-pow` do branch `fp8-scheme`, microkernel QMMA, ≥ 1e6 átomos bit-exatos vs B200, memorando go/no-go) | 1,5 | M1, M5a | **G3** |
| M13 | Docs EN+PT-BR, CI/release/upstream-watch, tarball + SHA256SUMS + attestation, assinatura da carteira pelo dono, **v0.1.0 público** | 3 | M8, M9, M11 | **G4** |
| M14 | Piloto de 72 h + relatório (EN/PT-BR) + v0.1.1 | 3 | M13 | |
| M15 | Contingência: padrão oficial 2×64 no mesmo mainloop (só se uma pool rejeitar o 8×16) | 2 | M6 | |

## Alerta registrado em 2026-09-26
- ✅ **Carteira da taxa trocada em 2026-09-26 por carteira própria `oyster` (prl1pkqp…s90n).** Ainda pendente: Antes do v0.1.0: criar carteira própria (desktop wallet/oystercli) para `DEV_WALLET`; ajustar o limite de pagamento na HeroMiners para ≥ o depósito mínimo de PRL da SafeTrade (pagamentos de 1 PRL podem ser perdidos); a prova de controle (G4) só é possível com carteira própria.

## Regras que não mudam
- Nunca enviar share sem verificar localmente com o `zk-pow` oficial; `cert_version` ≥ 4 ⇒ pausar com "atualização necessária".
- GPU nesta máquina só como runtime `miner` do `spark-modo`; avisar antes de parar o vLLM; clock 2200 MHz nos testes; perfil **Balanced** (75 W alvo / 85 W corte) por padrão.
- Fee: constantes só em `crates/spm-fee/src/lib.rs`; READMEs e `docs/FEE.md` idênticos (CI falha se divergirem); sem config remota; sem ofuscação; sem packing.
- CI nunca minera; releases reprodutíveis e atestados; nunca copiar `akoya-miner`.
- Semanal: vigiar `Fp8ForkHeight`/PR #311. Mensal: atualizar `docs/pt-BR/VIABILIDADE.md`.

## Portões
- **G0** kernel INT8 bit-idêntico ao `zk-pow`; provas passam em `check_cert_version_eligible(3)` + `verify_plain_proof(Salted)`.
- **G1** soaks de 60 min e 24 h a 2200 MHz sem power-off, zero divergências, ≥ ~70 TH/s creditados.
- **G2** shares aceitas em LuckyPool BR e HeroMiners BR com o nosso minerador.
- **G3** `QMMA` FP8 do GB10 bit-exato contra a referência `fp8-scheme`.
- **G4** dono assina o endereço da fee (BIP-322 simples, oystercli) antes do release público.
