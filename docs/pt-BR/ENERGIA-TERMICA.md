# Energia e temperatura

Como o spark-pearl-miner mantém um DGX Spark fora da faixa de desligamento. Código: `crates/spm-governor`; unit de boot: `packaging/systemd/system/spark-pearl-clockcap.service`; logger de soak: `bench/soak-log.sh`. Números: `docs/_data/facts.toml` (`[power]`). Inglês: [`docs/en/POWER-THERMAL.md`](../en/POWER-THERMAL.md).

## 1. O problema

Algumas unidades do DGX Spark **desligam abruptamente** (sem shutdown, nada nos logs) sob carga sustentada de GPU por volta de **88–92 W na GPU**. A NVIDIA reconheceu como problema conhecido em 2026-07-27 e não publicou correção; a unidade do autor já tem o firmware mais novo (ver [VIABILIDADE](VIABILIDADE.md) §4). Relatos públicos nos NVIDIA Developer Forums:

- [Hard power-off under sustained GPU load at ~90W, persists after full platform firmware update](https://forums.developer.nvidia.com/t/hard-power-off-under-sustained-gpu-load-at-90w-persists-after-full-platform-firmware-update/378315)
- [DGX Spark (GB10) reproducibly hard powers-off under GPU load](https://forums.developer.nvidia.com/t/dgx-spark-gb10-reproducibly-hard-powers-off-under-gpu-load-fully-updated-zero-crash-capture/373251)

O que o GB10 oferece:

- **Nenhum limite de potência por software.** `nvidia-smi -pl` não é suportado.
- **Trava do clock do SM**, `nvidia-smi -lgc 300,<MHz>`. Exige root e se perde no reboot.
- **Telemetria** via NVML (potência, clock do SM, temperatura da GPU, motivos de redução de clock) e as zonas térmicas ACPI (`acpitz` em `/sys/class/thermal`), tudo legível sem root.
- **O nosso próprio duty cycle.** O worker pode calcular durante uma fração de cada período e ficar ocioso no resto.

Para ter escala: o pico dos tensor cores só em registradores (MB1, [BENCHMARKS](BENCHMARKS.md)) consome 51 W em clock stock e 34 W a 2200 MHz. Um kernel de mineração real adiciona tráfego de shared memory e L2 (um minerador fechado para o Spark declara ~99 W), então sem controle ficaríamos bem dentro da faixa de desligamento. Com o modelo do vLLM residente e ocioso, esta unidade fica em 15 W e 2424 MHz.

## 2. Perfis

| Perfil | Alvo | Corte | Cap de clock recomendado | Observações |
|---|---|---|---|---|
| Eco | 60 W | 70 W | 1800 MHz | Silencioso e frio, longe da faixa. |
| **Balanced** (padrão) | **75 W** | **85 W** | **2000 MHz** | O padrão em todo lugar. |
| Max | 88 W | 92 W | 2200 MHz | Dentro da faixa de desligamento. Recusado sem `power.max_acknowledged = true`. |

O alvo é para onde o controlador leva a potência. O corte pausa a mineração (seção 4). O cap de clock é o valor da unit de boot.

## 3. Cap de clock (opcional, root uma vez)

Travar o clock do SM em 2200 MHz custa ~9 % do pico (96,0 contra 108,6 T-MAC/s no MB1) e é a principal rede de proteção: mesmo que o governor falhe, a GPU não sobe para a faixa. O minerador funciona sem ele, mas num DGX Spark eu recomendo.

```bash
packaging/install-clockcap.sh                 # imprime os comandos sudo exatos, não muda nada
sudo packaging/install-clockcap.sh --apply    # executa (instala a unit, daemon-reload, enable --now)
sudo packaging/install-clockcap.sh --apply --mhz 2000   # Eco
sudo packaging/uninstall-clockcap.sh --apply  # desativa, remove e restaura os clocks padrão (nvidia-smi -rgc)
```

A unit é `Type=oneshot` com `RemainAfterExit=yes`: `ExecStart=/usr/bin/nvidia-smi -lgc 300,2000` no boot (depois do `nvidia-persistenced`), `ExecStop=/usr/bin/nvidia-smi -rgc`. Ela de propósito não é ordenada depois do `multi-user.target`, porque esse target a puxa e a ordem viraria um ciclo.

**Detecção.** O NVML não tem consulta para clock travado, mas o cap aparece nos clocks: com ele instalado o clock do SM nunca passa de 2200 MHz, ocioso ou em carga (sem cap, esta unidade fica em 2424 MHz ociosa). O governor informa *sem cap* assim que uma amostra passa do cap por mais de 30 MHz, e *com cap* depois de 30 s de carga com duty ≥ 90 % sem passar dele. O veredito aparece na API de status (`power.clock_cap`: `unknown`, `capped` ou `uncapped`, com o maior clock visto).

## 4. Comportamento do governor

O daemon amostra a **10 Hz**: potência, clock do SM, temperatura da GPU e motivos de redução de clock via NVML, mais a zona `acpitz` mais quente. O NVML nunca cria contexto CUDA, então o daemon nunca aparece como processo de computação e não precisa de root. A `libnvidia-ml.so` é carregada em tempo de execução (feature `nvml` do cargo, ligada por padrão); quando ela não carrega, o daemon passa a usar `nvidia-smi --query-gpu=power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active --format=csv,noheader` a 2 Hz. Sem nenhum dos dois o governor não funciona: o daemon gera um alerta e não deixa um worker de GPU de verdade rodar (a simulação na CPU continua). Um buraco de mais de 3 s na telemetria segura o worker do mesmo jeito até as leituras voltarem.

**Como o daemon aplica.** Cada mudança de duty vai para o worker como um quadro IPC `SetDuty`; um worker novo ou retomado recebe o duty atual antes de voltar, e depois de mais de 30 s sem calcular recomeça de 10 %. Um corte pausa o worker (IPC `Pause`, contexto CUDA mantido, sem liberação por ociosidade) e o retoma quando o governor solta. Uma assinatura de falha para a mineração: alerta, o worker é liberado e a mineração fica parada (inclusive se o daemon reiniciar) até o usuário apertar Iniciar. Tudo aparece na API: `status.power` e `GET /api/v1/gpu` (`power`) trazem o perfil em vigor, alvo, corte, duty, a fonte da telemetria e a última leitura, o cap de clock, o corte em vigor com a contagem até a retomada, o número de cortes, o último corte e a falha travada; o fluxo SSE manda um evento `power` a cada corte, retomada, falha, troca de perfil e mudança de telemetria, e o estado pausado mostra `pause_reason` `power_trip`, `power_fault` ou `no_telemetry`.

**Controle de duty.** Um controlador PI ajusta o duty cycle do worker entre 10 % e 100 %: 0,5 % por watt de erro (proporcional) e 0,5 % por watt-segundo (integral), na forma incremental, de modo que limitar o duty já é o anti-windup. O duty sobe no máximo 20 %/s (10 → 100 % em 4,5 s) e desce sem limite. Acima de 78 °C na GPU o alvo cai 3 W por °C, para o controlador recuar antes do corte por temperatura. Num modelo de planta de primeira ordem de potência e temperatura (`spm_governor::sim`) ele mantém o alvo de cada perfil em **±3 W** durante 30 minutos simulados (pior leitura a 1,2 W do alvo, com ruído), com leitura de potência 4× mais lenta ou 2× mais ruidosa, depois de um job 12 % mais pesado e com a sala mais quente. No hardware isso ainda falta mostrar (TODO M11).

**Cortes.**

| Condição | Ação | Retomada |
|---|---|---|
| potência da GPU > corte em 3 amostras seguidas | pausa | depois de 60 s, com 10 % de duty, subindo em rampa |
| temperatura da GPU > 83 °C | pausa | depois de 60 s e GPU ≤ 78 °C e acpitz ≤ 90 °C |
| `acpitz` mais quente > 95 °C | pausa | igual à anterior |
| uma assinatura de falha (seção 6) | **para** e alerta | só depois que o usuário limpar o alerta |

Potência e temperaturas contam mesmo com o nosso worker parado: se outro processo já deixou a GPU tão quente ou tão carregada, não podemos somar carga.

**Outras regras.** Trocar para um perfil mais baixo corta o duty na mesma proporção na hora e mantém o corte anterior por 2 s enquanto a potência desce, para que uma troca pela GUI não dispare o corte novo. Depois de mais de 30 s sem calcular (cedendo ao vLLM, liberado), o próximo início volta a subir a partir de 10 %. Um buraco de mais de 2 s na telemetria reinicia a rampa e as janelas das assinaturas de falha.

## 5. Desligamento sujo: `running.marker`

Quando o daemon sobe, antes de qualquer worker calcular, ele grava `$XDG_STATE_HOME/spark-pearl-miner/running.marker` (perfil, horário de início, pid; com fsync) e o apaga numa parada limpa (SIGTERM, `systemctl --user stop`). Se o marcador já existir no início, a execução anterior terminou sem limpar: um crash, um `SIGKILL` ou um desligamento. A nova execução usa então **um perfil abaixo** do que estava rodando (Max → Balanced → Eco → Eco, nunca acima do configurado) e gera um alerta; a API de status mostra isso (`power.stepped_down`, `power.unclean_start`). A redução vale para aquela execução: um perfil mais alto escolhido no meio do caminho continua limitado, e depois de uma parada limpa volta o perfil configurado. Paradas sujas repetidas continuam reduzindo. Trocar o perfil com o daemon rodando regrava o marcador com o perfil em vigor.

## 6. Assinaturas de falha

Estes padrões indicam problema de hardware ou firmware, não de carga. O governor para a mineração e alerta em vez de tentar contornar.

| Assinatura | Padrão | Significado |
|---|---|---|
| `usb_pd` | clock do SM < 850 MHz com 5–15 W em carga (duty ≥ 50 %) por mais de 10 s | A negociação USB-PD falhou; a unidade roda com um orçamento de energia reduzido. |
| `safety_mode` | potência presa em 30 ± 3 W (variação ≤ 4 W) com clock do SM < 1400 MHz em carga por mais de 30 s | "Safety mode" do firmware, tratado pelo suporte da NVIDIA como sintoma de RMA. |
| `thermal_cap_100w` | potência presa em 100 ± 4 W (variação ≤ 4 W) por mais de 10 s, faça o nosso worker o que fizer | O cap térmico de 100 W está segurando a GPU. Como o corte por potência pausa o nosso worker em 0,3 s, ver isso significa que outra coisa carrega a GPU desse jeito ou que a pausa falhou. |

## 7. O que fazer com sintomas de RMA

1. Deixe a mineração parada. O governor já parou; limpar o alerta não conserta a unidade.
2. Desligue e religue o Spark com a fonte USB-C e o cabo **originais** ligados direto na tomada (sem hub, dock ou extensão). É o remédio habitual para `usb_pd`.
3. Verifique ventilação e poeira: saídas de ar livres, nada empilhado em cima, temperatura da sala. Há relatos de poeira causando desligamentos sob carga.
4. Junte evidências: `sudo nvidia-bug-report.sh`, o log de soak (`bench/soak-log.sh`, seção 8), `journalctl -b -1` depois de um desligamento e o export de diagnóstico do minerador.
5. Se `safety_mode` ou `usb_pd` voltar depois de desligar a frio, ou se os desligamentos continuarem no perfil Eco com o cap de clock instalado, abra um chamado no suporte da NVIDIA com o material acima. Não continue minerando nessa unidade.

## 8. Soaks (portão G1)

`bench/soak-log.sh` registra a cada 10 s: `nvidia-smi --query-gpu=timestamp,power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active` mais o `acpitz` mais quente, em `docs/benchmarks/soak-<UTC>.csv`. Não precisa de root e não mexe na GPU. Ao sair, imprime potência máxima, clock mínimo, temperaturas máximas, os motivos de redução de clock vistos, todo intervalo maior que 20 s entre linhas e toda sessão que terminou sem a linha de fim limpo (os dois indicam suspeita de desligamento). Depois de um desligamento, no boot seguinte: `bench/soak-log.sh --summarize <arquivo>`.

Plano, numa janela de GPU avisada e com o vLLM parado: escada de clock 1800–2200 MHz com 10 min por degrau, depois 60 min e 24 h no perfil padrão. Aprovação: nenhum desligamento, zero divergências de cálculo, ≥ ~70 TH/s creditados. Os resultados entram aqui quando existirem; **ainda não há** (o worker de GPU é o M5).

Para ver rapidamente o que o governor enxerga: `cargo run --release -p spm-governor --features nvml --example telemetry -- 10`.


**Soak G1 nº 1 (2026-09-26 21:54–22:11 UTC, `bench/g1-soak.sh`, cap 2200 MHz, forma de produção, vLLM parado):** 16,9 min, 102 amostras, **sem desligamento**. Minerando: potência média 82,7 W, máxima 87 W, subindo ~1 W a cada 5 min com o aquecimento do SoC; clock médio 2162 MHz; GPU máx 83 °C; **`acpitz` máx 97,5 °C**. Interrompido manualmente a 87 W (acima do corte de 85 W do Balanced e dentro da faixa relatada de desligamento). Consequência: o cap do Balanced passou de 2200 para 2000 MHz; os soaks de 60 min e 24 h serão repetidos a 2000 MHz com o governor ativo. Log bruto: `docs/benchmarks/g1-20260926T215417Z-soak.csv`.
