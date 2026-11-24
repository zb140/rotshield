# Personal downstream releases

Assessment date: November 18, 2026. No branches, commits, tags, releases, uploads,
or production changes were made for this assessment.

## Current first-release implementation

The personal branch uses the existing build-plugin.yml workflow, not a new
release framework. It builds with the lockfile, runs Rust and shell regressions,
stages build-info.json, and packages an explicit runtime allowlist. It generates
the final SHA-256-pinned manifest after packaging and publishes that manifest,
the archive, build record, and SHA256SUMS as a personal prerelease on a matching
version tag. The source manifest is explicitly a build template.

For this first release, pluginURL is pinned to that same release asset rather
than a moving channel branch. Subsequent upgrades are intentional/manual. This
simpler choice supersedes the optional channel-branch recommendation below.
GitHub Actions must be enabled on the fork before pushing the release tag;
its built-in repository token handles publication without personal credentials.

API work in scrub.sh/status.php and the new results/parser files is deliberately
left in the local working tree, outside the release commits and runtime package.
A preservation snapshot is stored locally under plugins/archive; it is not pushed.

## Original inspection: release problems addressed by the workflow change

- `origin` is already `git@github.com:zb140/rotshield.git`.
- The current source base is `3bc673c`, containing the METADATA_UUID fix. The
  uncommitted scanner/runner/notification work is not represented by that commit.
- `plugins/rotshield.plg` still uses upstream `digi-deity/rotshield` for **both**
  `pluginURL` (future manifest checks) and `bundleURL` (package downloads).
  Merely publishing a package to the fork will not redirect either one.
- The manifest pins SHA-256, but `.github/workflows/build-plugin.yml` computes
  MD5, does not inject SHA-256, and publishes only the `.txz`. The existing pinned
  hash is not a hash of a newly built downstream package. A differently built
  package cannot reuse it. Keep checksum validation; fix the publishing process.
- The existing workflow publishes on matching `v*` tags, not on an ordinary branch
  push. It checks that the tag matches the manifest version. Do not push a release
  tag expecting this unmodified workflow to produce a usable downstream install.
- `pkg_build.sh` includes almost every file under the plugin source directory;
  the local stop harness would be shipped unless omitted from release staging.
  Its `craft-corrupt` exclusion is useful but not a general release allowlist.
- `install.sh` preserves an existing config, reapplies cron, and deletes cached
  packages for other versions. Keep rollback packages **outside** that cache.

These are packaging/update blockers, not reasons to rename the plugin or wait
for upstream.

## Recommended model

Use the existing fork as a personal distribution home. Retain the `rotshield`
plugin/package/config identity so it upgrades the existing install, rather than
creating two plugins that might run against the same disks. Keep upstream credit
and licensing. No Community Apps submission, advertising, or maintainer promise
is necessary.

Keep logical safety fixes separable for upstream review; keep fork-only URL,
release-channel, and qualification material in a distinct downstream change.
Upstream review and downstream use can proceed independently.

### Two independently controlled release steps

1. **Build immutable release assets from a recorded source revision.**
2. **Promote a known-good manifest into the personal update channel.**

Recommended update channel: a dedicated branch (proposed name
`downstream-release`) serving:

```text
https://raw.githubusercontent.com/zb140/rotshield/downstream-release/plugins/rotshield.plg
```

This preserves the raw-manifest URL pattern used by the existing plugin. The
branch need not be created or changed until an actual release is approved.
Keep its manifest pointed at a fixed release asset, never an arbitrary CI run.
A channel branch can contain the release manifest/notes only; it need not be the
branch used to develop Rust changes.

For each release manifest:

- `name`, install paths, config paths, and package name remain `rotshield`.
- `pluginURL` points to the personal channel above.
- `bundleURL` points to an immutable tag in `zb140/rotshield`, with the existing
  versioned `rotshield-<version>-x86_64-1.txz` naming convention.
- `sha256` is computed from the **finished** package, not copied from upstream.
- Manifest `version`, any retained `bundleversion`, tag, and filename agree.
- Changelog clearly labels the personal build and whether repair is qualified.

