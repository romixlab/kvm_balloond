# kvm_balloond

Lends memory between the KVM guests of one host through their virtio balloons, the way ESXi's reservations
and shares or Hyper-V's Dynamic Memory do, for plain libvirt hosts.

- **A floor and a ceiling per guest.** The floors are guaranteed: plan them so they fit in the host's RAM
  minus its reserve, and the host itself never runs out.
- **The balloon follows what the guest uses.** An idle guest's balloon inflates down to what it uses plus
  some headroom, so the memory goes to the host and to other guests, and the guest *sees* less RAM. A guest
  that runs short (low MemAvailable, swapping in) grows again when the host has room. When the host is short,
  guests are cut first to what they can't drop, then to their floors, by shares. A guest cut below what it
  uses handles it itself: page cache first, then its swap, then its own OOM killer. The host's OOM killer
  never has to pick a whole VM (and QEMU gets `oom_score_adj -900` to be sure).
- **Reacts at once:** a PSI trigger on `/proc/pressure/memory` wakes it the moment the host stalls on memory;
  otherwise a round every 2 s. RHEL and Alma kernels build PSI in but switch it off
  (`CONFIG_PSI_DEFAULT_DISABLED=y`): boot them with `psi=1`, or the daemon runs on the timer alone.
- **Passed-through devices are left alone:** a guest with a PCI/mdev `<hostdev>` (VFIO: a GPU, a USB
  controller) has its RAM pinned for DMA, so a balloon can't take any; it is never touched.
- **No balloon `autodeflate`**: a guest under pressure would take its memory back past the host.

Works best with free page reporting (`<memballoon model='virtio' freePageReporting='on'/>`) and something
that drops cold page cache inside the guests, so a guest's "uses" means its hot set: `dist/cache-trim.timer`
asks the guest kernel every minute to drop its inactive file pages beyond 1 GiB (`memory.reclaim` with
`swappiness=0`: the LRU knows which pages are cold). DAMON_RECLAIM was tried first and is too coarse for this:
one access anywhere in a ~26 MB region keeps the whole region warm.

## Run

```sh
cargo build --release   # or: --target x86_64-unknown-linux-musl for a static binary
sudo install -m755 target/release/kvm_balloond /usr/local/bin/
sudo install -m644 dist/kvm_balloond.service /etc/systemd/system/
sudo cp dist/kvm_balloond.toml /etc/kvm_balloond.toml   # optional: every key has a default
kvm_balloond --once    # the guests and the plan, changes nothing
sudo systemctl enable --now kvm_balloond
```

It talks to libvirt through `virsh -c qemu:///system` (`domstats --balloon`, `dumpxml`, `dommemstat
--period`, `setmem --live`), so it needs root or the `libvirt` group; setting QEMU's `oom_score_adj` needs
root.

## The plan, per round

For each running guest with fresh balloon stats (all clamped to its `[floor, ceiling]`):

| level | value |
|---|---|
| comfortable | MemTotal - MemFree (hot cache included) + `headroom`; + `grow_step` above its size when short |
| tight | MemTotal - MemAvailable (what it can't drop) + `tight_headroom` |
| floor | guaranteed |

Budget = host MemAvailable - `reserve` + what the managed guests' QEMUs hold now. Comfortable for all if it
fits; else tight for all and the rest by shares up to comfortable; else floors and the rest by shares up to
tight. Shrinking goes at most `shrink_step` a round; changes under `min_change` are skipped.

Damping ([src/damp.rs](src/damp.rs)): a busy guest's numbers wobble every round, so the plan isn't set as it
comes. Growing goes through at once, and so does shrinking because the host is short. Idle memory goes back to
the host only after the plan has wanted it for `shrink_settle` (30 s), and then to the highest target seen in
that time. After a change, a guest is left alone until its balloon got there (within `min_change`), or for
`settle_timeout` (20 s) at most.

## License

MIT
