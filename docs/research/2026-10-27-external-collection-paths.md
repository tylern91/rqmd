# Collection paths outside the project (2026-10-27)

[← Research notes](README.md)

Design and threat model for issue #148: how a project-local `.rqmd/` index should treat a collection whose path resolves outside the project directory. No code changes here. Labels: **Fact** is a line read or a command run in the session (rqmd 0.32.1 on a scratch directory), **Inference** is reasoning from facts, **Open question** needs a decision.

## Threat model

- **Who controls the index file.** A project-local `.rqmd/index.sqlite` is picked up implicitly whenever it exists in the current directory. Anyone who can get that file into a directory the user later runs rqmd in controls its `store_collections` rows: `path`, `pattern`, `ignore_patterns`, `allow_hidden`, `max_file_bytes` and `update_command`. The realistic route is a cloned or downloaded repository that commits `.rqmd/`.
- **What it can cause.** (1) Reading: the first `rqmd update`, `rqmd watch` or MCP `refresh: true` walks the named directory and copies matching UTF-8 files into the project's index. (2) Exfiltration: a later `search`, `get`, `query` or MCP tool call returns that content, so it can reach a terminal, a log or an agent's context; the MCP server is the realistic sink. (3) Code execution through `update_command`, which is already gated (below). Nothing in rqmd sends the content off the machine by itself.
- **Not in scope.** Content an attacker writes into the index (prompt injection through results) is a separate problem. An attacker who can already run commands as the user needs nothing here. `--index-dir` and the global index are chosen by the user.

### Current behavior

- Fact: the project-local index is chosen by `.rqmd/index.sqlite` existing in the current directory and tagged `IndexSource::ProjectLocal` (`crates/rqmd-cli/src/store.rs:18-25`, `:37-40`).
- Fact: `hooks_allowed` returns true for `Explicit` and `Global` and the flag value for `ProjectLocal` (`store.rs:54-58`). `rqmd update` passes it (`main.rs:496`) and `update_one_collection` skips the hook with a warning when it is false (`commands/index.rs:1057-1060`). `refresh_allowed` reuses that rule for MCP `refresh: true` (`store.rs:65-67`, `main.rs:529`). Refresh and watch never run a hook (`commands/refresh.rs:69-75`, `commands/watch.rs:145-151`).
- Fact: the only place a collection root is taken from the stored row for a disk walk is `let dir = Path::new(&col.path)` (`commands/index.rs:1040`). It is not canonicalized or compared with anything; the only check is `dir.exists()` (`:1041-1049`). The walk takes the pattern, hidden-file and size settings from the same row (`:1124-1129`, `WalkOptions::for_collection(col)`), and `document::prepare` reads each file (`:1166`, `document.rs:351`).
- Fact: the symlink guard compares each file's real path with the canonical collection root (`document.rs:200`, `:299-303`). The root is the stored path, so a root outside the project is its own boundary and the guard does not constrain it.
- Fact: `collection add` canonicalizes at add time (`commands/collection.rs:75-77`), so a relative path or symlinked root is stored resolved. A row written directly into the database is not.
- Fact: `rqmd watch` is passed no `IndexSource` (`main.rs:499-506`), so on a project-local index it walks every collection with no gate.
- Fact: `get` reads stored document text (`db::get_document_raw`, `commands/get.rs:56-67`) and only builds a path string from the root (`:71-74`). So `update`, `watch` and MCP refresh are the three entry points that read outside files; `search`, `get`, `query` and the MCP read tools serve what those already copied.
- Fact, reproduced with rqmd 0.32.1: `rqmd init` in `proj/`, `collection add ../outside --name ext`, edit `outside/secret.md`, then `rqmd update` with no flags printed `Indexed: 0 new, 1 updated, 0 unchanged, 0 removed`, and `rqmd search` returned `rqmd://ext/secret.md`. With the stored path edited to the relative `../linkt`, and to `notes`, a symlink inside the project to `../linkt`, `rqmd update --full` also walked the target (`1 unchanged`) and `rqmd search` returned its content. A nonexistent absolute path printed `WARN: directory not found` and indexed nothing.
- Inference: because the walk settings come from the row, an attacker row can set `allow_hidden` and a broad `pattern` to reach dot-directories such as `~/.ssh`, limited to files the user can read, that decode as UTF-8 and fit `max_file_bytes`. Not run end to end.
- Inference: moving the project leaves absolute stored paths pointing at the old location, which prints the `directory not found` warning above; if something else now occupies that location, it is read as an outside path.
- Open question: how often projects commit `.rqmd/`. The attack needs a shipped index, and no data says it is common.

## Options

