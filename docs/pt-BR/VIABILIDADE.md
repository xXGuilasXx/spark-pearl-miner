# Viabilidade (retrato de 2026-09-26)

> Veredito da fase de pesquisa: **GO como piloto de código aberto; NO-GO como plano de renda.** Os números vêm de `docs/_data/facts.toml`; atualizar mensalmente. Tudo sobre o futuro é estimativa.

## 1. Quanto um DGX Spark pode render

| Item | Valor | Fonte |
|---|---|---|
| Hashrate da rede | ~54,8 EH/s (unidade MAC/s) | WhatToMine, API da HeroMiners |
| Dificuldade | ~29,4 M | WhatToMine |
| Recompensa por bloco | ~2.305,8 PRL, caindo ~4 %/mês, sem halvings | `node/chaincfg/params.go` |
| Tempo de bloco observado | ~151 s (alvo 194 s) | API da HeroMiners |
| Rendimento bruto | **0,0241 PRL por TH/s por dia** (= 306,9 × recompensa ÷ dificuldade) | derivado; hashrate.no mostra 0,0247 |
| Preço do PRL | US$1,30 (R$6,78); mínima US$0,14 em 2026-07-23, máxima US$1,76 em 2026-09-23 | CoinGecko |

**Unidade:** no Pearl, 1 "hash" = 1 multiplica-acumula int7×int7 do GEMM com ruído, normalizado ao rank 128. `1 TH/s = 10¹² MAC/s = 2 TOPS INT8` de GEMM útil. As pools creditam `diff × 2³²` MACs por share aceita.

**GB10, medido nesta unidade:** com o perfil padrão Balanced e o cap de clock de 2000 MHz este minerador sustenta **73,9 TH/s creditados (T-MAC/s) com ~63 W na GPU, GPU a 72 °C** (`docs/benchmarks/20260926-2000mhz-sustained.md`). O pico INT8 dos tensor cores (MB1, `docs/pt-BR/BENCHMARKS.md`) é 217 TOPS em clock stock e 170 TOPS a 2000 MHz, então o teto é ~108 TH/s (~85 TH/s no cap): o minerador roda a ~87 % do pico com cap. Um minerador fechado para DGX Spark declara ~76 TH/s a ~99 W.

| Hashrate | PRL/dia bruto | US$/dia @ 1,30 | US$/dia @ 0,70 | US$/dia @ 0,30 |
|---|---|---|---|---|
| 60 TH/s | 1,45 | 1,88 | 1,01 | 0,43 |
| **73,9 TH/s (medido, 2000 MHz)** | **1,78** | **2,32** | **1,25** | **0,53** |
| 75 TH/s | 1,81 | 2,35 | 1,27 | 0,54 |
| 90 TH/s | 2,17 | 2,82 | 1,52 | 0,65 |

Custos (a potência na tomada ainda não foi medida; a GPU em si consome ~63 W no cap padrão, o resto é SoC, memória, SSD e ventoinhas): estimados 100–130 W na tomada enquanto minera → 2,4–3,1 kWh/dia → R$1,9–3,4/dia (R$0,80–1,10/kWh) ≈ US$0,37–0,66/dia. Taxa da pool 0–1 %, taxa do desenvolvedor 2 %.
**Líquido a 75 TH/s e US$1,30: ≈ US$1,4–1,9/dia (≈ US$45–55/mês). Preço de equilíbrio: ≈ US$0,18–0,43/PRL.** O PRL valeu US$0,14 em julho de 2026.

Num Spark que também serve LLMs (como o do autor), só se minera com a GPU ociosa, então os números reais são menores.

## 2. Quanto rende a taxa do desenvolvedor

2 % do bruto: **≈ 0,036 PRL/dia ≈ 1,1 PRL/mês ≈ US$1,4/mês por Spark minerando 24/7** no preço e dificuldade de hoje. Cerca de **70 instalações sempre ligadas por US$100/mês**; o dobro com 50 % de ocupação. O único produto comparável tem 2 estrelas e 3 downloads (2026-09-26): a demanda não está provada. No Spark do próprio autor a taxa é zero (carteira de mineração = carteira da taxa ⇒ desliga sozinha).

## 3. Ventos contrários (todos medidos, nenhum hipotético)
- Dificuldade **+36 % em 30 dias**; PRL por TH caiu **−32 % em 10 semanas**; só o software dos mineradores dobrou o rendimento por placa entre junho e setembro de 2026.
- A recompensa por bloco cai suavemente: ~2.206 PRL em 1 mês, ~1.794 em 6 meses, ~1.435 em 12 meses.
- Se o tempo de bloco voltar ao alvo de 194 s, a emissão diária cai mais ~22 %.
- Liquidez fina e concentrada: ~US$2,9 M/dia de volume, ~87 % na SafeTrade (nota 1,9/5 no Trustpilot, com reclamações de saque). Emissão diária ≈ 60 % do volume diário (pressão vendedora).
- Pools concentradas: Kryptex ~45–50 %, pearlhash.xyz ~25–28 %.

## 4. Dois riscos que podem encerrar o produto com dias de aviso
1. **Hard fork FP8 / certificado v4.** Branch oficial `fp8-scheme`, PR #311 (aberto, sem altura de mainnet). Depois de `Fp8ForkHeight`, todo minerador int8/v3 passa a produzir shares inválidas. Alturas de fork anteriores foram definidas horas a dias antes da ativação. O FP8 `mma.sync` do GB10 (`QMMA.16832`) *provavelmente* reproduz bit-a-bit a aritmética presa ao B200 (medido até agora só numa RTX PRO 6000). O portão **G3** testa isso cedo; o mainloop é templatizado para um backend FP8 vir depois. ~70 % do código (pools, failover, taxa, GUI, energia, empacotamento) sobrevive ao fork.
2. **Desligamento abrupto do DGX Spark sob carga sustentada de GPU** (~88–92 W na GPU). A NVIDIA reconheceu como problema conhecido (2026-07-27) e não publicou correção; esta unidade já tem o firmware mais novo. Mitigação: cap de clock em 2000 MHz (unit de boot, root uma vez), governor de energia sem root com padrão **Balanced** (alvo 75 W, corte 85 W), soaks escalonados. Custo: ~22 % do pico stock (85,0 contra 108,6 T-MAC/s no MB1); o primeiro soak a 2200 MHz chegou a 87 W e placa a 97,5 °C, por isso o cap é 2000 MHz.

## 5. Critérios de parada / re-escopo
- G0 (bit-exatidão) falhar, ou o soak Balanced desligar a unidade.
- `Fp8ForkHeight` de mainnet ser definido antes do v0.1.0 e G3 falhar.
- PRL ficar abaixo do preço de equilíbrio para a tarifa de energia do autor.

## 6. Não é aconselhamento financeiro
Nada aqui é recomendação de investimento. Renda de mineração no Brasil é tributável (IN RFB 2291/2025, "DeCripto"); consulte um contador.
