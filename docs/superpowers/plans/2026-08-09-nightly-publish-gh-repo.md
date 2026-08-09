# Nightly Publish GH_REPO Fix

> **For agentic workers:** Execute this plan task-by-task. Steps remain unticked by repository
> convention.

**Goal:** Stop the Nightly publish job from requiring a local git checkout when creating the draft
staging release, without changing the read-only build job or the seven-asset transaction.

- [ ] Add a contract that requires the publish job to set `GH_REPO` for repository-scoped `gh`
      commands when no checkout is present.
- [ ] Set `GH_REPO: ${{ github.repository }}` on the publish job so `gh release create` does not
      probe `.git` remotes.
- [ ] Record the 0.1.48/0.1.49 automated failures, the 0.1.50 build-then-manual-promote recovery,
      and the verified public asset digests in `handoff.md`.
- [ ] Run focused release/security tests, `git diff --check`, and land the fix on `develop`.
