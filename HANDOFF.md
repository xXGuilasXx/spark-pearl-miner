# Retomada do trabalho — spark-pearl-miner (29/09/2026)

Este arquivo é o ponto de partida para continuar o projeto num chat novo. Detalhes locais da minha
máquina (integração com o `spark-modo`, preferências de pool) ficam na memória do Claude Code, não
aqui.

## Estado atual

- **main** (ver `git log -1`), publicado no GitHub. Pre-releases **v0.1.0-alpha.1** e **v0.1.0-alpha.2** com
  tarball aarch64 + `SHA256SUMS`; o instalador (`packaging/install.sh`) baixa a mais nova e confere
  o checksum.
- **Kernel**: INT8 `mma.sync` para `sm_121a`, bit-exato contra o `zk-pow` oficial (portão G0).
- **Pools ao vivo**, todas com shares aceitas: Kryptex (`prl-br.kryptex.network:8048`, TLS, sessão
  v2 com provas em gzip por padrão), HeroMiners BR (`br.pearl.herominers.com:1200`), LuckyPool BR
  (`pearl-br.luckypool.io:3360`, SPKI fixada). Failover automático testado ao vivo (troca em ~1 s,
  volta após 60 s estável). Chave avançada `pools.login` para entrar numa pool com uma conta.
- **Energia**: perfil Balanced com cap de 2000 MHz. **Portão G1 cumprido**: soak de 60 min
  (74,5 T-MAC/s) e de **24 h** (77,9 T-MAC/s na média, 67 W médios / 71 W máx, GPU até 80 °C,
  placa até 89 °C, sem disparo, sem desligamento, 765 aceitas / 4 stale / 0 inválidas).
  Evidências em `docs/benchmarks/g1-*`.
- **Produto**: GUI enxuta (assistente de 3 passos, um painel, diálogo de ajustes; o resto no
  `config.toml` comentado), acesso local sem token para o mesmo usuário, manual ilustrado EN/PT
  (`docs/*/MANUAL.md`, capturas por `tools/screenshots.sh`), README voltado ao produto.
- **Taxa de 2 %** divulgada e imutável (`crates/spm-fee/src/lib.rs`, guarda de CI no README).

## Regras de trabalho (resumo; as completas estão em `CLAUDE.md`)

- Uma branch por trabalho; merge em `main` só com `tools/merge-check.sh` verde, **checando o código
  de saída** (`if tools/merge-check.sh > log; then …`); push automático depois de cada merge verde.
- Commits, comentários e docs como se fossem meus, sem nenhuma marca de IA. Subagentes em Opus 5.5.
- `export CARGO_TARGET_DIR=$HOME/.cache/spark-pearl-miner/target` (o caminho do repo tem espaço).
- Não compilar nem rodar gates durante soaks (a CPU esquenta a placa e dispara o governor).
- Nunca usar `pgrep -f` com um padrão que apareça no próprio comando (mata o shell); usar `ps`
  ancorado. Loggers longos: `(setsid nohup script … &)`.

## O que falta (ordem sugerida)

Feito em 29/09: **degrau de 2100 MHz testado e descartado.** Placa até 88,4 °C (igual a 2000 MHz),
mas +4,9 W (máx 78,6 W, acima do alvo de 75 W um quarto do tempo) e nenhum ganho de taxa
(−2,3 ± 2,2 T-MAC/s num A/B intercalado). O cap continua em 2000 MHz. Evidências em
`docs/benchmarks/clk2100-20260929-*`.

1. **Provas comprimíveis**: preencher A/B de forma repetitiva para a prova cair de ~120 KB para
   poucos KB em gzip/zstd (a Kryptex diz ~100×), reduzindo stale; confirmar aceitação nas pools
   (item no TODO, M6).
2. **Lacunas entre tentativas** (M10): a 2000 MHz o minerador ocupa os SMs só 87–93 % do tempo
   (`nvidia-smi pmon`) e um kernel mais rápido não aumentou a taxa. Expor os tempos de tentativa e
   de espera nas stats/API, achar a espera e mirar ~98 % (+5–10 % sem mais clock nem potência).
   Explicar os dois patamares no mesmo clock (~84 T-MAC/s a ~71 W e ~76 a ~66 W).
3. **Hora de aceitação na HeroMiners** (último item do M6).
4. **M12 — FP8 / certificado v4**: acompanhar o PR #311 e a `Fp8ForkHeight`; spike de `QMMA` e
   bit-exatidão contra o `zk-pow` do branch `fp8-scheme` antes do fork (risco nº 1 do projeto).
5. **M13 — release estável**: CI em `ubuntu-24.04-arm` com atestação de build, provar o controle
   da carteira da taxa (G4, `signmessage`), revisar a estimativa de ganhos do README com dados
   atuais.

## Comandos úteis

```bash
spark-pearl-miner status                     # estado, taxa de 60 s, shares, pools
journalctl -u spark-miner.service -f          # log do minerador (na minha máquina)
cd "/home/xxguilasxx/Desktop/Miner PRL/spark-pearl-miner" && tools/merge-check.sh
packaging/make-release.sh                     # tarball + SHA256SUMS; imprime o gh release create
bench/soak-log.sh --interval 10 --duration 3600 --out docs/benchmarks/<nome>.csv
```

## Prompt para o novo chat

> Continue o projeto spark-pearl-miner (minerador de Pearl/PRL para o DGX Spark) no repositório
> `/home/xxguilasxx/Desktop/Miner PRL/spark-pearl-miner`. Leia primeiro `HANDOFF.md`, `CLAUDE.md`,
> `TODO.md` e a memória do projeto. Estado: v0.1.0-alpha.2 publicada, portão G1
> cumprido (soak de 24 h), minerador rodando no modo mineração desta máquina. Siga as regras de
> branch, gate com código de saída checado, push automático após merge verde, sem marca de IA e
> subagentes em Opus 5.5. Comece pelo item 1 de "O que falta" (provas comprimíveis), avisando
> antes de qualquer coisa que pare ou reinicie o minerador.
