# Desktop E2EE release qualification

Do not tag a desktop release until every required gate below has evidence for
its release candidate. A passing implementation task or an older checkpoint is
not release approval. Record the source SHA, artifact hashes, platform, commands,
exit statuses, findings, and reviewer/owner decision for each gate. Reuse evidence
only after checking the relevant source, dependencies, and environment for changes.

## Required gates

- [ ] **Protocol and security review:** independent review of the whole final
  implementation, not only changes since an earlier review. Cover dependencies,
  key lifecycle, pairing, membership/rotation, backup boundaries, attachment
  binding, hostile servers, and secret exposure. Resolve findings and revalidate.
  Check server databases, objects, logs, and backups for known plaintext and
  usable keys. The [protocol maintenance contract](SYNC_PROTOCOL.md) and
  [core sync implementation](crates/aven-core/src/sync/) are review inputs.
- [ ] **Physical-iPhone interoperability before desktop tagging:** build the
  phone against the final desktop core revision; scan-to-join over HTTPS,
  synchronize a task in each direction, and transfer one image. Record phone,
  OS, app/core/server revisions, TLS origin, completion state, and image hash.
  Simulator and desktop-only tests do not satisfy this protocol-freeze gate.
- [x] **Integrated validation:** `just check-full` on the candidate; include
  opt-in batch crash-boundary tests. Retain full logs and actual test counts.
  Follow [TESTING.md](TESTING.md); do not substitute compilation for tests.
- [x] **Desktop manual journey:** isolated production CLI/TUI/daemon setup,
  invitation/join, task/image transfer, conflicts, removal/rotation, offline
  work, interrupted setup/join, missing credentials, and actionable failure
  states. Include TLS success and certificate/proxy failure cases. Distinguish
  executed current checks from older checks carried forward by source comparison.
- [ ] **Migration and recovery:** old-release sync before upgrading, current
  images, backup, fresh server storage, and empty-peer join. Verify missing-image
  refusal, preservation of unavailable devices, interrupted publication/install,
  nonempty-join and bound-target restore/import refusal. Preserve post-cutover
  work before rollback. No cross-vault merge or same-vault restore is promised.
- [ ] **Performance decision:** representative history and image workloads,
  memory/CPU, writer holds and responsiveness, retry reuse, round trips, and
  storage growth. Retain ranges and failures. Compare timings under controlled
  conditions before claiming no regression; investigate unexplained differences.
- [ ] **Platform lifecycle:** macOS CLI/TUI/daemon access and restricted Linux
  storage; missing/unavailable keys pause sync without blocking local work.
  Exercise locked-login and pre-GUI launchd on a disposable macOS account; never
  lock a developer's normal login Keychain for qualification.
- [ ] **Signed distribution:** verify the protected `macos-signing` environment
  and actual [release workflow](.github/workflows/release.yml) artifacts for both
  macOS architectures, plus both Linux release targets. Check hashes of extracted
  and installed bytes. macOS signatures must satisfy the stable Developer ID
  requirement enforced by [the signer](scripts/sign-macos-release), with identifier
  `fi.zendit.Aven` and team `YG824X87Y2`. Verify silent Keychain reads and daemon
  continuation across independently signed builds through Homebrew, install
  script, and `aven update`; record CI run links. Do not release ad-hoc bytes.
- [x] **Licenses and documentation:** current lockfile passes `cargo deny check`
  against [deny.toml](deny.toml); publish accurate [sync](docs/src/content/docs/sync.md),
  [backup](docs/src/content/docs/backups.md), [privacy](docs/src/pages/privacy.astro),
  and [changelog](CHANGELOG.md) documentation. Build the website. Keep local
  plaintext, metadata leakage, stale/split-view limits, and cutover omissions
  explicit. App Store export compliance and mobile license notices gate the
  iOS release separately; they do not replace the pre-tag phone smoke test.

