# Changelog

## [Unreleased]

## [0.1.1] - 2026-11-18

### Added

- Guests with passed-through devices (VFIO) can lend memory too, through a virtio-mem device: a guest with one is
  planned like any other, and its memory is set with `virsh update-memory-device --requested-size --live` in whole
  blocks instead of the balloon. The plan starts from what the device has plugged (an unplug takes the guest time),
  and the balloon stays for the guest's stats only. A VFIO guest without virtio-mem is still left alone.

## [0.1.0] - 2026-11-14

### Changed

- `min_change` defaults to 512M (was 64M), and it is also how close a balloon must be to its target.

### Fixed

- The balloons no longer swing with a busy guest: alma-1b's changed 323 times in 16 minutes during a build,
  every 2 s between 10.9G and 12.1G. Growing and shrinking for a short host still go through at once. Idle
  memory goes back only after the plan has wanted it for `shrink_settle` (30 s), to the highest target seen in
  that time. A guest whose balloon is still moving is left alone until it gets there or `settle_timeout` (20 s)
  passes.
