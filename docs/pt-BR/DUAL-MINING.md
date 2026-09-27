# Dual mining no DGX Spark — veredito (2026-09-26)

**Recomendação: nenhum dual mining que consuma energia.** O único complemento de custo zero (merge mining de NOCK) rende centavos e só faz sentido enquanto o minerador estiver na pool reserva (LuckyPool). Base: PRL a ~80 TH/s ≈ 1,85–1,94 PRL/dia ≈ US$2,4–2,7/dia (HeroMiners, taxa 0 %).

## 1. Merge mining (mesmas shares, 0 W extra)
| Opção | Situação | Rendimento a 80 TH/s | Observações |
|---|---|---|---|
| HeroMiners + MDL | **morto** | ~0,34 MDL/dia ≈ US$0,0001 | avisos de MDL comentados na página da pool, `modelos.herominers.com` não resolve, MDL ≈ US$0,0004 com ~US$1 mil/dia de volume |
| LuckyPool + NOCK (Nockchain AI-PoW) | ativo | ~0,37 NOCK/dia ≈ **US$0,011/dia** | login `ENDEREÇO_PRL+ENDEREÇO_NOCK[.worker]` (endereço nativo base58 da Nockchain); mínimo de 50 NOCK ≈ 134 dias; a dificuldade de IA dobrou numa semana |
| Kryptex | nenhum | — | — |

Trocar a pool *principal* para a LuckyPool por causa do NOCK dá prejuízo: a taxa de 1 % em PRL (US$0,024–0,028/dia) supera o NOCK. **Só na reserva** a taxa já está sendo paga, então o NOCK é ganho puro. Implementação: campo opcional "endereço NOCK" aplicado só a slots LuckyPool (M9, baixa prioridade).

## 2. Dual mining na CPU (RandomX / XMR) — medido aqui
`xmrig --bench=1M` (offline, sem pool), GPU ociosa: 20 threads 7,45 kH/s; 10 × Cortex-X925 5,30 kH/s; 10 × Cortex-A725 3,37 kH/s. A ~US$0,036 por kH/s/dia (XMR), bruto de US$0,27 / 0,19 / 0,12 por dia.
- **A carga nos X925 leva o SoC de ~50 °C a 84 °C em ~30 s e a 87,4 °C aos 60 s, ainda subindo**, dentro da faixa em que o controlador embarcado do DGX Spark desliga a máquina (relatos: ~87–98 °C de hotspot) — antes mesmo de somar o calor da GPU minerando PRL no mesmo die. **Descartado.**
- Só os A725 estabilizam em ~56–57 °C, mas uma perda de 5 % no PRL (US$0,12–0,135/dia) já apaga todo o bruto; CPU e GPU dividem um único orçamento de ~140 W que a NVML não enxerga, então o governor da GPU não consegue compensar os watts da CPU. **Não vale** (marginal na melhor hipótese, e só com medidor na tomada, parada por `acpitz` a ~80 °C e teste A/B de 24 h).

## 3. Segundo algoritmo na GPU
Nenhuma moeda de GPU supera o PRL neste chip (escalando da RTX 5090: Quantus ≈ US$1,5–1,9/dia usando 100 % da GPU; toda moeda limitada por memória ≤ US$0,40/dia com 273 GB/s). Time-slicing só divide o tempo de GPU (líquido ≈ −US$0,25 a −US$1,10/dia em 50/50); execução simultânea é inviável porque o CTA do PRL usa ~90 % dos registradores do SM e ~72 KB de shared memory; o governor de 75 W tiraria do PRL qualquer watt extra; e nenhum minerador dual fechado existe para aarch64. **Não.**

## 4. O que fazer com a folga de energia
Um clock maior no PRL valeria cerca de +5–10 % a cada 100–200 MHz (≈ +US$0,12–0,27/dia), mais do que todas as opções de dual mining somadas, mas está suspenso: o soak G1 nº 1 a 2200 MHz já chegou a 87 W e placa a 97,5 °C, então o padrão Balanced é 2000 MHz e 2300–2400 MHz ficariam dentro da faixa de desligamento. É estimativa; só rever com evidência nova sobre o desligamento.

Premissas de energia: R$0,80–1,10/kWh ≈ US$0,15–0,21; 1 W contínuo ≈ R$0,019–0,026/dia.