Recovery kits, self-removal, same-vault restore, hosted service implementation,
periodic snapshots, compaction, and global ciphertext deduplication are outside
this desktop release. Notarization and updater signer verification are separate
follow-ups, not evidence supplied by checksum verification.

## Desktop manual journey: October 4, 2026

Executed on `efe11a8e` with the normal debug CLI, isolated databases, loopback
HTTP servers and real macOS Keychain accounts (cleaned afterwards). Raw logs and
harnesses are under `history/2026-10-04-desktop-manual-journey/`.

| Area | Result |
| --- | --- |
| Base journey | October 2 `manual.py` rerun at this source: setup, two joins, task and exact image transfer, removal and rotation, removed-device refusal, daemon pull, missing wrapping key, canary scan, backup and new-vault recovery. Passed. |
| Offline work | Server stopped: `sync` fails with `[encrypted-tail-network]` and local edits continue; `doctor` shows pending changes. After restart, offline edits sync and non-conflicting changes merge. |
| Conflicts | Concurrent offline description edits produce a conflict on both devices; resolving with a variant token converges both. The guide's `--use local` example failed and is fixed in `a50141f4`. |
| Interrupted setup | Unreachable server: `[bootstrap-network]` with next step. Setup killed at 0.15, 0.4 and 0.8 s (during upload): status reports setup incomplete, rerunning setup with the same invitation completes, and a fresh peer receives all 8 images byte-for-byte. |
| Interrupted join | Server down during join: `[enrollment-network]`; retry with a fresh invitation succeeds. Joiner killed mid-join: status and `sync` say joining is incomplete with recovery steps; a fresh invitation completes it. Local data added during a pending join is refused with `[shared-state-install]` and a next step. |
| Missing credentials | Deleting a device's protected key files: `[protected-key-storage-missing]` with a do-not-replace hint, local work continues, nothing is regenerated; restoring the files resumes sync including work made meanwhile. |
| TUI and TLS | Carried forward from the [October 3 Linux TUI/TLS QA](history/2026-10-03-e2ee-tui-tls-qa/README.md) at `092022f0` plus `69bde6ca`: TUI setup, join, two-way sync and device removal over HTTPS; wrong-host, self-signed and expired certificates; proxy body limit. TUI, sync and HTTP client sources changed by one image-lifecycle line since. |

Observations, not fixed: if the inviting device quits, the joiner waits for the
10-minute invitation lifetime showing only "Waiting for the inviting device...";
the inviter's device list shows joined devices without names.

## Assessment: October 2, 2026

**Not ready to tag.** Production source assessed:
`7de8910f9bba06e8cc9d590a1a6bfd6e43e8318a` (still unreleased). Qualification
changes add documentation and atomically publish the benchmark worker's readiness
file; they do not change the production protocol or dependencies.

Raw evidence is retained locally under
`history/2026-10-02-desktop-release-qualification/`. That directory is ignored by
Git: the results below are the portable evidence summary, not a claim that raw
logs ship with the repository. The release owner must retain/export raw evidence
with final approval. No tag, release upload, personal sync data, or signing
secret was changed by this qualification.

### Fresh executed evidence

