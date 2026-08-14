#!/usr/bin/env bash
#
# Step 3: claim the npm package names with placeholder releases.
#
# CI cannot perform the first publish: the release workflow authenticates via
# npm's OIDC trusted publishing, and npm only lets you configure a trusted
# publisher on a package that already exists. So each name has to be created
# once by hand, after which CI takes over for every subsequent release.
#
# These placeholders are version 0.0.1 - far below the real 0.7.0 - and contain
# nothing but a package.json. Merging the release PR (step 5) publishes the real
# packages and moves the "latest" tag off them.
#
# Nothing in the repository is modified; everything is staged in a temp dir.
#
# Usage:  ./scripts/step3-claim-npm-names.sh
#         ./scripts/step3-claim-npm-names.sh --dry-run   # pack only, no publish

set -uo pipefail

SCOPE="@denisixnpm"
EXPECTED_USER="denisixnpm"
PLACEHOLDER_VERSION="0.0.1"
PLATFORMS=(darwin-arm64 darwin-x64 linux-x64 linux-arm64 win32-x64 win32-arm64)

DRY_RUN=""
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN="--dry-run"
  echo "DRY RUN - packing only, nothing will be published."
  echo
fi

# --- preflight ---------------------------------------------------------------

registry="$(npm config get registry)"
if [[ "$registry" != "https://registry.npmjs.org/" ]]; then
  echo "ERROR: npm registry is '$registry', expected https://registry.npmjs.org/" >&2
  echo "A private/proxy registry has no $SCOPE scope. Fix with:" >&2
  echo "  npm config set registry https://registry.npmjs.org/" >&2
  exit 1
fi

if ! whoami_out="$(npm whoami 2>&1)"; then
  echo "ERROR: not logged in to npm. Run 'npm login' first." >&2
  exit 1
fi

if [[ "$whoami_out" != "$EXPECTED_USER" ]]; then
  echo "ERROR: logged in as '$whoami_out', but the packages use the $SCOPE scope." >&2
  echo "npm only allows publishing to a scope matching your username (or an org" >&2
  echo "you belong to), so this would fail with a permission 404." >&2
  exit 1
fi

echo "Registry: $registry"
echo "User:     $whoami_out"
echo "Scope:    $SCOPE"
echo

# --- stage placeholders ------------------------------------------------------

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

make_placeholder() {
  local name="$1" desc="$2" dir="$workdir/$1"
  mkdir -p "$dir"
  cat > "$dir/package.json" <<JSON
{
  "name": "$SCOPE/$name",
  "version": "$PLACEHOLDER_VERSION",
  "description": "$desc",
  "license": "MIT OR Apache-2.0",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/denisix/agent-rdp.git"
  },
  "homepage": "https://github.com/denisix/agent-rdp#readme"
}
JSON
  printf '%s\n' "$dir"
}

# --- publish -----------------------------------------------------------------

declare -a failed=()

publish() {
  local name="$1" dir="$2"
  echo "--> $SCOPE/$name"
  if ! ( cd "$dir" && npm publish --access public $DRY_RUN ); then
    # Re-running after a partial failure is fine; an already-claimed name just
    # means there is nothing left to do for it.
    if npm view "$SCOPE/$name" version >/dev/null 2>&1; then
      echo "    already published, skipping"
    else
      failed+=("$SCOPE/$name")
    fi
  fi
  echo
}

for p in "${PLATFORMS[@]}"; do
  dir="$(make_placeholder "agent-rdp-$p" "Placeholder - native binary package for $SCOPE/agent-rdp")"
  publish "agent-rdp-$p" "$dir"
done

dir="$(make_placeholder "agent-rdp" "Placeholder - real release publishing shortly")"
publish "agent-rdp" "$dir"

# --- verify ------------------------------------------------------------------

if [[ -n "$DRY_RUN" ]]; then
  echo "Dry run complete - nothing was published."
  exit 0
fi

echo "Verifying all 7 names exist..."
missing=0
for name in "agent-rdp" "${PLATFORMS[@]/#/agent-rdp-}"; do
  version="$(npm view "$SCOPE/$name" version 2>/dev/null || echo MISSING)"
  printf '  %-44s %s\n' "$SCOPE/$name" "$version"
  [[ "$version" == "MISSING" ]] && missing=1
done

echo
if (( missing )); then
  echo "Some packages are missing. Re-run this script - already-published names are skipped." >&2
  exit 1
fi

if (( ${#failed[@]} )); then
  echo "Publishes reported errors: ${failed[*]}" >&2
  exit 1
fi

echo "All 7 names claimed."
echo
echo "Next (step 4): configure trusted publishing for each package at"
echo "  https://www.npmjs.com/package/$SCOPE/<name>  ->  Settings -> Trusted Publisher"
echo "    Organization or user: denisix"
echo "    Repository:           agent-rdp"
echo "    Workflow filename:    release-please.yml"
echo "    Environment:          (leave blank)"
echo
echo "Then merge the release PR. Do not merge before step 4 - the publish job"
echo "will fail authentication."
