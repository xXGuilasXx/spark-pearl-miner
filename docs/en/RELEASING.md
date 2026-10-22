# Releases

Releases publish the code and Linux ARM64 binary from one tested commit. Merging a PR
changes `main`; the **CI** and **Release** workflows then build, validate and publish the
new version. Existing releases and tags are never replaced. The first version using
this process is **0.1.0-alpha.3**.

## Preparing a contribution

1. Review the entire PR, its discussions and checks, including security, compatibility
   and the tests appropriate to the change. Test untrusted contributions in isolation
   without repository or machine credentials.
2. If the change requires a new downloadable release, bump the version **once before
   merging**. If the contribution already includes an appropriate bump, retain it.
   Run from the repository root with Python 3.11 or newer:

   ```bash
   python3 tools/bump-version.py --dry-run
   python3 tools/bump-version.py
   ```

   An `alpha.N`, `beta.N` or `rc.N` version advances its numeric suffix; a stable
   version advances the patch number. For an intentional channel or version change:

   ```bash
   python3 tools/bump-version.py --version 0.1.0-beta.1
   ```

   The helper validates all inputs before writing. It updates
   `[workspace.package].version`, the workspace's own packages in `Cargo.lock`, and
   the status version in both READMEs. Registry and Git dependency versions remain
   unchanged. It rejects malformed versions, downgrades, mismatched lock entries and
   missing README markers. It does not run Git, merge or publish anything.
3. Include the version changes in the PR and rerun validation on its final commit:

   ```bash
   python3 tools/test-bump-version.py
   bash tools/ci-check.sh cpu
   ```

   CI must also pass for that final revision. CUDA changes require the full ARM64
   build and the applicable GB10 hardware evidence described below. Do not approve
   a later, unreviewed SHA on the strength of an earlier successful run.
4. Merge the validated revision while preserving contributor authorship. The CI run
   for the resulting `main` commit is the release gate, including any changes brought
   in by the merge.

## Automated build and publication

The **CI** workflow runs CPU checks first, followed by the **ARM64 CUDA package** job
on a Linux ARM64 runner. The build uses the pinned Rust **1.98.1** toolchain and a
digest-pinned CUDA **13.0.2** development image (`nvcc` 13.0.88). Cargo uses `--locked`.
The full check includes workspace host tests, Clippy, the CUTLASS host-layout test,
SASS checks, installer tests, version and fee consistency checks, and release packaging.

The full job produces the `release-linux-aarch64` artifact: the versioned Linux
ARM64 tarball, `SHA256SUMS`, and build metadata recording the complete commit SHA
and the validation limits. **Release** accepts only a successful CI run triggered by
a push to `main`, consumes that run's exact artifact, verifies its contents and
checksums, and creates the tag at that tested SHA. It does not rebuild code from a
later `main` commit. GitHub build-provenance attestation is required before publication.

Publication uses a draft until its assets and attestation are ready, then rechecks
the commit and downloads before making the release public. If `main` has advanced,
the older run is skipped. An owned draft can be retried after failure; unrelated
drafts and conflicting tags are left untouched and block publication. Alpha, beta
and release-candidate versions are marked as prereleases, not as GitHub's stable
"latest" release.

Each version is published once. An already published version is skipped;
publishing another build requires a new version. Do not move an existing tag or
replace its downloads to distribute a fix. Failures remain visible in GitHub Actions
and block publication. Inspect and resolve a failed run before retrying it; never
bypass a failed test or attestation to publish manually.

The binary is packaged by `packaging/make-release.sh`. Its tarball and checksums
remain usable with the existing installer. New release notes must identify the
included fixes and distinguish compile/host validation from actual GPU testing.

## Hardware validation limits

**CI does not emulate a GB10 and does not test mining on a GPU.** Both
`SPM_GPU_TESTS` and `SPM_NVML_TESTS` are forced to `0`. A successful ARM64/CUDA build
confirms that the code compiles and its host checks pass; it does not establish
accepted shares, throughput, power draw, temperature or long-running stability on
a DGX Spark.

The maintainer no longer has a DGX Spark. Changes to kernels, GPU correctness,
performance, power or thermal behavior require relevant evidence from a contributor
with a real GB10 before approval and release. An RTX 3080 cannot supply this evidence:
the current kernel uses GB10-specific facilities, including TMA and `setmaxnreg`.

Version **0.1.0-alpha.3** distributes the fixes from PRs #1, #2 and #3 and adds the
automated pipeline. It has no new GB10 execution test. The CUDA sources and the GPU
worker implementation are unchanged from `0.1.0-alpha.2`; the release notes must
state the hardware-validation limitation explicitly.

## Local checks and cleanup

`python3 tools/test-bump-version.py` is a permanent regression suite and creates its
fixtures only in automatically removed temporary directories. The helper also
accepts `--root PATH` to operate on an isolated checkout. Keep permanent regression
tests in the repository, and remove temporary builds, logs, screenshots, containers
and investigation scripts after validation.

See also: [Contributing](../../CONTRIBUTING.md),
[Kernel contract](KERNEL.md), and [Português](../pt-BR/LANCAMENTOS.md).
