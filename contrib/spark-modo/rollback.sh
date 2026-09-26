#!/bin/sh
# Desfaz a integração do minerador com o spark-modo (rode com sudo):
#   1. para e remove a unit spark-miner.service;
#   2. volta o spark-modo e o spark-recurso aos backups *.pre-miner feitos no `patch -b`;
#   3. se a seleção de runtime aponta para o minerador, volta para o padrão (LM Studio).
# Não mexe no modo (ia/treino), nos outros runtimes nem no daemon --user do minerador.
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "rode com sudo: sudo $0" >&2
    exit 77
fi

SELECAO=/var/lib/spark-modo/modelo.json

if systemctl list-unit-files spark-miner.service >/dev/null 2>&1; then
    systemctl disable --now spark-miner.service 2>/dev/null || true
fi
rm -f /etc/systemd/system/spark-miner.service

for f in /usr/local/sbin/spark-modo /usr/local/libexec/spark-recurso; do
    if [ -f "$f.pre-miner" ]; then
        cp -a "$f.pre-miner" "$f"
        echo "restaurado: $f"
    else
        echo "sem backup $f.pre-miner: nada a restaurar (o patch não foi aplicado?)" >&2
    fi
done

if [ -f "$SELECAO" ] && grep -q '"miner"' "$SELECAO"; then
    tmp="$SELECAO.tmp$$"
    printf '%s\n' '{"backend": "lmstudio", "perfil": "qwen3.8-27b"}' > "$tmp"
    chmod 0644 "$tmp"
    mv "$tmp" "$SELECAO"
    echo "seleção de runtime voltou para lmstudio/qwen3.8-27b (peça o modelo de novo no portal se quiser outro)"
fi

systemctl daemon-reload
echo "pronto: integração do minerador removida."
