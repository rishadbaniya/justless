# FAQ

**Where is the output stored?**
In `.justless/` at the project root. Each run has a directory with its exact `stdout`, `stderr` and a `record.json`. The same directory holds the code index, retired tool results and recent activity. On Unix it is created readable only by your user. It holds its own `.gitignore`, so Git ignores it even before `justless init` adds it to the project's `.gitignore`. Nothing is deleted automatically. You can delete the directory at any time: the index is rebuilt on the next query, and older run IDs stop resolving. Captured output can include secrets that a command printed, so keep it private. `justless run --raw -- COMMAND` runs a command without recording it.

Routed searches skip the archive: `grep` and `ack` get an option that excludes `.justless/`, as do `rg` and `ag` when they search hidden files. A listing such as `find .` can still include the files under `.justless/runs/`.

**How do I turn it off?**
For one command, run `justless run --raw -- COMMAND`. Hooks leave a command that already runs through `justless run` unchanged, and `--raw` streams its output untouched. For a project, run `justless init --remove`, then start a new agent session. Add `--dry-run` first to see each change, and `--purge` to delete `.justless/` as well. [Removing Justless](reference.md#remove-justless-from-a-project) explains what it restores. To remove it everywhere, do that in each project, then delete the `justless` binary from the install prefix, `~/.cargo/bin` by default.
