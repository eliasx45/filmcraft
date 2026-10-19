//! Native EffectCraft render transport. A caller publishes an output only after this worker
//! succeeds; cancellation, changed sources and process failure never replace a good preview.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use web_time::Instant;

use filmcraft_project::linked_composition::LinkedComposition;
use serde_json::Value;

const MAX_PROJECT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPENDENCIES: usize = 100_000;
const SOURCE_CHANGED: &str = "EffectCraft source changed; render the latest save";
const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceVersion {
    /// Full project bytes catch same-size edits even on coarse-timestamp filesystems.
    pub project_bytes: Vec<u8>,
    pub files: BTreeMap<PathBuf, (u64, std::time::SystemTime)>,
}

fn file_stamp(path: &Path) -> Result<(u64, std::time::SystemTime), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    Ok((meta.len(), meta.modified().map_err(|e| format!("{}: {e}", path.display()))?))
}

/// Conservatively watches every footage/proxy file in the project, including image sequences.
/// Missing dependencies are an error: EffectCraft's missing-media slate is not a valid cache.
pub fn source_version(link: &LinkedComposition) -> Result<SourceVersion, String> {
    link.validate()?;
    let path = Path::new(&link.project_path);
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_PROJECT_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_PROJECT_BYTES {
        return Err("EffectCraft project exceeds the 64 MiB link limit".into());
    }
    let doc: Value = serde_json::from_slice(&bytes).map_err(|e| format!("EffectCraft project: {e}"))?;
    if !matches!(doc.get("schema").and_then(Value::as_u64), Some(1 | 2)) {
        return Err("unsupported EffectCraft project schema; update FilmCraft".into());
    }
    let items = doc.get("items").and_then(Value::as_object).ok_or("EffectCraft project has no items")?;
    let comp = items.get(&link.composition_id.to_string()).and_then(|v| v.get("kind")).filter(|v| v.get("type").and_then(Value::as_str) == Some("Comp"));
    if comp.is_none() {
        return Err(format!("EffectCraft composition {} is missing", link.composition_id));
    }
    let mut files = BTreeMap::new();
    let base = path.parent().ok_or("EffectCraft project has no parent directory")?;
    if items.len() > MAX_DEPENDENCIES {
        return Err("too many EffectCraft project items".into());
    }
    for item in items.values() {
        let footage = item.get("kind").filter(|v| v.get("type").and_then(Value::as_str) == Some("Footage"));
        let proxy = item.get("proxy").and_then(|v| v.get("footage"));
        for source in footage.into_iter().chain(proxy) {
            let paths = source
                .get("path")
                .and_then(Value::as_str)
                .into_iter()
                .chain(source.get("sequence").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str));
            for raw in paths.filter(|p| !p.is_empty()) {
                let dependency = base.join(raw);
                files.insert(dependency.clone(), file_stamp(&dependency)?);
                if files.len() > MAX_DEPENDENCIES {
                    return Err("too many EffectCraft media dependencies".into());
                }
            }
        }
    }
    Ok(SourceVersion { project_bytes: bytes, files })
}

/// Own the subprocess even during unwinding, so cancelling/closing a session cannot orphan it.
struct RenderChild(Child);

