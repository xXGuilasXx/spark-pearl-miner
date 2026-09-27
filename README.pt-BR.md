# spark-pearl-miner

**Minerador de Pearl (PRL) de código aberto feito para o NVIDIA DGX Spark (GB10, `sm_121`, aarch64).**
Não oficial. Sem afiliação, patrocínio ou endosso da NVIDIA ou da Pearl Research Labs.
_English: [README.md](README.md)_

> **Status: pré-alfa (planejamento / bring-up).** Nada aqui minera ainda. Acompanhe o [TODO.pt-BR.md](TODO.pt-BR.md).

## O que é
- Um minerador de Proof-of-Useful-Work (PearlHash, GEMM int7×int7→int32) cujo kernel CUDA usa nativamente o caminho INT8 `mma.sync` do GB10 (`sm_121a`), verificado bit-a-bit contra a referência oficial `zk-pow` antes de qualquer share ser enviada.
- Um daemon que nunca segura contexto CUDA, um processo `gpu-worker` descartável que segura, e uma GUI web local (EN/PT-BR) para configurar a **carteira** e **até 3 pools** com **failover automático**.
- Feito para a realidade do DGX Spark: o desligamento abrupto conhecido sob carga sustentada de GPU (cap de clock + governor sem root), pressão de memória unificada e convivência com um vLLM residente.

## Taxa do desenvolvedor (divulgada)
`dev fee 2.00% → prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n @ br.pearl.herominers.com:1200 (HeroMiners), worker "devfee", fatias de 120 s, só enquanto minera`
Todas as constantes da taxa vivem em um único arquivo, `crates/spm-fee/src/lib.rs`; o CI falha se este README divergir dele. **A carteira da taxa não é configurável**: não há flag, variável de ambiente, chave de config nem API que a altere, e a GUI a mostra somente para leitura. Os releases oficiais são compilados de forma reprodutível e atestados para você conferir que roda o original. Sem configuração remota, sem ofuscação, sem binários empacotados. A taxa se desliga sozinha quando a sua carteira é a carteira da taxa.

## Doações
Se este projeto for útil para você, doações em PRL são bem-vindas no mesmo endereço:

`prl1pkqprrek7pemaxyvl4deusyz2hrkywnkhl86w7yqv53x0qyvsd5fs57s90n`

## Pools
O minerador vem com três presets de pool e failover automático entre elas: HeroMiners BR, LuckyPool BR e Kryptex (TLS na porta 8048); veja `docs/pt-BR/CONFIGURACAO.md`. Se ainda precisa escolher uma: eu já minerei PRL na **Kryptex** e nunca tive problema com os pagamentos dela. Cadastrar-se pelo meu link de referência não custa nada e ajuda este projeto:

https://pool.kryptex.com/?ref=b2cfe3e2 (link de referência)

## Expectativas honestas
Um GB10 deve atingir cerca de 65–85 TH/s (creditados, unidade das pools) dentro de um envelope seguro de 75–85 W, o que, nas condições de rede de setembro de 2026, dá cerca de 1,6–2,0 PRL/dia brutos. A dificuldade subiu 36 % nos 30 dias anteriores a este texto e a recompensa por bloco cai ~4 % ao mês. Já existe um minerador fechado para DGX Spark; a proposta deste projeto é ser _aberto e auditável_, não _o primeiro_. Leia `docs/pt-BR/VIABILIDADE.md` antes de gastar com hardware ou energia.

## Requisitos (alvo)
DGX OS 7.x (Ubuntu 24.04, aarch64), driver CUDA 13.0 ≥ 580, Rust ≥ 1.88 para compilar, um endereço de carteira Pearl (`prl1…`, bech32m). Opcional: `sudo` uma vez para instalar o cap de clock da GPU no boot.

## Documentação
- [Arquitetura](docs/pt-BR/ARQUITETURA.md) · [Contrato do kernel (EN)](docs/en/KERNEL.md) · [Benchmarks](docs/pt-BR/BENCHMARKS.md) · [Viabilidade](docs/pt-BR/VIABILIDADE.md)
- [Energia e térmica](docs/pt-BR/ENERGIA-TERMICA.md) · [Coexistência com LLM residente](docs/pt-BR/COEXISTENCIA.md) · [Dual mining](docs/pt-BR/DUAL-MINING.md)
- [Protocolos das pools (EN)](docs/protocol/) · [Decisões](docs/pt-BR/DECISOES.md) · [TODO](TODO.pt-BR.md)

## Licença
Apache-2.0 — veja [LICENSE](LICENSE) e [NOTICE](NOTICE) (ISC: Pearl Research Labs e The Decred developers; BSD-3: NVIDIA CUTLASS).
