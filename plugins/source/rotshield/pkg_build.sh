#!/bin/bash
# pkg_build.sh — package the plugin source tree into a Slackware .txz bundle.
#
# Copies the explicit runtime file list (preserving relative paths) into
# a temp tree, tar'd into
#   ../../archive/<plugin>-<version>-x86_64-1.txz
# and the resulting bundle is what the .plg installs via upgradepkg.
#
# The bundle name is VERSIONED with the version from the .plg:
# unRAID's plugin manager never re-downloads a bundle file that already
# exists on the flash drive, so a versioned filename is what makes
# updates actually refresh the shipped files. The .plg derives its FILE
# Name and bundleURL from the same version entity, so we verify below
# that the .plg references exactly the bundle we are about to build.
#
# Unlike rathole we do NOT download a binary at install time — the
# scrub-rs binary is copied into
#   usr/local/emhttp/plugins/rotshield/bin/
# by CI (build-plugin.yml) before this script runs, so it is bundled.
# Test tools and unfinished API files are not in the runtime allowlist.

set -eu

DIR="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"
PLUGIN_DIR="$(dirname "$(dirname "${DIR}")")" # .../plugin
PLG="${PLUGIN_DIR}/rotshield.plg"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
plugin=$(basename "${DIR}")
archive="$(dirname "$(dirname "${DIR}")")/archive"

# The bundle version IS the <!ENTITY version> of the .plg — single source
# of truth for the plg, the bundle filename and the release asset name.
VERSION="$(sed -n 's/.*<!ENTITY version[[:space:]]*"\([^"]*\)".*/\1/p' "${PLG}" | head -1)"
if [ -z "${VERSION}" ]; then
    echo "FATAL: cannot read <!ENTITY version> from ${PLG}" >&2
    exit 1
fi
bundle="${plugin}-${VERSION}-x86_64-1.txz"

# Guard: the .plg must reference exactly this bundle (its bundlefile
# entity embeds &version;), otherwise the released asset would not match
# the .plg's FILE Name / bundleURL and updates would 404 or go stale.
grep -q "rotshield-&version;-x86_64-1.txz" "${PLG}" || {
    echo "FATAL: ${PLG} does not define the versioned bundlefile entity" >&2
    exit 1
}
grep -q "&bundlefile;" "${PLG}" || {
    echo "FATAL: ${PLG} does not use &bundlefile; in the bundle FILE element" >&2
    exit 1
}

mkdir -p "$tmpdir" "$archive"

cd "$DIR" || {
    echo "FATAL: cannot cd to $DIR"
    exit 1
}
# Adding a runtime file requires an intentional packaging change. In particular,
# do not package local test harnesses or unfinished API work from a dirty tree.
runtime="usr/local/emhttp/plugins/rotshield"
files=(
    etc/rc.d/rc.rotshield
    install/slack-desc
    "$runtime/bin/scrub-rs"
    "$runtime/build-info.json"
    "$runtime/icons/rotshield.png"
    "$runtime/images/rotshield.png"
    "$runtime/rotshield.page"
    "$runtime/status.php"
    "$runtime/scripts/install.sh"
    "$runtime/scripts/uninstall.sh"
    "$runtime/scripts/scrub.sh"
    "$runtime/scripts/notify-common.sh"
    "$runtime/scripts/notify-monitor.sh"
    "$runtime/scripts/results.php"
    "$runtime/scripts/status-parser.php"
)
for path in "${files[@]}"; do
    [ -f "$path" ] || {
        echo "FATAL: missing runtime file $path" >&2
        exit 1
    }
    cp --parents -f "$path" "$tmpdir/"
done
cd "$tmpdir" || {
    echo "FATAL: cannot cd to $tmpdir"
    exit 1
}
# Normalise line endings and make scripts executable.
find . -type f \( -iname '*.sh' -o -iname '*.page' -o -iname '*.plg' -o -iname 'rc.*' \) -exec sed -i 's/\r//g' {} +
chmod -R +x ./
# NOTE: do NOT pass --mode here. The --mode="a=r,u+w,a+X" form strips the
# execute bit from every file (including scripts/install.sh and
# scripts/uninstall.sh), which makes the .plg install/remove hooks fail with
# exit 126 ("/bin/bash returned 126"). The chmod -R +x above already set the
# correct perms, so we preserve them by omitting --mode entirely.
tar cfJCo "${archive}/${bundle}" "$tmpdir" . --owner=0 --group=0
rm -rf "$tmpdir"

echo "Built ${archive}/${bundle}"
sha256sum "${archive}/${bundle}"