impl Drop for RenderChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Render to a caller-owned, fresh temporary directory. No shell, no user-supplied flags, no
/// in-place overwrite of existing media. The caller owns cache publication and cleanup.
pub fn render(executable: &Path, link: &LinkedComposition, expected: &SourceVersion, output: &Path, cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("EffectCraft render cancelled".into());
    }
    if output.exists() {
        return Err("EffectCraft render destination already exists".into());
    }
    if &source_version(link)? != expected {
        return Err(SOURCE_CHANGED.into());
    }
    let log_path = output.with_extension("render.log");
    let log = std::fs::OpenOptions::new().write(true).create_new(true).open(&log_path).map_err(|e| format!("render log: {e}"))?;
    let child = Command::new(executable)
        .env("RAYON_NUM_THREADS", "4")
        .arg("render")
        .arg("--link-output")
        .arg("--project")
        .arg(&link.project_path)
        .arg("--comp")
        .arg(link.composition_id.to_string())
        .arg("--out")
        .arg(output)
        .args(["--format", "prores", "--prores", "4444", "--channels", "rgba", "--audio", "off", "--resolution", "full", "--quality", "best"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", executable.display()))?;
    let mut child = RenderChild(child);
    let started = Instant::now();
    let mut checked = Instant::now();
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err("EffectCraft render cancelled".into());
        }
        if started.elapsed() > Duration::from_secs(3600) {
            return Err("EffectCraft render exceeded one hour; reduce the composition duration".into());
        }
        if std::fs::metadata(output).is_ok_and(|m| m.len() > MAX_OUTPUT_BYTES) {
            return Err("EffectCraft render exceeded the 16 GiB cache entry limit".into());
        }
        if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 1024 * 1024) {
            return Err("EffectCraft worker exceeded the diagnostic log limit".into());
        }
        if checked.elapsed() >= Duration::from_millis(500) {
            if &source_version(link)? != expected {
                return Err(SOURCE_CHANGED.into());
            }
            checked = Instant::now();
        }
        if let Some(status) = child.0.try_wait().map_err(|e| format!("EffectCraft worker: {e}"))? {
            if &source_version(link)? != expected {
                return Err(SOURCE_CHANGED.into());
            }
            if !status.success() {
                let mut diagnostic = String::new();
                if let Ok(file) = std::fs::File::open(&log_path) {
                    let _ = file.take(4096).read_to_string(&mut diagnostic);
                }
                return Err(format!("EffectCraft render failed ({status}): {}", diagnostic.trim()));
            }
            let (size, _) = file_stamp(output)?;
            if size == 0 || size > MAX_OUTPUT_BYTES {
                return Err("EffectCraft produced an empty or oversized render".into());
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> (PathBuf, LinkedComposition) {
        let dir = crate::media_test_util::tmp_dir(name);
        let path = dir.join("titles.ecproj");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"schema":2,"items":{"1":{"kind":{"type":"Comp"}},"2":{"kind":{"type":"Footage","path":"source.bin"}}}})).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("source.bin"), b"original").unwrap();
        (dir, LinkedComposition { project_path: path.to_string_lossy().into_owned(), composition_id: 1, baked: false })
    }

    #[test]
    fn detects_saved_project_and_dependency_changes() {
        let (dir, link) = fixture("linked-render-changes");
        let original = source_version(&link).unwrap();
        assert_eq!(original.files.len(), 1);
        std::fs::write(dir.join("source.bin"), b"changed footage").unwrap();
        let changed = source_version(&link).unwrap();
        assert_ne!(original, changed);
        let mut doc: Value = serde_json::from_slice(&changed.project_bytes).unwrap();
        doc["items"]["1"]["name"] = json!("New title");
        std::fs::write(&link.project_path, serde_json::to_vec(&doc).unwrap()).unwrap();
        assert_ne!(changed, source_version(&link).unwrap());
        std::fs::remove_file(dir.join("source.bin")).unwrap();
        assert!(source_version(&link).unwrap_err().contains("source.bin"));
    }

    #[test]
    fn refuses_missing_composition_and_future_schema() {
        let (_, mut link) = fixture("linked-render-invalid");
        link.composition_id = 999;
        assert!(source_version(&link).unwrap_err().contains("composition 999"));
        link.composition_id = 1;
        std::fs::write(&link.project_path, br#"{"schema":999,"items":{}}"#).unwrap();
        assert!(source_version(&link).unwrap_err().contains("schema"));
    }

    #[test]
    fn cancelled_or_obsolete_request_never_starts_renderer() {
        let (dir, link) = fixture("linked-render-cancel");
        let version = source_version(&link).unwrap();
        let output = dir.join("never.mov");
        let executable = dir.join("does-not-exist");
        assert!(render(&executable, &link, &version, &output, &AtomicBool::new(true)).unwrap_err().contains("cancelled"));
        std::fs::write(dir.join("source.bin"), b"newer source bytes").unwrap();
        assert!(render(&executable, &link, &version, &output, &AtomicBool::new(false)).unwrap_err().contains("latest save"));
        assert!(!output.exists());
    }
    #[test]
    #[ignore = "requires FILMCRAFT_EFFECTCRAFT_TEST_CLI pointing to a built native CLI"]
    fn real_effectcraft_alpha_render_matches_direct_frame() {
        let executable = PathBuf::from(std::env::var_os("FILMCRAFT_EFFECTCRAFT_TEST_CLI").expect("set the EffectCraft test CLI path"));
        let dir = crate::media_test_util::tmp_dir("linked-real-cli");
        let source = dir.join("source.ecproj");
        let create = Command::new(&executable)
            .args([
                "run",
                "comp.new",
                r##"{"name":"Link test","width":64,"height":64,"frameRate":24,"duration":0.25}"##,
                "layer.newSolid",
                r##"{"name":"Orange square","width":32,"height":32,"color":"#ff8000"}"##,
                "--empty",
                "--save-as",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(create.status.success(), "{}", String::from_utf8_lossy(&create.stderr));
        let opacity = Command::new(&executable).args(["set", "1", "4", "transform/opacity", "50", "--project"]).arg(&source).arg("--save").output().unwrap();
        assert!(opacity.status.success(), "{}", String::from_utf8_lossy(&opacity.stderr));
        // Saved draft export presets must never shrink or retime a linked composition.
        let presets = Command::new(&executable)
            .args([
                "run",
                "renderQueue.saveTemplate",
                r#"{"kind":"renderSettings","name":"Draft","params":{"resolution":"quarter","frameRate":12,"proxyUse":"all"}}"#,
                "renderQueue.saveTemplate",
                r#"{"kind":"outputModule","name":"Cropped","params":{"format":"prores","crop":{"enabled":true,"left":8},"resize":{"enabled":true,"width":16,"height":16}}}"#,
                "renderQueue.setTemplateDefault",
                r#"{"kind":"renderSettings","slot":"movie","name":"Draft"}"#,
                "renderQueue.setTemplateDefault",
                r#"{"kind":"outputModule","slot":"movie","name":"Cropped"}"#,
                "--project",
            ])
            .arg(&source)
            .arg("--save")
            .output()
            .unwrap();
        assert!(presets.status.success(), "{}", String::from_utf8_lossy(&presets.stderr));
        let link = LinkedComposition { project_path: source.to_string_lossy().into_owned(), composition_id: 1, baked: false };
        let version = source_version(&link).unwrap();
        let output = dir.join("cache.mov");
        let started = Instant::now();
        render(&executable, &link, &version, &output, &AtomicBool::new(false)).unwrap();
        eprintln!("EffectCraft 64x64, 6 frames: {:?}", started.elapsed());
        let direct = dir.join("direct.png");
        let result = Command::new(&executable)
            .args(["render-frame", "--project"])
            .arg(&source)
            .args(["--comp", "1", "--frame", "0", "--transparent", "--out"])
            .arg(&direct)
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let reference = image::open(&direct).unwrap().to_rgba8().into_raw();
        let media = filmcraft_codecs::open_bytes("cache.mov", std::fs::read(&output).unwrap().into()).unwrap();
        assert!(!media.info().has_audio());
        assert_eq!(media.info().frame_rate(), filmcraft_time::FrameRate::FPS_24);
        let frame = media.video_frame(filmcraft_media::FrameRequest::full(filmcraft_time::Tick::ZERO)).unwrap().to_rgba8();
        assert_eq!(frame.len(), reference.len());
        let mut max_color_error = 0;
        for (actual, expected) in frame.as_chunks::<4>().0.iter().zip(reference.as_chunks::<4>().0) {
            assert_eq!(actual[3], expected[3], "alpha mismatch");
            if expected[3] > 0 {
                for channel in 0..3 {
                    max_color_error = max_color_error.max(actual[channel].abs_diff(expected[channel]));
                }
            }
        }
        assert!(max_color_error <= 4, "ProRes round-trip max channel error: {max_color_error}");
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheManifest {
    link: LinkedComposition,
    files: BTreeMap<PathBuf, (u64, std::time::SystemTime)>,
    renderer: (PathBuf, (u64, std::time::SystemTime)),
    output: (u64, std::time::SystemTime),
}

/// A verified movie, ready for publication. The worker opens it before the session adopts it.
pub struct CachedRender {
    pub path: String,
    pub source: filmcraft_media::SharedSource,
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).map_err(|e| e.to_string())?.take(limit + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!("{} exceeds the size limit", path.display()));
    }
    Ok(bytes)
}

fn matches_cache(path: &Path, link: &LinkedComposition, version: &SourceVersion, executable: &Path) -> bool {
    let Some(dir) = path.parent() else { return false };
    let Ok(bytes) = read_bounded(&dir.join("manifest.json"), 32 * 1024 * 1024) else { return false };
    let Ok(manifest) = serde_json::from_slice::<CacheManifest>(&bytes) else { return false };
    manifest.link == *link
        && manifest.files == version.files
        && manifest.renderer.0 == executable
        && file_stamp(executable).is_ok_and(|stamp| stamp == manifest.renderer.1)
        && file_stamp(path).is_ok_and(|stamp| stamp == manifest.output)
        && read_bounded(&dir.join("source.ecproj"), MAX_PROJECT_BYTES).is_ok_and(|bytes| bytes == version.project_bytes)
}

fn fresh_cache_dir(root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cache directory: {e}"))?;
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        count = count.saturating_add(1);
        if count > 8192 {
            return Err("EffectCraft cache has too many entries; remove unused cache folders".into());
        }
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(entry.path()).map_err(|e| e.to_string())?.take(16) {
            total = total.saturating_add(file.map_err(|e| e.to_string())?.metadata().map_err(|e| e.to_string())?.len());
        }
    }
    if total > 16 * 1024 * 1024 * 1024 {
        return Err("EffectCraft cache is full; remove unused cache folders (32 GiB budget, 16 GiB reserved per render)".into());
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
    let dir = root.join(format!("render-{}-{time}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Reuse a verified full-quality cache, or render a fresh immutable movie. Files are never
/// overwritten, so previews/exports holding an earlier source cannot read half a new render.
pub fn cached_render(
    executable: &Path,
    link: &LinkedComposition,
    previous: Option<&str>,
    root: &Path,
    cancelled: &AtomicBool,
    services: &dyn crate::Services,
) -> Result<CachedRender, String> {
    for _ in 0..16 {
        match cached_render_once(executable, link, previous, root, cancelled, services) {
            Err(error) if error.starts_with(SOURCE_CHANGED) => continue,
            result => return result,
        }
    }
    Err("EffectCraft sources keep changing; finish saving before retrying the link".into())
}

fn cached_render_once(
    executable: &Path,
    link: &LinkedComposition,
    previous: Option<&str>,
    root: &Path,
    cancelled: &AtomicBool,
    services: &dyn crate::Services,
) -> Result<CachedRender, String> {
    // Preview and export share the same process budget. Waiting remains cancellable.
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err("EffectCraft render cancelled".into());
        }
        match GATE.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    type CacheIndex = BTreeMap<(String, u64, PathBuf), String>;
    static INDEX: std::sync::OnceLock<std::sync::Mutex<CacheIndex>> = std::sync::OnceLock::new();
    let mut index = INDEX.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let key = (link.project_path.clone(), link.composition_id, executable.to_path_buf());
    let indexed = index.get(&key).cloned();
    check_renderer(executable, root, cancelled)?;
    let renderer_stamp = file_stamp(executable)?;
    let version = source_version(link)?;
    let pool = crate::MediaPool::default();
    if let Some(path) = previous.into_iter().chain(indexed.as_deref()).find(|p| matches_cache(Path::new(p), link, &version, executable)) {
        let source = pool.probe_file(path, services).map_err(|e| e.to_string())?;
        return Ok(CachedRender { path: path.to_string(), source });
    }
    let dir = fresh_cache_dir(root)?;
    let output = dir.join("composition.mov");
    let result = (|| {
        render(executable, link, &version, &output, cancelled)?;
        let path = output.to_string_lossy().into_owned();
        let source = pool.probe_file(&path, services).map_err(|e| e.to_string())?;
        if !source.info().has_video() || source.info().has_audio() {
            return Err("EffectCraft must produce visual-only media".into());
        }
        if file_stamp(executable)? != renderer_stamp {
            return Err("EffectCraft renderer changed during rendering; retry with the updated renderer".into());
        }
        let manifest =
            CacheManifest { link: link.clone(), files: version.files, renderer: (executable.to_path_buf(), renderer_stamp), output: file_stamp(&output)? };
        std::fs::write(dir.join("source.ecproj"), version.project_bytes).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
        if bytes.len() > 32 * 1024 * 1024 {
            return Err("EffectCraft cache dependency manifest exceeds 32 MiB".into());
        }
        filmcraft_format::atomic_write(&dir.join("manifest.json"), &bytes).map_err(|e| e.to_string())?;
        Ok(CachedRender { path, source })
    })();
    if let Ok(cache) = &result {
        if index.len() >= 256 {
            index.pop_first();
        }
        index.insert(key, cache.path.clone());
    }
    if result.is_err() {
        // This exact directory was exclusively created by this call and never published.
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct CompositionChoice {
    pub id: u64,
    pub name: String,
}

/// Read a bounded composition list for the link picker. Call off the UI thread.
pub fn compositions(path: &Path) -> Result<Vec<CompositionChoice>, String> {
    let bytes = read_bounded(path, MAX_PROJECT_BYTES)?;
    let doc: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !matches!(doc.get("schema").and_then(Value::as_u64), Some(1 | 2)) {
        return Err("unsupported EffectCraft project schema; update FilmCraft".into());
    }
    let items = doc.get("items").and_then(Value::as_object).ok_or("EffectCraft project has no items")?;
    if items.len() > MAX_DEPENDENCIES {
        return Err("too many EffectCraft project items".into());
    }
    let mut choices = Vec::new();
    for (id, item) in items {
        if item.get("kind").and_then(|v| v.get("type")).and_then(Value::as_str) != Some("Comp") {
            continue;
        }
        let id = id.parse::<u64>().map_err(|_| "invalid composition id")?;
        if id == 0 {
            return Err("invalid composition id".into());
        }
        if choices.len() >= 4096 {
            return Err("composition picker supports at most 4096 compositions per project".into());
        }
        choices.push(CompositionChoice { id, name: item.get("name").and_then(Value::as_str).unwrap_or("Untitled composition").chars().take(256).collect() });
    }
    if choices.is_empty() {
        return Err("this EffectCraft project has no compositions".into());
    }
    Ok(choices)
}

/// Convert an interchange selection into a new editable project. Publication uses a hard link
/// so an existing destination can never be overwritten, including a file created during import.
/// The one top-level composition `file.importTimeline` made, refusing a renderer too old for
/// trimmed animation or an import with warnings or missing media.
fn imported_composition(result: &Value) -> Result<u64, String> {
    if result.get("supportsSourceTimeKeys").and_then(Value::as_bool) != Some(true) {
        return Err("EffectCraft renderer is too old for accurate trimmed animation handoff; update it (original clips kept)".into());
    }
    for field in ["warnings", "missing"] {
        let entries = result.get(field).and_then(Value::as_array).ok_or_else(|| format!("EffectCraft import response missing {field}; update renderer"))?;
        if !entries.is_empty() {
            return Err(format!("EffectCraft import reported {field}: {}; original clips kept", Value::Array(entries.clone())));
        }
    }
    let comps = result.get("comps").and_then(Value::as_array).ok_or("EffectCraft import returned no composition list")?;
    if comps.len() != 1 {
        return Err("EffectCraft import must return one top-level composition".into());
    }
    comps.first().and_then(Value::as_u64).ok_or_else(|| "invalid imported composition id".to_string())
}

pub fn import_timeline(executable: &Path, bytes: &[u8], destination: &Path, cancelled: &AtomicBool) -> Result<LinkedComposition, String> {
    if destination.exists() {
        return Err("EffectCraft destination already exists; choose a new project filename".into());
    }
    let parent = destination.parent().ok_or("EffectCraft destination needs a parent folder")?;
    let dir = fresh_cache_dir(&parent.join(".filmcraft-handoff"))?;
    let result = (|| {
        let timeline = dir.join("selection.otio");
        let project = dir.join("selection.ecproj");
        std::fs::write(&timeline, bytes).map_err(|e| e.to_string())?;
        let params = serde_json::json!({"path":timeline,"format":"otio"}).to_string();
        let mut command = Command::new(executable);
        // Match FilmCraft's linear-light layer blending without reinterpreting decoded footage.
        // Only this new empty handoff project changes; existing projects keep their look.
        command
            .args(["run", "file.projectSettings", r#"{"blendLinear":true,"bitDepth":32}"#, "file.importTimeline"])
            .arg(params)
            .args(["--empty", "--save-as"])
            .arg(&project)
            .arg("--json");
        let response = run_json_command(command, &dir, cancelled, Duration::from_secs(120))?;
        let result = response
            .get("result")
            .unwrap_or(&response)
            .as_array()
            .and_then(|results| results.iter().find(|entry| entry.get("command").and_then(Value::as_str) == Some("file.importTimeline")))
            .and_then(|entry| entry.get("result"))
            .ok_or("EffectCraft returned no timeline import result")?;
        let composition_id = imported_composition(result)?;
        source_version(&LinkedComposition { project_path: project.to_string_lossy().into_owned(), composition_id, baked: false })?;
        if cancelled.load(Ordering::Relaxed) {
            return Err("EffectCraft handoff cancelled; original clips kept".into());
        }
        std::fs::hard_link(&project, destination).map_err(|e| format!("could not publish {} without overwriting: {e}", destination.display()))?;
        Ok(LinkedComposition { project_path: destination.to_string_lossy().into_owned(), composition_id, baked: false })
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// The running EffectCraft app's control port: `EFFECTCRAFT_CONTROL_PORT`, else its default 9877.
pub fn effectcraft_control_port() -> u16 {
    std::env::var("EFFECTCRAFT_CONTROL_PORT").ok().and_then(|v| v.trim().parse::<u16>().ok()).filter(|p| *p != 0).unwrap_or(9877)
}

/// One JSON-lines connection to a running EffectCraft's control channel (127.0.0.1 only).
struct Control {
    reader: std::io::BufReader<std::net::TcpStream>,
    writer: std::net::TcpStream,
    next_id: u64,
}

const MAX_REPLY_BYTES: u64 = 64 * 1024 * 1024;

impl Control {
    fn connect(port: u16, reply_timeout: Duration) -> Result<Self, String> {
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let stream =
            std::net::TcpStream::connect_timeout(&address, Duration::from_millis(400)).map_err(|e| format!("EffectCraft is not open on port {port}: {e}"))?;
        stream.set_read_timeout(Some(reply_timeout)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        Ok(Self { reader: std::io::BufReader::new(stream), writer, next_id: 1 })
    }

    /// Runs one engine command in the open document and returns its result.
    fn execute(&mut self, command: &str, params: Value) -> Result<Value, String> {
        use std::io::{BufRead, Write};
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = serde_json::json!({"id": id, "method": "engine.execute", "params": {"command": command, "params": params}});
        let mut line = request.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).map_err(|e| format!("EffectCraft control: {e}"))?;
        let mut reply = String::new();
        (&mut self.reader).take(MAX_REPLY_BYTES).read_line(&mut reply).map_err(|e| format!("EffectCraft control: {e}"))?;
        let reply: Value = serde_json::from_str(reply.trim()).map_err(|e| format!("EffectCraft control reply: {e}"))?;
        if reply.get("id").and_then(Value::as_u64) != Some(id) {
            return Err("EffectCraft control reply did not match the request".into());
        }
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            let error = reply.get("error").and_then(Value::as_str).unwrap_or("request failed");
            return Err(format!("EffectCraft {command}: {error}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// The saved project open in the running EffectCraft, if any (After Effects-style handoff target).
/// A quick probe: `None` when EffectCraft isn't running, doesn't answer, or its project is unsaved.
pub fn open_effectcraft_project(port: u16) -> Option<String> {
    let mut control = Control::connect(port, Duration::from_secs(2)).ok()?;
    let summary = control.execute("project.summary", serde_json::json!({})).ok()?;
    summary.get("path").and_then(Value::as_str).filter(|p| !p.is_empty() && p.to_ascii_lowercase().ends_with(".ecproj")).map(str::to_owned)
}

/// Replace with EffectCraft Composition into the project open in the running EffectCraft, as
/// Premiere does with After Effects: the selection becomes a new composition there and the project
/// is saved (the link renders from the saved file). A refused import is undone in EffectCraft.
pub fn import_timeline_live(port: u16, bytes: &[u8], cancelled: &AtomicBool) -> Result<LinkedComposition, String> {
    let mut control = Control::connect(port, Duration::from_secs(120))?;
    let summary = control.execute("project.summary", serde_json::json!({}))?;
    let project = summary
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or("the project open in EffectCraft has never been saved; save it there first (original clips kept)")?
        .to_owned();
    let parent = Path::new(&project).parent().ok_or("EffectCraft project needs a parent folder")?;
    let dir = fresh_cache_dir(&parent.join(".filmcraft-handoff"))?;
    let result = (|| {
        let timeline = dir.join("selection.otio");
        std::fs::write(&timeline, bytes).map_err(|e| e.to_string())?;
        let result = control.execute("file.importTimeline", serde_json::json!({"path": timeline, "format": "otio"}))?;
        let composition_id = match imported_composition(&result) {
            Ok(id) if !cancelled.load(Ordering::Relaxed) => id,
            refused => {
                // Take the half-made composition back out; `expectRevision` keeps a later user edit safe.
                if let Some(revision) = control.execute("project.summary", serde_json::json!({})).ok().and_then(|s| s.get("revision").cloned()) {
                    let _ = control.execute("edit.undo", serde_json::json!({"expectRevision": revision}));
                }
                return Err(refused.err().unwrap_or_else(|| "EffectCraft handoff cancelled; original clips kept".into()));
            }
        };
        control.execute("file.save", serde_json::json!({}))?;
        let link = LinkedComposition { project_path: project.clone(), composition_id, baked: false };
        source_version(&link)?;
        Ok(link)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(all(test, unix))]
mod worker_lifecycle_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn failed_worker_and_incompatible_protocol_are_actionable_errors() {
        let dir = crate::media_test_util::tmp_dir("link-worker-errors");
        let executable = dir.join("worker");
        let project = dir.join("source.ecproj");
        std::fs::write(&project, br#"{"schema":2,"items":{"1":{"kind":{"type":"Comp"}}}}"#).unwrap();
        let link = LinkedComposition { project_path: project.to_string_lossy().into_owned(), composition_id: 1, baked: false };
        let version = source_version(&link).unwrap();
        std::fs::write(&executable, b"#!/bin/sh\necho 'renderer failed' >&2\nexit 7\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = render(&executable, &link, &version, &dir.join("failed.mov"), &AtomicBool::new(false)).unwrap_err();
        assert!(error.contains("renderer failed"), "{error}");
        assert!(!dir.join("failed.mov").exists());
        std::fs::write(&executable, b"#!/bin/sh\nprintf '%s\n' '{\"protocol\":99,\"compositionIds\":true,\"sourceTimeKeys\":true}'\n").unwrap();
        let cache = dir.join("cache");
        std::fs::create_dir(&cache).unwrap();
        let good = cache.join("last-good.mov");
        std::fs::write(&good, b"keep previous preview").unwrap();
        assert!(check_renderer(&executable, &cache, &AtomicBool::new(false)).unwrap_err().contains("incompatible"));
        assert_eq!(std::fs::read(&good).unwrap(), b"keep previous preview");
    }

    #[test]
    fn running_worker_stops_on_cancel_or_newer_save() {
        for newer_save in [false, true] {
            let dir = crate::media_test_util::tmp_dir("link-worker-lifecycle");
            let executable = dir.join("worker");
            std::fs::write(&executable, b"#!/bin/sh\n: > \"$0.started\"\nwhile :; do :; done\n").unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            let project = dir.join("source.ecproj");
            std::fs::write(&project, br#"{"schema":2,"items":{"1":{"kind":{"type":"Comp"}}}}"#).unwrap();
            let link = LinkedComposition { project_path: project.to_string_lossy().into_owned(), composition_id: 1, baked: false };
            let version = source_version(&link).unwrap();
            let cancel = Arc::new(AtomicBool::new(false));
            let _cleanup = CancelOnDrop(cancel.clone());
            let worker_cancel = cancel.clone();
            let output = dir.join("new.mov");
            let worker = std::thread::spawn(move || render(&executable, &link, &version, &output, &worker_cancel));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !dir.join("worker.started").exists() {
                assert!(Instant::now() < deadline, "worker did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            if newer_save {
                std::fs::write(&project, br#"{"schema":2,"items":{"1":{"name":"new save","kind":{"type":"Comp"}}}}"#).unwrap();
            } else {
                cancel.store(true, Ordering::Relaxed);
            }
            let error = worker.join().unwrap().unwrap_err();
            assert!(error.contains(if newer_save { "latest save" } else { "cancelled" }), "{error}");
            assert!(Instant::now() < deadline, "cancellation/source change must not wait for render completion");
        }
    }
}

fn run_json_command(mut command: Command, dir: &Path, cancelled: &AtomicBool, timeout: Duration) -> Result<Value, String> {
    let stdout_path = dir.join("result.json");
    let stderr_path = dir.join("command.log");
    let stdout = std::fs::File::create(&stdout_path).map_err(|e| e.to_string())?;
    let stderr = std::fs::File::create(&stderr_path).map_err(|e| e.to_string())?;
    let child = command.stdin(Stdio::null()).stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr)).spawn().map_err(|e| e.to_string())?;
    let mut child = RenderChild(child);
    let start = Instant::now();
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err("EffectCraft operation cancelled".into());
        }
        if start.elapsed() > timeout {
            return Err("EffectCraft operation timed out".into());
        }
        if [&stdout_path, &stderr_path].into_iter().any(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 1024 * 1024)) {
            return Err("EffectCraft diagnostic limit exceeded".into());
        }
        if let Some(status) = child.0.try_wait().map_err(|e| e.to_string())? {
            if !status.success() {
                return Err(format!("EffectCraft operation failed ({status}): {}", String::from_utf8_lossy(&read_bounded(&stderr_path, 1024 * 1024)?)));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    serde_json::from_slice(&read_bounded(&stdout_path, 1024 * 1024)?).map_err(|e| format!("EffectCraft response: {e}"))
}

fn check_renderer(executable: &Path, root: &Path, cancelled: &AtomicBool) -> Result<(), String> {
    type Versions = BTreeMap<PathBuf, (u64, std::time::SystemTime)>;
    static CHECKED: std::sync::OnceLock<std::sync::Mutex<Versions>> = std::sync::OnceLock::new();
    let mut checked = CHECKED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let stamp = file_stamp(executable)?;
    if checked.get(executable) == Some(&stamp) {
        return Ok(());
    }
    let dir = fresh_cache_dir(root)?;
    let mut command = Command::new(executable);
    command.args(["exec", "link.capabilities", "--empty", "--json"]);
    let response = run_json_command(command, &dir, cancelled, Duration::from_secs(15));
    let _ = std::fs::remove_dir_all(&dir);
    let response = response.map_err(|e| format!("EffectCraft renderer does not support composition linking; update it: {e}"))?;
    let capabilities = response.get("result").unwrap_or(&response);
    if capabilities.get("protocol").and_then(Value::as_u64) != Some(1)
        || capabilities.get("compositionIds").and_then(Value::as_bool) != Some(true)
        || capabilities.get("sourceTimeKeys").and_then(Value::as_bool) != Some(true)
        || capabilities.get("neutralRenderDefaults").and_then(Value::as_bool) != Some(true)
    {
        return Err("incompatible EffectCraft link protocol; update the renderer".into());
    }
    if checked.len() >= 32 {
        checked.pop_first();
    }
    checked.insert(executable.to_path_buf(), stamp);
    Ok(())
}

pub(crate) fn cache_is_current(executable: &Path, link: &LinkedComposition, path: &str) -> Result<bool, String> {
    Ok(matches_cache(Path::new(path), link, &source_version(link)?, executable))
}

#[cfg(test)]
mod live_handoff_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A stand-in EffectCraft control channel: answers `engine.execute` for one connection at a
    /// time and records the commands it ran.
    fn fake_effectcraft(project: Option<PathBuf>, import: Value) -> (u16, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut writer = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let params = &request["params"];
                    let command = params["command"].as_str().unwrap_or_default().to_string();
                    if command == "file.importTimeline" {
                        // the selection file exists while EffectCraft reads it
                        assert!(Path::new(params["params"]["path"].as_str().unwrap()).is_file());
                    }
                    log.lock().unwrap().push(params.clone());
                    let result = match command.as_str() {
                        "project.summary" => serde_json::json!({"path": project, "revision": 7}),
                        "file.importTimeline" => import.clone(),
                        "file.save" => {
                            let path = project.clone().unwrap();
                            std::fs::write(path, r#"{"schema":2,"items":{"42":{"kind":{"type":"Comp"}}}}"#).unwrap();
                            Value::Null
                        }
                        _ => Value::Null,
                    };
                    let reply = serde_json::json!({"id": request["id"], "ok": true, "result": result});
                    writeln!(writer, "{reply}").unwrap();
                }
            }
        });
        (port, seen)
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("filmcraft-live-handoff-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn commands(seen: &Mutex<Vec<Value>>) -> Vec<String> {
        seen.lock().unwrap().iter().map(|p| p["command"].as_str().unwrap().to_string()).collect()
    }

    fn good_import() -> Value {
        serde_json::json!({"supportsSourceTimeKeys": true, "warnings": [], "missing": [], "comps": [42]})
    }

    #[test]
    fn replacement_lands_as_a_new_comp_in_the_open_saved_project() {
        let dir = TempDir::new("ok");
        let project = dir.path().join("Open.ecproj");
        let (port, seen) = fake_effectcraft(Some(project.clone()), good_import());
        assert_eq!(open_effectcraft_project(port).as_deref(), project.to_str());
        let link = import_timeline_live(port, b"{}", &AtomicBool::new(false)).unwrap();
        assert_eq!(link, LinkedComposition { project_path: project.to_string_lossy().into_owned(), composition_id: 42, baked: false });
        assert_eq!(commands(&seen), ["project.summary", "project.summary", "file.importTimeline", "file.save"]);
        assert!(!dir.path().join(".filmcraft-handoff").read_dir().unwrap().any(|e| e.unwrap().path().join("selection.otio").exists()));
    }

    #[test]
    fn a_refused_import_is_undone_and_never_saved() {
        let dir = TempDir::new("refused");
        let project = dir.path().join("Open.ecproj");
        let import = serde_json::json!({"supportsSourceTimeKeys": true, "warnings": ["unsupported transition"], "missing": [], "comps": [42]});
        let (port, seen) = fake_effectcraft(Some(project.clone()), import);
        let error = import_timeline_live(port, b"{}", &AtomicBool::new(false)).unwrap_err();
        assert!(error.contains("unsupported transition"), "{error}");
        assert_eq!(commands(&seen), ["project.summary", "file.importTimeline", "project.summary", "edit.undo"]);
        assert_eq!(seen.lock().unwrap().last().unwrap()["params"]["expectRevision"], 7);
        assert!(!project.exists());
    }

    #[test]
    fn no_running_effectcraft_or_an_unsaved_project_falls_back_to_a_new_file() {
        let (port, _) = fake_effectcraft(None, good_import());
        assert_eq!(open_effectcraft_project(port), None);
        assert!(import_timeline_live(port, b"{}", &AtomicBool::new(false)).unwrap_err().contains("never been saved"));
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert_eq!(open_effectcraft_project(closed), None);
    }
}
