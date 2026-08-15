#!/usr/bin/env bash
# Sourced by propose.sh and promote.sh — the one copy of the rule that
# titles a promotion. A push that carries a version bump is a release, and
# its pull request (and the squash commit main gets) should say which one.
#
# Sets: $title
promotion_title() {
  git fetch -q origin main

  local version released
  version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
  released=$(git show origin/main:Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)

  if [ "$version" != "$released" ]; then
    title="release $version"
  else
    title="promote dev to main"
  fi
}
