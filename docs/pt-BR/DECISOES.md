# Registro de decisões

| Data | Decisão | Por |
|---|---|---|
| 2026-09-26 | Repo `xXGuilasXx/spark-pearl-miner`, Apache-2.0, docs EN (principal) + PT-BR | usuário |
| 2026-09-26 | Taxa do desenvolvedor fixa e divulgada de 2,00 %; constantes só em `crates/spm-fee/src/lib.rs` | usuário |
| 2026-09-26 | Pools padrão: HeroMiners BR → LuckyPool BR → Kryptex; sessão da taxa na HeroMiners (fallbacks Kryptex/LuckyPool); worker `devfee` | usuário |
| 2026-09-26 | Uma sessão autorizada (que nunca envia share) na HeroMiners com a carteira do usuário para capturar o dialeto | usuário |
| 2026-09-26 | No Spark do autor o minerador é o 4º runtime `miner` do `spark-modo` (lease exclusivo da GPU); o repo público continua genérico, integração em `contrib/spark-modo/` | usuário |
| 2026-09-26 | O vLLM pode ser parado para janelas de GPU com aviso antes de cada parada; clock travado em 2200 MHz nos testes | usuário |
| 2026-09-26 | Toolchain instalado no usuário (rustup 1.98.1, submódulo CUTLASS v4.8.0, zk-pow oficial @3fe2267) | usuário |
| 2026-09-26 | Público desde o 1º commit com banner pré-alfa; commits como `xXGuilasXx <guilasamaral@gmail.com>` | usuário |
| 2026-09-26 | O endereço inicial da taxa era de depósito de exchange; uma carteira própria (`oyster`/`oystercli` oficiais) está sendo criada para substituí-lo antes do v0.1.0 | usuário |
| 2026-09-26 | Host Rust novo + biblioteca CUDA em vez de bifurcar o CPPminer (3 desenhos, 3 juízes unânimes); daemon sem CUDA + `gpu-worker` descartável; GUI web embutida em 127.0.0.1:4078 | painel de design, aceito |
| 2026-09-26 | Todo subagente roda em Opus 5.5 (`CLAUDE_CODE_SUBAGENT_MODEL`) | usuário |
| 2026-09-26 | Nova carteira própria da taxa criada com o `oystercli` oficial (SPV); `DEV_WALLET = prl1pkqp…s90n` | usuário |
| 2026-09-26 | Cap de clock da GPU em 2000 MHz no padrão Balanced (substitui a trava de 2200 MHz dos testes acima): o soak G1 nº 1 a 2200 MHz chegou a 87 W e placa a 97,5 °C; a 2000 MHz o minerador sustenta 73,9 T-MAC/s a ~63 W, GPU a 72 °C | medição |
| 2026-09-27 | Ordem padrão das pools Kryptex (8048, TLS) → HeroMiners BR → LuckyPool BR (substitui a ordem acima): a Kryptex é a pool recomendada pelo dono, as três verificadas ao vivo com 0 rejeitadas; um `config.toml` existente nunca é reescrito | plano do projeto (um commit reversível) |
| 2026-09-27 | GUI enxuta: configuração em 3 passos (idioma, carteira, taxa + Começar), um painel, um diálogo de Configurações (carteira, worker, idioma, três pools `host:porta`); as telas Pools, Failover, Energia, Taxa, Logs e Sobre saem da GUI, mas os endpoints da API e os comandos da CLI continuam | plano do projeto |
| 2026-09-27 | Um `config.toml` comentado escrito pelo `Config::to_toml()` (sem `toml_edit`): bloco Básico (GUI) + conjunto Avançado pronto para o DGX Spark; comentários acrescentados à mão não são mantidos num salvamento pela GUI | plano do projeto |
| 2026-09-27 | Instalador em um comando (`packaging/install.sh`, sem root) e tarball de release com SHA256SUMS (`packaging/make-release.sh`); builds reprodutíveis e atestados ficam para depois | plano do projeto |
| 2026-09-27 | Manual do usuário ilustrado EN/PT-BR (`docs/{en,pt-BR}/MANUAL.md`) com capturas tiradas de um minerador simulado pelo `tools/screenshots.sh` | pedido do usuário |
