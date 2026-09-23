#!/bin/sh
# Enable the publish-hygiene pre-commit hook repository-locally.
# Run once after cloning / after the hook files appear:
#   sh scripts/setup-hooks.sh
#
# Why this is needed at all: `core.hooksPath` lives in .git/config, which is
# NOT cloned. Committing scripts/git-hooks/pre-commit gives the repo the hook
# *file*, but every fresh clone still has zero enforcement until someone runs
# this. The CI job (.github/workflows/publish-hygiene.yml) is the half that
# cannot be skipped; this is the fast local half.
git config core.hooksPath scripts/git-hooks
echo "core.hooksPath -> scripts/git-hooks (publish-hygiene pre-commit active)"
