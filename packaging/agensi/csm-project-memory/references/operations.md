# Recall, save, and hand off

Use an explicit absolute project path when working through an MCP server that might have started in another directory. Git subdirectories share their repository's project key; linked worktrees normally share the main repository key. Separate directories outside Git have separate keys.

## MCP tools

| Purpose | Tool and arguments |
| --- | --- |
| Selected continuity context | `csm_brief(project)` |
| Keyword search of saved memories | `csm_search(query, project, limit, kind)`; keep `all_projects` false |
| Recent saved memories | `csm_list(project, limit, kind)` |
| Version, database, counts | `csm_status(project)` |
| One durable memory | `csm_save(content, project, kind, tags, importance, source)` |
| Current handoff | `csm_checkpoint(summary, project, next, blockers, goal, goal_done)` |
| Authorized removal of a saved memory | `csm_delete(id, project)` |

These are argument summaries, not positional call syntax. `kind` is `note`, `lesson`, `decision`, or `fact`; `tags`, `next`, and `blockers` are arrays of strings; importance ranges from 1 to 5. Save requires `content`; checkpoint requires `summary`. Goal fields are optional and should be omitted unless authorized. There is no journal MCP tool; use the CLI for raw journal inspection.

Save a stable principle with its reason and evidence. For example, an authorized decision record can explain the chosen database and the constraint that led to it. Avoid duplicating entire prompts or a changing task checklist. Search for an existing relevant record before adding a near-duplicate. A checkpoint instead describes current state: completed work, what was actually tested, what remains unverified, blockers, and the next useful step. Recording a checkpoint does not authorize executing its proposed next step.

CSM does not automatically promote journal activity into durable lessons. A large event count can coexist with zero saved memories. Briefings select an active goal, latest checkpoint, session evidence, relevant failures, and saved memories within a size budget; they do not summarize every record. Use a meaningful checkpoint when the user wants a reliable handoff.

## CLI fallback

For default installation paths, replace the example project with the intended existing directory:

```powershell
$csmExe = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
$csmProject = 'C:\work\example-project'
& $csmExe status --project $csmProject
& $csmExe brief --project $csmProject --explain
& $csmExe search 'database decision' --project $csmProject --limit 5
& $csmExe journal --project $csmProject --limit 10
```

Only run the following writes when the user has authorized the corresponding memory or checkpoint; replace the example text with verified, relevant content:

```powershell
& $csmExe save 'Use SQLite for local persistence because this tool must run offline.' --kind decision --tags 'storage,offline' --project $csmProject
& $csmExe checkpoint 'Local persistence is implemented; unit tests passed. Fresh-session recall remains unverified.' --next 'Verify recall in a fresh project session.' --project $csmProject
```

CLI checkpoint accepts repeatable `--next` and `--blocker`. `--goal` and `--goal-done` are explicit goal changes. CLI search is keyword-based; failure to find a phrase is not proof that nothing was recorded. Inspect a bounded recent list or journal when needed. Avoid `--all` unless cross-project inspection is requested.

## Isolated smoke test

Run this only when the user requests a functional test. It creates a unique temporary project and database, and does not configure host hooks or use the production memory database. `$csmExe` must already point to the intended executable.

```powershell
$csmTestRoot = Join-Path $env:TEMP ('csm-smoke-' + [guid]::NewGuid().ToString('N'))
$csmTestProject = Join-Path $csmTestRoot 'project'
$csmTestDb = Join-Path $csmTestRoot 'smoke.db'
New-Item -ItemType Directory -Path $csmTestProject -Force | Out-Null
& $csmExe --db $csmTestDb save 'Smoke marker: violet-otter.' --kind fact --project $csmTestProject
if ($LASTEXITCODE -ne 0) { throw 'CSM smoke save failed.' }
& $csmExe --db $csmTestDb checkpoint 'The isolated smoke marker was saved.' --next 'Recall the marker.' --project $csmTestProject
if ($LASTEXITCODE -ne 0) { throw 'CSM smoke checkpoint failed.' }
& $csmExe --db $csmTestDb search 'violet-otter' --project $csmTestProject
if ($LASTEXITCODE -ne 0) { throw 'CSM smoke search failed.' }
& $csmExe --db $csmTestDb brief --project $csmTestProject --explain
if ($LASTEXITCODE -ne 0) { throw 'CSM smoke briefing failed.' }
Write-Output ('Isolated test files: ' + $csmTestRoot)
```

Verify the search returns the marker and the briefing includes the checkpoint and next step. Each command starts a new process, so this checks persistence and explicit recall. It does not test a real host session, MCP registration, automatic hook firing, or model consumption. Report those boundaries and the exact temporary directory. Do not silently erase unrelated temporary files or seed production memory to extend this test.
