# Benchmarks

Todos os números foram medidos no DGX Spark do autor (GB10, 48 SMs, CC 12.1, driver 580.178.04, CUDA 13.0) e são reproduzíveis com os scripts em `bench/`. Os logs brutos ficam em `docs/benchmarks/`.

## MB1 — pico dos tensor cores só em registradores (2026-09-26, `bench/mb1.sh`, binário sha256 `28bfcf4d…`)
`mma.sync.m16n8k32` com todos os operandos em registradores, 8 warps × 4 blocos por SM, 4 s por teste, clock do SM amostrado via NVML.

| Clock da GPU | INT8 `IMMA.16832` | FP8 `QMMA.16832` (e4m3→f32) | MAC/clk/SM | potência máx. da GPU |
|---|---|---|---|---|
| stock (2462 MHz) | **108,6 T-MAC/s** (217 TOPS) | 107,0 T-MAC/s (214 TOPS) | 919 | 51 W |
| 2200 MHz (cap de segurança) | **96,0 T-MAC/s** (192 TOPS) | 95,9 T-MAC/s | 914 | 34 W |
| 2000 MHz | 85,0 T-MAC/s | 84,9 T-MAC/s | 893 | 26 W |
| 1800 MHz | 75,7 T-MAC/s | 75,8 T-MAC/s | 890 | 21 W |

O que isso significa para o PearlHash (1 TH/s de pool = 10¹² MAC int8/s):
- O teto é **~108 TH/s em clock stock e ~96 TH/s no cap de 2200 MHz** que recomendamos por causa do desligamento conhecido.
- FP8 roda na **mesma taxa** que INT8, então o fork de certificado v4 (FP8) não reduziria o teto no GB10.
- ~919 MAC/clk/SM é ~90 % da taxa teórica de 1024; os 10 % restantes são overhead de emissão do loop só em registradores.
- Sem tráfego de memória a potência é baixa (51 W); um kernel real adiciona tráfego de shared memory e L2, então o alvo Balanced de 75 W tem folga.
- Meta de planejamento para o kernel v1: 80–90 % do pico com cap ⇒ **77–86 TH/s a 2200 MHz**.
