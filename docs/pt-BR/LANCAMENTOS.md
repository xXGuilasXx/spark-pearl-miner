# Lançamentos

Cada release publica o código e o binário Linux ARM64 de um único commit testado.
O merge de um PR altera a `main`; os workflows **CI** e **Release** então compilam,
validam e publicam a nova versão. Releases e tags existentes nunca são substituídas.
A primeira versão com esse processo é a **0.1.0-alpha.3**.

## Preparação de uma contribuição

1. Revise o PR completo, as discussões e os checks, incluindo segurança,
   compatibilidade e os testes adequados à mudança. Execute contribuições ainda não
   confiáveis em ambiente isolado, sem credenciais do repositório ou da máquina.
2. Se a mudança exigir uma nova versão para download, incremente a versão **uma
   única vez antes do merge**. Se a contribuição já tiver um incremento adequado,
   preserve-o. Na raiz do repositório, com Python 3.11 ou mais recente:

   ```bash
   python3 tools/bump-version.py --dry-run
   python3 tools/bump-version.py
   ```

   Uma versão `alpha.N`, `beta.N` ou `rc.N` avança seu sufixo numérico; uma versão
   estável avança o patch. Para mudar de canal ou escolher a versão explicitamente:

   ```bash
   python3 tools/bump-version.py --version 0.1.0-beta.1
   ```

   O helper valida todas as entradas antes de gravar. Atualiza
   `[workspace.package].version`, os pacotes próprios do workspace em `Cargo.lock`
   e a versão de status nos dois READMEs. Versões de dependências de registry e Git
   são preservadas. Versões inválidas, regressões de versão, entradas divergentes
   no lock e marcadores ausentes nos READMEs são recusados. Ele não executa Git,
   faz merge ou publica arquivos.
3. Inclua a alteração de versão no PR e repita a validação no commit final:

   ```bash
   python3 tools/test-bump-version.py
   bash tools/ci-check.sh cpu
   ```

   O CI também precisa passar nessa revisão final. Mudanças CUDA exigem a
   compilação ARM64 completa e as evidências aplicáveis no GB10 descritas abaixo.
   Não aprove um SHA posterior sem revisão usando o resultado de uma execução
   anterior.
4. Faça o merge da revisão validada preservando a autoria da contribuição. O CI
   do commit resultante na `main` é o requisito para publicação e inclui quaisquer
   alterações incorporadas pelo merge.

## Compilação e publicação automáticas

O workflow **CI** executa os checks de CPU e, em seguida, o job **ARM64 CUDA package**
em um runner Linux ARM64. A compilação usa Rust **1.98.1** fixado e a imagem de
desenvolvimento CUDA **13.0.2** fixada por digest (`nvcc` 13.0.88). O Cargo usa
`--locked`. A validação completa inclui testes de host do workspace, Clippy, o teste
de layout CUTLASS no host, checks SASS, testes do instalador, consistência de versão
e taxa, além do empacotamento da release.

O job completo gera o artefato `release-linux-aarch64`: o tarball Linux ARM64 com
versão, `SHA256SUMS` e metadados com o SHA completo do commit e os limites da
validação. **Release** aceita apenas um CI bem-sucedido originado por push na `main`,
consome o artefato exato dessa execução, verifica o conteúdo e os checksums e cria
a tag no SHA testado. Ele não recompila o código de um commit posterior da `main`.
A atestação de proveniência da compilação do GitHub é obrigatória antes da publicação.

A publicação usa um draft até os arquivos e a atestação ficarem prontos, e então
confere novamente o commit e os downloads antes de tornar a release pública.
Se a `main` avançou, a execução anterior é ignorada. Um draft do próprio processo
pode ser retomado após falha; drafts de outra origem e tags conflitantes são
preservados e impedem a publicação. Versões alpha, beta e release candidate são
marcadas como pré-lançamentos, não como a versão estável "latest" do GitHub.

Cada versão é publicada uma única vez. Se a versão já estiver publicada, a publicação
é ignorada; outro build precisa de uma nova versão. Não mova tags existentes nem
substitua seus downloads para distribuir uma correção. Falhas ficam visíveis no
GitHub Actions e impedem a publicação. Investigue e resolva uma execução com falha
antes de repeti-la; não ignore testes ou atestação para publicar manualmente.

O binário é empacotado por `packaging/make-release.sh`. O tarball e seus checksums
continuam compatíveis com o instalador existente. As notas de cada release devem
identificar as correções e distinguir compilação e testes de host de testes reais
na GPU.

## Limites da validação de hardware

**O CI não emula um GB10 nem testa mineração em GPU.** As variáveis
`SPM_GPU_TESTS` e `SPM_NVML_TESTS` são forçadas para `0`. Uma compilação ARM64/CUDA
bem-sucedida confirma a compilação e os checks de host; não comprova shares
aceitas, desempenho, consumo, temperatura ou estabilidade prolongada no DGX Spark.

O mantenedor não tem mais um DGX Spark. Mudanças em kernels, correção na GPU,
desempenho, energia ou comportamento térmico precisam de evidências relevantes
fornecidas por um colaborador com GB10 real antes de aprovação e lançamento.
A RTX 3080 não fornece essa evidência: o kernel atual usa recursos específicos
do GB10, incluindo TMA e `setmaxnreg`.

A versão **0.1.0-alpha.3** distribui as correções dos PRs #1, #2 e #3 e acrescenta
o pipeline automático. Ela não tem novo teste de execução no GB10. Os fontes CUDA
e a implementação do worker da GPU permanecem iguais aos da `0.1.0-alpha.2`;
as notas da release devem declarar explicitamente esse limite de validação.

## Checks locais e limpeza

`python3 tools/test-bump-version.py` é uma suíte permanente de regressão e cria
suas fixtures apenas em diretórios temporários removidos automaticamente. O helper
também aceita `--root CAMINHO` para operar em um checkout isolado. Preserve os testes
permanentes no repositório e remova builds, logs, capturas de tela, contêineres e
scripts temporários de investigação após validar.

Veja também: [Contribuição](../../CONTRIBUTING.md),
[Contrato do kernel](../en/KERNEL.md) e [English](../en/RELEASING.md).
