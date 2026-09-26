# Integração com o spark-modo (DGX Spark do autor)

Estes arquivos transformam o worker de GPU do spark-pearl-miner em mais um **runtime** do
`spark-modo`, ao lado de `spark-model` (LM Studio), `spark-gguf` e `spark-vllm`. Nada aqui é
aplicado automaticamente: revise, aplique com `sudo` e, se não gostar, rode o `rollback.sh`.

| Arquivo | O que faz |
|---|---|
| `spark-miner.service` | unit de sistema do worker: `ExecCondition=spark-modo permite-miner`, `ExecStart=spark-recurso miner -- …/spark-pearl-miner gpu-worker --attach /run/user/1000/spark-pearl-miner/worker.sock`, `User=xxguilasxx`, `SupplementaryGroups=spark-runtime`, `KillMode=mixed`, `Restart=no` |
| `spark-recurso.patch` | aceita o tipo `miner`: `LOCK_EX` na trava do recurso, só no modo `ia` e com o backend `miner` selecionado, e exige a GPU vazia (`require_idle_cuda`) antes do `exec`, como o vLLM |
| `spark-modo.patch` | `permite-miner` (ExecCondition), o runtime `spark-miner` no catálogo/seleção (backend `miner`, perfil `pearl`) e `spark-modo runtime miner`; os códigos de saída não mudam |
| `rollback.sh` | desfaz tudo (unit, patches via backups `*.pre-miner`, seleção volta para o LM Studio) |

## Como funciona

- O **daemon** (`systemctl --user` do xxguilasxx) fica sempre de pé, nunca cria contexto CUDA e
  escuta em `/run/user/1000/spark-pearl-miner/worker.sock`. Na GUI, escolha *Compartilhando a
  GPU → Runtime do spark-modo* (isso grava `worker.launch = "external"`).
- `sudo spark-modo runtime miner` descarrega o modelo que estiver na memória, espera a GPU
  esvaziar e sobe `spark-miner.service`. O `spark-recurso` pega a lease exclusiva, confere a GPU
  vazia e faz o `exec` do worker, que se conecta ao daemon. O spark-modo considera o runtime
  pronto quando o processo principal da unit já é o `spark-pearl-miner` (depois da lease).
- Quando um harness pede um modelo, o roteador chama `spark-modo runtime gguf|vllm|lmstudio …`:
  o spark-modo para o `spark-miner` (o worker sai na hora e libera o contexto) e sobe o modelo.
  O daemon mostra "Aguardando o spark-modo" até o minerador voltar a ser o runtime.
- `spark-modo treino` para o minerador junto com os serviços de IA, e `permite-miner` só
  devolve 0 no modo `ia` com o backend `miner` selecionado: no treino o minerador nunca sobe.
- O minerador **não** aparece como opção de pedido do portal (`ia miner` não existe): a troca
  completa do portal sincroniza os programas de desenvolvimento com o modelo escolhido, o que
  não faz sentido para o minerador. Use `sudo spark-modo runtime miner`.

> Até o marco M5 este binário ainda não tem o worker CUDA: `gpu-worker` sem `--sim` termina com
> código 2 e a unit falha logo depois da lease. Para testar o encanamento sem GPU, acrescente
> `--sim` ao `ExecStart` (a simulação roda na CPU e só acha shares numa pool de teste).

## Revisar

Os patches foram gerados com `diff -u` contra estas versões (confira antes de aplicar):

```
489dbabb0859ec0ba1568239482c1f7c54ca57e1f8339138a74027fd424ffe04  /usr/local/sbin/spark-modo
7f87d9a68f2f1b2dfb5c57e9483bdd64f6e7630768c21ca00baf1b6f54384b62  /usr/local/libexec/spark-recurso
```

```
sha256sum /usr/local/sbin/spark-modo /usr/local/libexec/spark-recurso
less contrib/spark-modo/spark-modo.patch contrib/spark-modo/spark-recurso.patch contrib/spark-modo/spark-miner.service
```

## Aplicar (com sudo)

1. Instale o binário e o daemon do usuário (sem sudo):

   ```
   install -Dm0755 target/release/spark-pearl-miner ~/.local/bin/spark-pearl-miner
   install -Dm0644 packaging/systemd/user/spark-pearl-miner.service ~/.config/systemd/user/spark-pearl-miner.service
   systemctl --user daemon-reload && systemctl --user enable --now spark-pearl-miner
   ```

   O `/run/user/1000` precisa existir mesmo sem sessão aberta: `sudo loginctl enable-linger xxguilasxx`.

2. Teste os patches e aplique guardando backups `*.pre-miner` (o `rollback.sh` usa esses backups):

   ```
   sudo patch --dry-run /usr/local/sbin/spark-modo < contrib/spark-modo/spark-modo.patch
   sudo patch --dry-run /usr/local/libexec/spark-recurso < contrib/spark-modo/spark-recurso.patch
   sudo patch -b -z .pre-miner /usr/local/sbin/spark-modo < contrib/spark-modo/spark-modo.patch
   sudo patch -b -z .pre-miner /usr/local/libexec/spark-recurso < contrib/spark-modo/spark-recurso.patch
   ```

3. Instale a unit (não habilite: quem a sobe é o `spark-modo runtime miner`):

   ```
   sudo install -m 0644 contrib/spark-modo/spark-miner.service /etc/systemd/system/spark-miner.service
   sudo systemctl daemon-reload
   ```

4. Confira: `spark-modo permite-miner; echo $?` deve dar `1` enquanto o minerador não for o
   runtime. Com o vLLM parado e a GPU vazia: `sudo spark-modo runtime miner`, depois
   `systemctl status spark-miner` e `spark-pearl-miner status`. Para devolver a GPU a um modelo:
   `sudo spark-modo runtime lmstudio` (ou peça o modelo por um harness).

## Desfazer

```
sudo contrib/spark-modo/rollback.sh
```

Ele para e remove a `spark-miner.service`, restaura os dois scripts a partir dos backups
`*.pre-miner` e, se a seleção apontava para o minerador, volta para `lmstudio/qwen3.8-27b`.
O daemon do usuário continua instalado; remova-o com
`systemctl --user disable --now spark-pearl-miner`.