| Option | Effect | Cost | Verdict |
|---|---|---|---|
| A. Do nothing | Keeps today's behavior | None; the exposure above stays | Rejected |
| B. Warn only | Names the collection and path, still walks it | Smallest, but the warning comes after the content is already copied | First step only |
| C. Skip by default, explicit opt-in | Skips collections whose canonical path is outside the project root on a project-local index, warns, and `--allow-external-paths` restores the walk | Breaks users who keep a project-local index over shared notes; one flag fixes it | Recommended end state |
| D. Reuse `--run-hooks` as a general trust switch | One flag for hooks and paths | Couples two decisions: trusting a hook to refresh notes is not trusting reads of `~` | Rejected |

## Recommendation

Approved direction: warn for one release, then skip by default, with an explicit opt-in flag. No behavior change ships with this note.

1. **Release N (warn).** On a project-local index, `rqmd update`, `rqmd watch` and MCP refresh print one warning per external collection, still walk it, and name `--allow-external-paths` as the way to silence it.
2. **Release N+1 (skip).** Those commands skip an external collection with `WARN: skipping collection '<name>' (<path>): outside the project directory; pass --allow-external-paths to index it`. The run continues with the other collections and ends as it does for a missing directory (`clean: false`, `commands/index.rs:1041-1049`). Documents already in the index are not pruned and stay searchable.
3. **Flag.** `--allow-external-paths` on `rqmd update`, `rqmd watch` and `rqmd mcp`. It applies to one invocation and is not stored in the index. A daemon keeps the decision of the command that started it, as `--no-refresh` does (`commands/mcp.rs:141-143`), because its child gets an explicit `--index-dir`.
4. **Scope.** `IndexSource::ProjectLocal` only. `Explicit` and `Global` are unchanged, matching `hooks_allowed`, so `rqmd --index-dir .rqmd update` stays an escape hatch.
5. **Outside the project.** The project root is the canonical form of the current directory where `.rqmd/` was found. A collection is external when `canonicalize(Path::new(&col.path))` does not start with it. A relative stored path is resolved against that directory first, a symlinked root is judged by its target, and a path that does not exist keeps today's `directory not found` warning. Inference: after a project move, an absolute path written before the move is external or missing.
6. **Surfaces.** `collection list`, `collection show` and `doctor` print the stored path today (`commands/collection.rs:241`, `:342`, `commands/index.rs:1354`) and gain an `external` marker. `status` prints no collection path today, so it gets only a count of external collections. The new warnings replace control characters in the path, which comes from the attacker's index; `document.rs` does this for excluded-folder names (`excluded_dirs_clause`).
7. **Migration.** Release N changes nothing for existing indexes except the warning. In N+1 a user with an intentional external collection runs `rqmd update --allow-external-paths`, or moves it to `--index-dir` or the global index. `CHANGELOG.md`, `docs/CLI.md`, `docs/MIGRATING.md` and `SECURITY.md` ("Update hook trust") get a matching entry.

## Open questions

- Whether `rqmd mcp --allow-refresh` should imply `--allow-external-paths`. Recommendation: no, so accepting refreshes and accepting outside reads stay separate decisions.
- Whether a skipped collection should make the run exit non-zero. Recommendation: no, match the missing-directory case.
- Which directory is the project root when `.rqmd` is itself a symlink. Recommendation: the directory containing it, not the link target.

## Proposed follow-up issue

**Title:** `feat(security): skip project-local collections outside the project unless --allow-external-paths`

**Body:**

Implements the decision in `docs/research/2026-10-27-external-collection-paths.md` (design issue #148). Two stages, one release apart.

Stage 1 (warn):
- [ ] On a project-local index (`IndexSource::ProjectLocal`), `rqmd update`, `rqmd watch` and MCP refresh print one warning per collection whose canonical path is outside the canonical current directory, naming the collection and path, and still index it.
- [ ] Explicit `--index-dir` and global indexes print nothing new.
- [ ] Control characters in the printed path are replaced.

Stage 2 (skip, the release after):
- [ ] The same commands skip an external collection, warn, continue with the rest, and do not prune its documents.
- [ ] `--allow-external-paths` on `update`, `watch` and `mcp` restores the walk for that invocation; a daemon keeps its starter's decision.
- [ ] Relative stored paths, symlinked roots and nonexistent paths behave as in the design, each with an acceptance test.
- [ ] `collection list`, `collection show` and `doctor` mark an external collection; `status` reports a count.
- [ ] `docs/CLI.md`, `docs/MIGRATING.md`, `SECURITY.md` and `CHANGELOG.md` are updated.

Out of scope: the `--run-hooks` rule, the in-tree symlink guard, and the trust model of `--index-dir` and the global index.