| Area | Result and scope | Local evidence |
| --- | --- | --- |
| Integrated gate | `just check-full`, exit 0: 3,153/3,153 nextest tests; 26 skipped. Clippy, formatting check, dependency/static checks, migration ordering and SQLx validation passed. Doctest targets passed with one ignored core example. This ran before the test-only readiness fix; subsequent focused release tests cover that fix. | `check-full.log`, `check-full.exit`, copied `test.log`, `doc-test.log`, `clippy.log`, `cargo-deny.log` |
| Opt-in durability/platform | Three selected tests passed: `process_crashes_preserve_batch_freeze_acceptance_and_partial_observation`, `isolated_macos_keychain_smoke_test`, `isolated_seed_keychain_reopen`. Batch test covers five process-crash stages; Keychain tests cover wrapped-secret tamper, key loss/no regeneration and fenced reopen. | `opt-in-final.log`, exit 0 |
| Production macOS CLI | Three devices: seed setup, two joins, bidirectional task edit, exact image bytes, remove one peer, survivor sync across rotation, rejected removed-device sync with local editing still usable. Direct non-TTY daemon pulled a peer edit silently; deleting another peer's wrapping key paused sync with `protected-key-storage-missing` while local editing continued, without key regeneration. Backup refused over a bound target, restored into a fresh DB, seeded a new vault and joined an empty peer. All checks passed; exact temporary Keychain accounts/processes/files cleaned. No TLS, TUI, service installation or signed upgrade in this run. | `manual.py`, `manual-expanded-pass.log`, exit 0 |
| Canary | Random fixture title/description/image alt text absent from five files comprising server DB/WAL/object storage and a consistent SQLite copy. This is a narrow plaintext check, not a usable-key audit, full log/backup audit, or independent security approval. | `manual-expanded-pass.log` |
| Near-cap setup | Three successful fresh release-test processes with 247 images / 267,431,441 plaintext bytes, separate server, actual `engine::run_setup`. Wall: 9.671/16.684/24.787 s; peak RSS: 72,663,040/88,145,920/76,513,280 B; longest writer hold: 1.821430/1.170163/3.671821 s. 78–79 requests, about 268.07 MB HTTP body traffic, exact progress/record-byte agreement, no dropped or rolled-back transactions. | `near-cap-fixed-{2,3,4}.log`, all exit 0 |
| Setup resume | In-process future cancellation (not process death): 32 MiB at 10.48% and 54.17%, 64 MiB at 90.42%. Frozen intent and staged ciphertext preserved exactly; full replay/adoption succeeded. | `interrupt-10.log`, `interrupt-50.log`, `interrupt-90-64mib.log`, all exit 0 |
| Licenses | Current gate's cargo-deny summary: zero advisory/license/source/ban errors; seven permitted duplicate-version warnings. Applies to the configured macOS arm64/Linux GNU graph, not independently to every distribution target or mobile notices. | `cargo-deny.log` |
| Documentation | Frozen-lockfile install and Astro build passed after sync-guide updates. | `docs-build.log`, exit 0 |

Production CLI binary SHA-256:
`397959c75a9fddc760035dbfea16bc9b35e975a78ea107e4fc528a9377d50ccc`.
This is the normal debug CLI emitted by integration compilation, not the unit-test
file-key backend and not a signed release artifact. Host: Mac14,5, arm64 macOS
Darwin 25.5.0, 12 CPUs, 32 GiB RAM; Rust 1.98.1.

### Failures and qualifications

- The benchmark parent observed a newly created but not fully written origin
  file and failed `bootstrap-origin` before any HTTP request. The worker now
  writes `origin.pending` then renames it; six subsequent near-cap/resume runs
  passed. The failed original run is `near-cap-3.log` (exit 100).
- A 32 MiB fixture crossed its 90% threshold only on the final whole-batch
  acknowledgement, violating the fixture's requirement to cancel before full
  upload (`interrupt-90.log`, exit 100). Increasing that fixture to 64 MiB
  provided a genuine 90.42% interruption; no production change was needed.
- One release-test compilation exceeded the tool's 600-second timeout
  (`near-cap-fixed-1.log`, status 124); the later build completed in 11m 59s.
  No aborted compilation is counted as validation.
- Current timing samples are slower and noisier than September 29's 5.835 s
  median, and longest writer holds reach 3.672 s. Host load averages were
  75–87 on 12 CPUs when inspected. Relevant production bootstrap paths have no
  diff from `5ce50fc2`, but this does not establish causation or acceptable UI
  latency. **Performance timing acceptance remains open** pending a controlled
  comparison or an explicit release-owner decision. Memory remains consistent
  with the spooled implementation, not the earlier 624 MB baseline.
