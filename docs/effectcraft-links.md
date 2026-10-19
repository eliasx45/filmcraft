# EffectCraft composition links

This is native FilmCraft ↔ EffectCraft linking through automatic cached rendering. It does not
implement Adobe Dynamic Link or a live per-frame server. The first implementation targets the
installed macOS apps and saved, visual-only compositions.

```text
FilmCraft video clips → editable EffectCraft project → rendered cache → same FilmCraft timeline item
                              save again ────────────────────┘
```

## Use

1. Choose **File ▸ Import ▸ Link EffectCraft Composition…**, browse to a saved `.ecproj`, load its
   compositions and select one by name. The renderer setup section accepts an absolute path to
   a compatible `effectcraft-cli`; FilmCraft remembers it in its own preferences.
2. Or select video clips and choose **Clip ▸ EffectCraft ▸ Replace with EffectCraft Composition…**.
   As Premiere does with After Effects, when EffectCraft is running with a saved project open (its
   control channel, `EFFECTCRAFT_CONTROL_PORT` or 9877), the selection becomes a new composition in
   that project and EffectCraft saves it; nothing asks for a file. Otherwise choose a **new**
   `.ecproj` filename. The original clips stay in place until the composition and its first render
   are ready. One Undo restores them. Original audio stays in FilmCraft. An import EffectCraft
   refuses is undone there.
3. **Window ▸ EffectCraft Links** shows status, opens the source, relinks a moved project, and
   provides Bake / Restore, Refresh and Cancel controls.
4. Edit the source in EffectCraft and save. FilmCraft scans in the background, renders changed
   compositions, and keeps the last completed picture visible until the update succeeds.

Save As does not redirect an existing link: explicitly Relink to use the new file. Composition
identity is its numeric item ID inside the referenced project, not its display name.

## What is preserved

- Timeline placement and source trims belong to FilmCraft clips, independently of generated movies.
- The visual handoff uses the existing OpenTimelineIO conversion, retaining editable layers and
  source media. FilmCraft tags its source-time keyframes; compatible EffectCraft importers preserve
  their timing through trims and speed changes while retaining legacy import behavior.
- Newly created handoff projects use 8-bit color and linear-light layer blending. Existing EffectCraft projects keep
  their authored color settings.
- Generated movies are full-resolution ProRes 4444 with alpha and no audio. The worker bypasses
  saved Movie Default templates, so draft frame rates, crop/resize presets and proxies cannot reduce
  the link output. Half-resolution previews
  do not reduce final export quality.
- Bake waits for a verified render, then suspends automatic updates while retaining a restorable link.
  Undoing a replacement or bake does not delete the editable EffectCraft project.

## Failure and export behavior

Rendering and filesystem scans run off the UI thread. Only one linked render runs at a time across
preview and export; newer saves invalidate an in-flight render. Cancellation stops the child process.
The source reference is rechecked before a worker result is applied, and opening another project
cancels the previous document's work.

Cache files are immutable. Each cache entry records source project bytes, footage size/mtime,
renderer identity and output identity. Failed updates retain the last completed preview with a
visible error. The renderer must advertise link protocol 1; incompatible binaries are rejected
instead of guessing composition identity or animation timing.

Final export prepares the links used by its sequence (including nested sources), waits for current
full-quality movies, and rechecks the set before exporting its immutable snapshot. A missing source,
deleted composition, failed render or incompatible renderer fails export instead of silently using
an old picture. Later edits after export preparation belong to the next export. Browser builds cannot
start the native worker and refuse exports using unbaked links; bake them on desktop first.

Project publication during replacement never overwrites an existing destination. If the timeline
changes while the handoff is running, its original clips remain untouched. A newly created editable
project may remain on disk after cancellation or a render failure, so it can be recovered or linked
manually.

## Bounds and current limitations

- Saved edits only; no updates from unsaved EffectCraft state and no real-time playback guarantee.
- Linked rendering currently caches the whole composition. Long compositions cost more time and disk.
- Visual-only handoff. Audio synchronization and audio editing are separate features.
- Conversion warnings/missing media abort replacement. Clip effects other than Motion, Opacity and
  speed, and effect masks, are left out (EffectCraft can't import them yet): the replacement shows
  a warning naming them, in the status bar and in EffectCraft Links. Graphics, multicam,
  variable-speed animation, footage reinterpretation, and transitions touching selected clips need
  an appropriate render/conversion first. Intervening unselected video clips must be
  included when needed to preserve stacking order.
- Paths are absolute. Moving projects requires Relink; packaging both projects and their assets is
  separate from linking. The source dependency scan conservatively watches all footage/proxies.
- Local-file dependency change detection uses size and modification time; project bytes are compared
  completely. Network filesystems and multi-machine transfers have not been validated.
- One render process, four Rayon threads, 256 active links, 16 pending imports, a 64 MiB project
  limit, 16 GiB per rendered movie, and a 32 GiB cache budget with space reserved before rendering.
  Full caches produce an actionable error; old cache folders are not deleted automatically because
  saved projects or undo history may still reference them.
- Synthetic checks are not Adobe-parity or real-project acceptance tests.

## Agent interface

All operations use engine commands. The renderer path lives in application preferences, never in a
project supplied by another person.

```json
{"command":"link.configure","params":{"executable":"/absolute/path/effectcraft-cli"}}
{"command":"link.import","params":{"path":"/absolute/path/titles.ecproj","composition":7,"wait":true}}
{"command":"link.replace","params":{"path":"/absolute/path/new-composition.ecproj"}}
{"command":"link.replace","params":{"open":true,"port":9877}}
{"command":"link.status","params":{}}
{"command":"link.relink","params":{"item":12,"path":"/absolute/path/moved.ecproj","composition":7}}
{"command":"link.bake","params":{"item":12}}
{"command":"link.restore","params":{"item":12}}
{"command":"link.refresh","params":{}}
{"command":"link.cancel","params":{}}
```

Import returns a request ID unless `wait:true` is used. Replacement is asynchronous. Status contains
request/item entries with ready, updating, baking, cancelled or error state. UI controls expose
`effectcraftLink.*` and `effectcraftLinks.*` automation IDs. Configuration is also available as
`general.effectcraftRenderer` through the normal preferences commands, or as the initial
`FILMCRAFT_EFFECTCRAFT_CLI` environment variable. Without an override, FilmCraft checks
`<application data directory>/EffectCraft Renderer/effectcraft-cli` before searching `PATH`.

## Verification

Focused synthetic tests cover save/reopen, undo, bake/restore, invalid arguments, changed sources,
missing compositions, incompatible schema, active-worker cancellation, save supersession, export
failure, and full-quality export after reduced previews. Native integration tests require a built,
compatible CLI and are opt-in:

```sh
FILMCRAFT_EFFECTCRAFT_TEST_CLI=/absolute/path/effectcraft-cli cargo test -p filmcraft-engine --lib linked_ -- --include-ignored
FILMCRAFT_EFFECTCRAFT_TEST_CLI=/absolute/path/effectcraft-cli cargo test -p filmcraft-ui-egui --test composition_links_ui -- --include-ignored
```

The UI tests drive menu commands and controls and can write screenshots with `FILMCRAFT_UI_SHOTS`.
The opt-in `measure_saved_link_preview_and_full_quality_export` test writes
`target/fixtures/dynamic-link/benchmark.json`. It measures a simple generated 1080p scene, not a
production edit or desktop presentation frame rate. Repository-wide gates remain a separate check.
