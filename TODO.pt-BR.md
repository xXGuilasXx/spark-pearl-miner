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
| M5 | `libspm_cuda` v0 + `gpu-worker` **bit-exato** (128×256×64, cp.async 3 estágios, tile de hash 8×16 em registradores, epílogo BLAKE3, KAT, canário, cancelamento por época) | 6–10 | M1, M5a | **G0** — 🟡 kernel C (TMA+mbarrier) bit-exato em 54 problemas, 87,5 T-MAC/s a 2300 MHz; processo gpu-worker pronto (`crates/spm-worker`: KAT, canário, provas verificadas, cancelamento por época em < 0,3 ms, 85–86 T-MAC/s ponta a ponta no formato padrão); selftest/bench no cap de 2000 MHz pendentes (referência sustentada já medida a 2000 MHz: 73,9 T-MAC/s creditados a ~63 W, GPU 72 °C) |
| M6 | Primeiras shares aceitas com o nosso minerador (LuckyPool BR → HeroMiners BR/TLS, 1 h: ≥ 20 aceitas, 0 invalid, stale < 1 %) | 1 | M4, M5 | **G2** — ✅ 2026-09-27: as três pools padrão ao vivo com 0 rejeitadas (LuckyPool BR 5 e HeroMiners BR 4 em 2026-09-26; Kryptex com a primeira share aceita em 2026-09-27 00:24 UTC; failover e retorno à primária ao vivo; `docs/benchmarks/m6-20260926T222707Z-first-shares.md`); a hora completa na HeroMiners passa para o piloto M14 |
| M7 | Máquina de estados de failover de 3 pools (redutor puro, ≥ 25 cenários, proptest, mockpool, teste ao vivo < 15 s) | 3 | M4 | ✅ 2026-09-26 (42 cenários + proptest; ao vivo: porta fechada → pool 2 em 1,1 s; volta à primária após sondagem + 60 s; 0 rejeitadas) |
| M8 | Dev fee 2 % transparente (`spm-fee`: constantes únicas, débito 200/9800, fatias 120 s, worker `devfee`, HeroMiners→LuckyPool→Kryptex, auto-off se carteira = dev, banner/log/API/`fee-test`, guarda de CI README×constantes) | 1,5 | M6, M7 | ✅ 2026-09-26 (29 testes; constantes imutáveis) |
| M9 | API + GUI web EN/PT-BR (assistente, dashboard, 3 slots de pool com failover ao vivo, energia, fee, logs, sobre; segurança token→cookie+CSRF; ✅ acesso local do mesmo usuário sem token em 2026-09-26: `api.trust_local_user`, outras contas e acesso remoto continuam com token), unit `systemd --user`, `.desktop`, `contrib/spark-modo` (runtime `miner`) | 4 | M7 | ✅ 2026-09-27: painel enxuto (configuração em 3 passos, um painel, diálogo de Configurações), manual ilustrado EN/PT com 16 capturas por idioma e o instalador em um comando |
| M10 | Kernel v1 (persistente 48 CTAs, TMA p/ B, raster L2, A' duplo-buffer; ≥ 85 % do pico IMMA; meta ≥ 76 TH/s a ≤ 85 W) | 5 | M5, M6 | |
| M11 | Governor de energia (Eco/Balanced/Max), unit de clock-cap, coexistência (yield/yield-release), guarda de memória, soaks 60 min + 24 h | 2 | M6 | **G1** — 🟡 código pronto e ligado ao daemon (M9b: governor, marcador, coexistência, guarda de memória, protocolo de pausa); soaks, demo de SIGKILL ao vivo e testes sob carga do vLLM pendentes |
| M12 | Spike FP8/V4 (`zk-pow` do branch `fp8-scheme`, microkernel QMMA, ≥ 1e6 átomos bit-exatos vs B200, memorando go/no-go) | 1,5 | M1, M5a | **G3** |
| M13 | Docs EN+PT-BR, CI/release/upstream-watch, tarball + SHA256SUMS + attestation, assinatura da carteira pelo dono, **v0.1.0 público** | 3 | M8, M9, M11 | **G4** |
| M14 | Piloto de 72 h + relatório (EN/PT-BR) + v0.1.1 | 3 | M13 | |
| M15 | Contingência: padrão oficial 2×64 no mesmo mainloop (só se uma pool rejeitar o 8×16) | 2 | M6 | |

## Instalação em um comando (2026-09-27, versão 0.1.0-alpha.1)
- 🟡 `packaging/install.sh` (como usuário, sem root; `curl -fsSL …/packaging/install.sh | bash`, de um clone ou do tarball extraído): baixa o release mais novo verificado pelo SHA256SUMS ou compila do fonte (rustup + nvcc), instala binário, unit `--user` com `ExecStartPre=config check`, atalho no menu e `~/.local/share/spark-pearl-miner/`; nunca mexe no config.toml; `--upgrade` (com volta automática se o binário novo recusar o config.toml), `--rollback`, `--uninstall [--purge]`, `--dry-run`. `packaging/make-release.sh` gera o tarball determinístico + SHA256SUMS e só imprime o `gh release create`. Teste: `tools/test-install.sh` (40 verificações num HOME descartável com binários falsos; `--real` empacota e instala o binário de verdade). Pendente: o primeiro release publicado por mim.

## Alerta registrado em 2026-09-26
- ✅ **Carteira da taxa trocada em 2026-09-26 por carteira própria criada com o `oystercli` oficial (prl1pkqp…s90n).** Resolvido: o endereço de depósito da SafeTrade não é mais usado. Ainda pendente antes do v0.1.0: a prova de controle (G4), agora possível com a carteira própria.

## Regras que não mudam
- Nunca enviar share sem verificar localmente com o `zk-pow` oficial; `cert_version` ≥ 4 ⇒ pausar com "atualização necessária".
- GPU nesta máquina só como runtime `miner` do `spark-modo`; avisar antes de parar o vLLM; clock 2000 MHz nos testes (cap padrão desde o soak G1 nº 1: 2200 MHz deu 87 W e acpitz 97,5 °C); perfil **Balanced** (75 W alvo / 85 W corte) por padrão.
- Fee: constantes só em `crates/spm-fee/src/lib.rs`; READMEs e `docs/en/FEE.md`/`docs/pt-BR/TAXA.md` (quando existirem) idênticos (CI falha se divergirem); sem config remota; sem ofuscação; sem packing.
- CI nunca minera; releases reprodutíveis e atestados; nunca copiar `akoya-miner`.
- Semanal: vigiar `Fp8ForkHeight`/PR #311. Mensal: atualizar `docs/pt-BR/VIABILIDADE.md`.

## Portões
- **G0** kernel INT8 bit-idêntico ao `zk-pow`; provas passam em `check_cert_version_eligible(3)` + `verify_plain_proof(Salted)`.
- **G1** soaks de 60 min e 24 h a 2000 MHz sem power-off, zero divergências, ≥ ~70 TH/s creditados.
- **G2** shares aceitas em LuckyPool BR e HeroMiners BR com o nosso minerador.
- **G3** `QMMA` FP8 do GB10 bit-exato contra a referência `fp8-scheme`.
- **G4** dono assina o endereço da fee (BIP-322 simples, oystercli) antes do release público.
