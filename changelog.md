## v1.8.5

Changes since v1.8.4:
* fix: make clippy happy
* refactor(mount): implement subdirectory injection for overlayfs
* fix(planner): skip overlay mount on symlinked partitions
* fix: make clippy happy
* Fix OverlayFS shadowing issue by reverting to legacy mount syscall
* overlayfs: refactor add_try_umount aka send_unmountable
* chore(release): bump version to v1.8.4 [skip ci]