Do not use `releases/latest` for the bundle: a moving binary URL and fixed
checksum/version can disagree. A draft/prerelease package should not be promoted
into the production update channel automatically.

Use a distinct, monotonically ordered downstream version. The exact suffix and
Unraid version comparison behavior need validation on Unraid before selecting a
permanent scheme; upstream currently uses date-plus-letter versions. Do not
assume SemVer prerelease ordering applies to Unraid. Version uniqueness also
avoids the cached-file behavior described in the current manifest.

## Build/publish acceptance criteria

Before the first release:

1. Preserve the current dirty work and organize reviewed commits. Do not publish
   an unrecorded mixture of working-tree edits as a supposedly reproducible tag.
2. Build from a clean staging checkout at a known source revision using the
   committed `Cargo.lock` (`cargo build --locked --release --bin scrub-rs`).
   Record the toolchain/target; validate compatibility with Unraid 7.3.3's glibc.
   A generic Ubuntu GNU binary is not automatically proof of compatibility.
3. Run local notification tests, Rust tests, syntax/static checks, and the
   fixture qualification required for that release's intended write policy.
   Packaging CI currently runs independently of heavy integration tests; a
   successful package build is not repair approval.
4. Stage only runtime files, `install/slack-desc`, the correct binary, and the
   rc script. Check executable permissions. Explicitly reject `craft-corrupt`,
   harnesses, outputs, temporary files, and unexpected files in the archive.
5. Include a build record in the package (source revision, upstream base,
   downstream version, toolchain/target, and binary SHA-256). Hash the final
   archive and generate the manifest **after** packaging. The manifest is
   outside the package, so there is no self-referential checksum problem.
6. Verify the manifest resolves to the intended fork/tag/asset and the exact
   archive SHA-256. Verify the packaged scanner hash matches the tested binary.
7. Prepare a draft release containing the `.txz`, generated `rotshield.plg`,
   `SHA256SUMS`, and source/build/test notes. No extra binary download at plugin
   startup is needed; preserve the existing bundled-binary model.
8. Download and verify those exact assets, then publish and promote the identical
   manifest to the personal channel only after explicit release approval.
   Never silently overwrite an asset under an already-installed version.

A small fork-only build/release workflow should implement these criteria. It
should generate the release manifest/checksum, upload all matching assets, run
notification tests, and default to a draft release. The existing workflow has
not been modified here. Local builds remain possible; GitHub is only the chosen
source/release distribution service, not part of the scan or repair data path.

## Switching novinha and validating persistence

Follow `../tests/NOTIFICATIONS.md` for idle-state, backup, config, and notification
checks. The first downstream release must remain **read-only**.

- Save the currently installed `.plg`, working package, config, and cron outside
  the install cache. Do not remove the plugin to switch sources: uninstall can
  stop work/remove installed files unnecessarily.
- Install/update with the exact generated downstream manifest using Unraid's
  normal plugin mechanism. Determine whether this is a first downstream upgrade
  or same-version reinstall before selecting the command; do not guess a force
  flag or use `upgradepkg` alone as proof the persisted `.plg` changed.
- Verify the saved `/boot/config/plugins/rotshield.plg` really contains the
  personal `pluginURL`, pinned fork asset URL, and correct hash. Unraid's next
  update check must not route back to upstream.
- Verify the running binary/build record, installed package version, preserved
  config (`WRITE=0`, no hidden `--repair` in extra options), unchanged monthly
  expression `0 3 5 * *`, and both cron entries.
- Smoke-test local Dynamix delivery. Test restart/reboot persistence at a suitable
  maintenance window before treating the release mechanism as qualified.
- To roll back, restore the matching old manifest/package/channel intentionally,
  while idle, and preserve `WRITE=0`. Do not rely on install cache retention.
- Avoid an independent auto-update rule replacing this safety-critical plugin
  without approval. A personal channel prevents accidental upstream selection;
  it does not itself disable Unraid auto-update settings.

No production paths, settings, packages, update rules, or binaries have been
inspected remotely or changed by this assessment. No GitHub fetch/push or issue
lookup was performed; remote-tracking refs are local observations, not proof of
current upstream activity or issue/PR status.
