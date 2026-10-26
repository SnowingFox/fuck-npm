---
title: Security and install policy
description: Audit dependencies, verify recorded content and configure install restrictions.
---

fnpm includes registry audit commands, recorded integrity verification, dependency script trust, minimum release age and a scanner integration. Each mechanism answers a different question.

## Audit advisories

```sh
fnpm audit
fnpm-lock audit --audit-level high
```

An audit depends on the configured registry's advisory service and its response. A successful response does not establish that the project has no vulnerabilities. Review advisories and project impact before applying upgrades.

## Verify stored content

```sh
fnpm-lock verify --integrity
```

This rehashes package content against the lock/store's recorded integrity data. The release installer separately verifies native archives with an embedded SHA-512 digest. Hash verification detects content mismatch; it does not decide whether published content is trustworthy.

## Restrict install behavior

Review [trusted lifecycle scripts](../lifecycle/) and [minimumReleaseAge](../configuration/). The scanner integration is part of the implementation and parity inventory. Scanner protocol and command edge cases still require testing with the scanner you intend to use. See the [inventory](https://github.com/SnowingFox/fuck-npm/blob/main/docs/PARITY.md) for implementation boundaries.

Use private-registry credentials through environment references in .npmrc. Avoid placing tokens in scripts, lockfiles, examples or committed configuration.
