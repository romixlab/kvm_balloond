# Changelog

## [Unreleased]

### Fixed

- The balloons no longer swing with a busy guest: alma-1b's changed 323 times in 16 minutes during a build,
  every 2 s between 10.9G and 12.1G. Growing and shrinking for a short host still go through at once. Idle
  memory goes back only after the plan has wanted it for `shrink_settle` (30 s), to the highest target seen in
  that time. A guest whose balloon is still moving is left alone until it gets there or `settle_timeout` (20 s)
  passes.

### Changed

- `min_change` defaults to 512M (was 64M), and it is also how close a balloon must be to its target.
