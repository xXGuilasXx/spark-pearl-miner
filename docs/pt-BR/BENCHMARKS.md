# Benchmarks

Todos os números foram medidos no DGX Spark do autor (GB10, 48 SMs, CC 12.1, driver 580.178.04, CUDA 13.0) e são reproduzíveis com os scripts em `bench/`. Os logs brutos ficam em `docs/benchmarks/`.

## MB1 — pico dos tensor cores só em registradores (2026-09-26, `bench/mb1.sh`, binário sha256 `28bfcf4d…`)
`mma.sync.m16n8k32` com todos os operandos em registradores, 8 warps × 4 blocos por SM, 4 s por teste, clock do SM amostrado via NVML.

| Clock da GPU | INT8 `IMMA.16832` | FP8 `QMMA.16832` (e4m3→f32) | MAC/clk/SM | potência máx. da GPU |
|---|---|---|---|---|
| stock (2462 MHz) | **108,6 T-MAC/s** (217 TOPS) | 107,0 T-MAC/s (214 TOPS) | 919 | 51 W |
| 2200 MHz (cap do Max) | **96,0 T-MAC/s** (192 TOPS) | 95,9 T-MAC/s | 914 | 34 W |
| 2000 MHz (Balanced cap, default) | 85,0 T-MAC/s | 84,9 T-MAC/s | 893 | 26 W |
| 1800 MHz | 75,7 T-MAC/s | 75,8 T-MAC/s | 890 | 21 W |

O que isso significa para o PearlHash (1 TH/s de pool = 10¹² MAC int8/s):
- O teto é **~108 TH/s em clock stock e ~85 TH/s no cap de 2000 MHz** que o padrão Balanced usa por causa do desligamento conhecido (~96 TH/s a 2200 MHz, o cap opcional do Max).
- FP8 roda na **mesma taxa** que INT8, então o fork de certificado v4 (FP8) não reduziria o teto no GB10.
- ~919 MAC/clk/SM é ~90 % da taxa teórica de 1024; os 10 % restantes são overhead de emissão do loop só em registradores.
- Sem tráfego de memória a potência é baixa (51 W), mas um kernel real adiciona tráfego de shared memory e L2: a 2200 MHz o kernel de produção consumiu 83–87 W (soak G1 nº 1), por isso o cap do Balanced é 2000 MHz.
- A meta de planejamento do kernel v1 era 80–90 % do pico com cap; medido a 2000 MHz dá ~87 % (abaixo).

## Mineração sustentada no cap de 2000 MHz (`docs/benchmarks/20260926-2000mhz-sustained.md`)

| Perfil / cap | Taxa creditada | Potência da GPU | Temperatura da GPU | Duty |
|---|---|---|---|---|
| Balanced / 2000 MHz (padrão) | **73,9 T-MAC/s** | ~63 W | 72 °C | 100 % |
| Max / 2200 MHz (soak G1 nº 1, 16,9 min) | — | 83–87 W | até 83 °C, placa a 97,5 °C | — |

A taxa creditada anda em degraus de uma tentativa (7,04e13 MACs).