- Initial CLI harness attempts exposed assumptions about flags/JSON and an
  inherited `AVEN_SYNC_DISABLED`; initial daemon setup also collided with the
  normal wake port. The successful run used an isolated ephemeral UDP port via
  its temporary config. Those failed harness logs are retained, not counted as passes.
  Combined nextest filtering required direct invocation with the standard test
  environment because `just _test` expands parentheses unquoted; use
  `--run-ignored ignored-only`, not `ignored`.

### Supporting older evidence, not current-candidate approval

| Area | Evidence and remaining limitation |
| --- | --- |
| Protocol | September 21 format, chunk, pairing, membership and ordering specs/vectors under `history/`; current regression suites live in core sync and HTTP modules. Full final independent implementation review remains required. |
| Faults/interoperability | [Checkpoint faults](history/2026-09-23-e2ee-checkpoint-faults.md), [journey](history/2026-09-23-e2ee-checkpoint-journey.md), and [pre-batch mixed-binary compatibility](history/2026-09-28-batch-validation-compatibility.md). Compatibility harness pins `2948a456`; current image paths changed, so do not represent its old/new run as fresh current evidence. |
| Security | [September 25 independent reproductions](history/2026-09-25-e2ee-security-review.md) informed fixes `88590119`, `d8e623ad`, `f7013697`, `87b155f1`. These and later crypto/storage changes still require whole-candidate review. |
| Performance | [September 29 near-cap/resume measurements](history/2026-09-29-integrated-reconcile-final-measurement.md), [history-heavy setup](history/2026-09-28-setup-upload-perf-results.md), and attachment scan optimization `346c5e82`. Not an iPhone CPU/battery benchmark. |
| Platform/TLS | [Real macOS Keychain/ACL/daemon QA](history/2026-09-27-e2ee-macos-keychain-qa.md), [Caddy HTTPS/proxy/certificate QA](history/2026-09-27-e2ee-tls-proxy-qa.md), and Linux [CLI/TUI QA](history/2026-09-23-e2ee-cli-manual-qa.md). Locked-login/pre-GUI cases and public WebPKI/physical-phone TLS remain unverified here. |
| Signing | Credential-storage task notes dated September 30 report configured protected signing secrets and two Developer-ID-signed local builds at `4231a9c7`: silent non-TTY/launchd reads, copy-and-rename upgrade, byte-identical Homebrew installation and daemon restart. Those owner-recorded results are not an executed current CI release, Intel test, or `aven update` test. Certificate renewal is due before February 1, 2027. |
| Migration | [Fresh backup/restore journey](history/2026-09-25-e2ee-backup-restore-qa.md) and [old-release cutover harness](history/2026-09-30-e2ee-upgrade-qa.sh) for Linux v0.1.44 → `5497c527`. The migration task is marked complete under narrowed desktop scope; its open phone dependency is not completed by that status. |

### Remaining release-owner actions

1. Obtain the final whole-implementation independent review and resolve findings.
2. Perform the required physical-iPhone HTTPS smoke test against the final core.
   The referenced `/Users/raine/code/aven-ios` checkout is absent on this host;
   no mobile source, export declaration, or phone result was inferred.
3. Resolve timing acceptance with controlled measurements or a recorded decision.
4. Verify current packaged/signed CI artifacts, Intel and Linux targets, and the
   remaining updater upgrade path. Do not create production tags merely to
   collect qualification evidence without explicit release authorization.
5. Exercise locked-login/pre-GUI behavior on a disposable account and carry
   forward remaining CLI/TUI/daemon/TLS evidence only after a relevant-source
   comparison, or rerun affected scenarios.
6. Archive approval/evidence against the exact shipping SHA. Full mobile QA,
   before-first-unlock/restore, mobile notices and Apple export compliance remain
   downstream iOS gates; the physical-phone protocol smoke is still a desktop gate.
