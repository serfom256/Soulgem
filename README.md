<p align="center">
  <img src="icon/soulgem.png" alt="Soulgem" width="150">
</p>


### **Soulgem** is a fast version control for big binary files, built for keeping track of installed game mods.

Snapshot your mod folder before installing something new and roll back in seconds if it breaks your game. Soulgem handles tens of thousands of files and gigabytes of data: unchanged files are never stored twice, restored files share disk space with the store where the filesystem allows it (APFS, Btrfs, XFS, ReFS), and old versions are compressed automatically.

---

### Supported commands:

- **init** - init Soulgem root in the current directory
- **commit** "message" - commits the current state of the path (the message is optional)
- **lc** - get list of all commits
- **diff** - show changed files: since the last commit, `diff <commit>` against a commit, or `diff <commit> <commit>` between two commits
- **checkout** - rollback to commit by the commit hash (`checkout <commit> <path>` restores only that file or folder, `-f` discards uncommitted changes)
- **verify** - check that all stored files are intact

---
**Soulgem** commits the full content of files, every file is stored once and history is compressed automatically.
Commits can be referenced by hash prefix (at least 4 characters), `HEAD` or `HEAD~N`. Files matching patterns in `.soulgemignore` are not tracked.
