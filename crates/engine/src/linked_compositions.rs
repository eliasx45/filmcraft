//! Saved-composition links: native work stays off the UI thread, and completed renders are
//! applied only while their original link still exists in the same document.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use web_time::Instant;

use filmcraft_project::{ItemId, ItemKind, Label, MediaClip, MediaRef, linked_composition::LinkedComposition};
use serde_json::{Value, json};

use crate::linked_render::{CachedRender, cached_render};
use crate::{
    EngineError, Result, Session,
    commands::{CommandSpec, always, bad, str_p, u64_p},
};

#[derive(Clone, Debug, PartialEq)]
struct Request {
    ticket: u64,
    item: Option<ItemId>,
    link: LinkedComposition,
    previous: Option<String>,
    bake: bool,
    replacement: Option<Arc<crate::linked_handoff::Plan>>,
    /// A replacement into the project open in the running EffectCraft (its control port) rather
    /// than a new project file.
    live: Option<u16>,
}

struct Completed {
    request: Request,
    result: std::result::Result<(LinkedComposition, CachedRender), String>,
}

struct Worker {
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Completed>,
    requests: Vec<Request>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct Runtime {
    /// Changes only when a different document is installed; UI drafts must not cross documents.
    pub document_generation: u64,
    worker: Option<Worker>,
    imports: Vec<Request>,
    next_ticket: u64,
    next_scan: Instant,
    /// Session configuration, never read from an untrusted project document.
    pub executable: PathBuf,
    pub cache_root: PathBuf,
    pub status: BTreeMap<String, Value>,
    suspended: Vec<ItemId>,
    bakes: Vec<ItemId>,
}

impl Default for Runtime {
    fn default() -> Self {
        let executable = std::env::var_os("FILMCRAFT_EFFECTCRAFT_CLI")
            .map(PathBuf::from)
            .or_else(|| crate::autosave::default_data_dir().map(|dir| dir.join("EffectCraft Renderer/effectcraft-cli")).filter(|path| path.is_file()))
            .unwrap_or_else(|| PathBuf::from("effectcraft-cli"));
        Self {
            document_generation: 0,
            worker: None,
            imports: Vec::new(),
            next_ticket: 1,
            next_scan: Instant::now(),
            executable,
            cache_root: std::env::temp_dir().join("filmcraft-effectcraft-cache"),
            status: BTreeMap::new(),
            suspended: Vec::new(),
            bakes: Vec::new(),
        }
    }
}

impl Runtime {
    pub fn reset_document(&mut self) {
        self.document_generation = self.document_generation.wrapping_add(1);
        self.reset();
    }

    /// Drop old-document work while retaining trusted renderer configuration.
    pub fn reset(&mut self) {
        self.worker = None;
        self.imports.clear();
        self.status.clear();
        self.suspended.clear();
        self.bakes.clear();
        self.next_scan = Instant::now();
    }
}

fn key(request: &Request) -> String {
    request.item.map_or_else(|| format!("request:{}", request.ticket), |id| format!("item:{}", id.0))
}

fn current(s: &Session, request: &Request) -> bool {
    match request.item {
        None => s.links.imports.iter().any(|r| r.ticket == request.ticket) && request.replacement.as_ref().is_none_or(|plan| plan.is_current(&s.project)),
        Some(id) => {
            !s.links.suspended.contains(&id)
                && s.project.linked_compositions.get(&id) == Some(&request.link)
                && s.project.item(id).and_then(|i| i.as_media()).is_some_and(|m| match &m.media {
                    MediaRef::File { path } => request.previous.as_ref() == Some(path),
                    _ => false,
                })
        }
    }
}

pub(crate) fn resolve_executable(path: &std::path::Path) -> std::result::Result<PathBuf, String> {
    if path.is_absolute() {
        return std::fs::canonicalize(path).map_err(|e| format!("EffectCraft renderer {}: {e}", path.display()));
    }
    if path.components().count() > 1 {
        return std::fs::canonicalize(path).map_err(|e| format!("EffectCraft renderer: {e}"));
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(path);
            if candidate.is_file() {
                return std::fs::canonicalize(candidate).map_err(|e| e.to_string());
            }
        }
    }
    Err("EffectCraft renderer not found; set FILMCRAFT_EFFECTCRAFT_CLI or use link.configure".into())
}

/// Called by the frontend and headless job pump. Only channels/document data are touched here;
/// filesystem scans, process waits and movie probing run on the worker.
pub fn poll(s: &mut Session) {
    let invalid: Vec<_> = s
        .links
        .imports
        .iter()
        .filter(|r| r.replacement.as_ref().is_some_and(|plan| !plan.is_current(&s.project)))
        .map(|r| (r.ticket, r.link.project_path.clone()))
        .collect();
    for (ticket, path) in invalid {
        s.links.imports.retain(|r| r.ticket != ticket);
        s.links
            .status
            .insert(format!("request:{ticket}"), json!({"state":"error","error":"timeline changed during handoff; original clips kept","projectPath":path}));
    }
    let mut completed = Vec::new();
    let mut disconnected = false;
    if let Some(worker) = &s.links.worker {
        if worker.requests.iter().any(|r| !current(s, r)) {
            worker.cancel.store(true, Ordering::Relaxed);
        }
        loop {
            match worker.receiver.try_recv() {
                Ok(result) => completed.push(result),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
    }
    for completed in completed {
        if !current(s, &completed.request) {
            continue;
        }
        let request = completed.request;
        if let Some(worker) = s.links.worker.as_mut() {
            worker.requests.retain(|r| r != &request);
        }
        let state_key = key(&request);
        match completed.result {
            Ok((resolved_link, cache)) => {
                let info = cache.source.info().clone();
                let path = cache.path.clone();
                let applied = if let Some(plan) = &request.replacement {
                    plan.apply(s, resolved_link.clone(), &cache)
                } else if let Some(id) = request.item.filter(|_| request.bake) {
                    s.edit("Bake EffectCraft Composition", |project, _| {
                        let item = project.item_mut(id).ok_or_else(|| EngineError::Other("linked item was removed".into()))?;
                        let ItemKind::Media(media) = &mut item.kind else {
                            return Err(EngineError::Other("linked item is not media".into()));
                        };
                        media.media = MediaRef::File { path: path.clone() };
                        media.info = info;
                        media.identity = None;
                        if let Some(link) = project.linked_compositions.get_mut(&id) {
                            link.baked = true;
                        }
                        Ok(id)
                    })
                } else if let Some(id) = request.item {
                    if let Some(item) = Arc::make_mut(&mut s.project).item_mut(id)
                        && let ItemKind::Media(media) = &mut item.kind
                    {
                        let changed = media.media != (MediaRef::File { path: path.clone() });
                        media.media = MediaRef::File { path: path.clone() };
                        media.info = info;
                        media.identity = None;
                        if changed {
                            s.media.remove(id);
                            s.bump_view();
                        }
                    }
                    Ok(id)
                } else {
                    let link = resolved_link.clone();
                    s.edit("Link EffectCraft Composition", |project, state| {
                        project.next_id.checked_add(1).ok_or_else(|| EngineError::Other("project item ids exhausted".into()))?;
                        let id = project.add_item(
                            &format!("EffectCraft Composition {}", link.composition_id),
                            Label::Purple,
                            ItemKind::Media(MediaClip {
                                media: MediaRef::File { path: path.clone() },
                                info,
                                interpret: Default::default(),
                                mark_in: None,
                                mark_out: None,
                                markers: Vec::new(),
                                offline: false,
                                proxy: None,
                                identity: None,
                            }),
                            None,
                        );
                        project.linked_compositions.insert(id, link);
                        state.project_selection = vec![id];
                        Ok(id)
                    })
                };
                match applied {
                    Ok(id) => {
                        if request.previous.as_ref() != Some(&cache.path) {
                            s.media.insert_file(id, &cache.path, cache.source);
                        }
                        if request.bake {
                            s.links.bakes.retain(|i| *i != id);
                        }
                        let state = if s.links.bakes.contains(&id) { "baking" } else { "ready" };
                        let mut status = json!({"state":state,"item":id.0,"path":cache.path});
                        if let Some(warning) = request.replacement.as_ref().and_then(|plan| plan.warning()) {
                            status["warning"] = json!(warning);
                        }
                        s.links.status.insert(format!("item:{}", id.0), status.clone());
                        s.links.status.insert(state_key, status);
                    }
                    Err(e) => {
                        s.links.status.insert(state_key, json!({"state":"error","error":e.to_string()}));
                    }
                }
            }
            Err(error) => {
                s.links.status.insert(state_key, json!({"state":"error","error":error,"stale":request.previous.is_some()}));
            }
        }
        if request.item.is_none() {
            s.links.imports.retain(|r| r.ticket != request.ticket);
        }
    }
    let oldest_ticket = s.links.next_ticket.saturating_sub(256);
    s.links.status.retain(|key, _| {
        if let Some(ticket) = key.strip_prefix("request:").and_then(|v| v.parse::<u64>().ok()) {
            return ticket >= oldest_ticket;
        }
        if let Some(id) = key.strip_prefix("item:").and_then(|v| v.parse::<u64>().ok()) {
            return s.project.linked_compositions.contains_key(&ItemId(id));
        }
        true
    });
    if disconnected {
        s.links.worker = None;
        s.links.next_scan = Instant::now() + Duration::from_secs(1);
    }
    if s.links.worker.is_some() || Instant::now() < s.links.next_scan {
        return;
    }
    let mut requests = s.links.imports.clone();
    for (id, link) in &s.project.linked_compositions {
        if link.baked || s.links.suspended.contains(id) {
            continue;
        }
        let Some(media) = s.project.item(*id).and_then(|i| i.as_media()).filter(|m| !m.offline) else { continue };
        let MediaRef::File { path } = &media.media else { continue };
        requests.push(Request {
            ticket: 0,
            item: Some(*id),
            link: link.clone(),
            previous: Some(path.clone()),
            bake: s.links.bakes.contains(id),
            replacement: None,
            live: None,
        });
    }
    if requests.is_empty() {
        return;
    }
    if requests.len() > 256 {
        s.links.status.insert("error".into(), json!({"state":"error","error":"at most 256 active composition links per project"}));
        return;
    }
    let (sender, receiver) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let executable = s.links.executable.clone();
    let root = s.links.cache_root.clone();
    let services = s.services.clone();
    let tasks = requests.clone();
    let spawn = std::thread::Builder::new().name("filmcraft-effectcraft-links".into()).spawn(move || {
        let executable = resolve_executable(&executable);
        for request in tasks {
            if worker_cancel.load(Ordering::Relaxed) {
                break;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let executable = executable.as_ref().map_err(Clone::clone)?;
                let link = if let (Some(plan), Some(port)) = (&request.replacement, request.live) {
                    crate::linked_render::import_timeline_live(port, &plan.interchange()?, &worker_cancel)?
                } else if let Some(plan) = &request.replacement {
                    crate::linked_render::import_timeline(executable, &plan.interchange()?, std::path::Path::new(&request.link.project_path), &worker_cancel)?
                } else {
                    request.link.clone()
                };
                cached_render(executable, &link, request.previous.as_deref(), &root, &worker_cancel, services.as_ref())
                    .map_err(|error| {
                        if request.replacement.is_some() {
                            format!("{error}; editable project retained at {} and original clips kept", link.project_path)
                        } else {
                            error
                        }
                    })
                    .map(|cache| (link, cache))
            }))
            .unwrap_or_else(|_| Err("EffectCraft link worker failed internally; see crash log".into()));
            if sender.send(Completed { request, result }).is_err() {
                break;
            }
        }
    });
    match spawn {
        Ok(_) => {
            for request in &requests {
                s.links.status.insert(key(request), json!({"state":"updating","stale":request.previous.is_some()}));
            }
            s.links.worker = Some(Worker { cancel, receiver, requests });
        }
        Err(e) => {
            s.links.status.insert("error".into(), json!({"state":"error","error":e.to_string()}));
            s.links.next_scan = Instant::now() + Duration::from_secs(5);
        }
    }
}

fn source(p: &Value, command: &str) -> Result<LinkedComposition> {
    let project_path = str_p(p, "path").ok_or_else(|| bad(command, "need `path`"))?.to_string();
    if !std::path::Path::new(&project_path).is_absolute() {
        return Err(bad(command, "project path must be absolute"));
    }
    let composition_id = u64_p(p, "composition").ok_or_else(|| bad(command, "need numeric `composition` id"))?;
    let link = LinkedComposition { project_path, composition_id, baked: false };
    link.validate().map_err(|e| bad(command, e))?;
    Ok(link)
}

fn selected(s: &Session, p: &Value, command: &str) -> Result<ItemId> {
    let id = u64_p(p, "item").map(ItemId).or_else(|| s.state.project_selection.first().copied()).ok_or_else(|| bad(command, "select a linked composition"))?;
    if !s.project.linked_compositions.contains_key(&id) {
        return Err(bad(command, "item is not a linked composition"));
    }
    Ok(id)
}

pub fn commands() -> Vec<CommandSpec> {
    let spec = |id, label, menu, params, run| CommandSpec { id, label, menu, shortcut: None, params, enabled: always, run, journal: true };
    vec![
        spec("link.configure", "Configure EffectCraft Renderer", &[], r#"{"executable":str}"#, |s, p| {
            let path = str_p(p, "executable").ok_or_else(|| bad("link.configure", "need executable path"))?;
            if !std::path::Path::new(path).is_absolute() {
                return Err(bad("link.configure", "executable path must be absolute"));
            }
            let mut prefs = s.prefs.clone();
            prefs.general.effectcraft_renderer = path.into();
            s.set_prefs(prefs).map_err(|e| EngineError::Other(format!("saving renderer preference: {e}")))?;
            Ok(Value::Null)
        }),
        spec("link.import", "Link EffectCraft Composition…", &["File", "Import"], r#"{"path":str,"composition":id,"wait":bool=false}"#, |s, p| {
            let link = source(p, "link.import")?;
            if s.project.linked_compositions.values().filter(|link| !link.baked).count().saturating_add(s.links.imports.len()) >= 256 {
                return Err(EngineError::Other("at most 256 active composition links per project; bake unused links first".into()));
            }
            if s.links.imports.len() >= 16 {
                return Err(bad("link.import", "too many pending imports"));
            }
            let ticket = s.links.next_ticket;
            s.links.next_ticket = ticket.checked_add(1).ok_or_else(|| EngineError::Other("link request ids exhausted".into()))?;
            s.links.imports.push(Request { ticket, item: None, link, previous: None, bake: false, replacement: None, live: None });
            s.links.next_scan = Instant::now();
            poll(s);
            if p.get("wait").and_then(Value::as_bool).unwrap_or(false) {
                let deadline = Instant::now() + Duration::from_secs(3605);
                loop {
                    poll(s);
                    if let Some(status) = s.links.status.get(&format!("request:{ticket}")) {
                        match status["state"].as_str() {
                            Some("ready") => return Ok(status.clone()),
                            Some("error" | "cancelled") => return Err(EngineError::Other(status["error"].as_str().unwrap_or("link import cancelled").into())),
                            _ => {}
                        }
                    }
                    if Instant::now() >= deadline {
                        s.links.imports.retain(|r| r.ticket != ticket);
                        return Err(EngineError::Other("EffectCraft link import timed out".into()));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            Ok(json!({"request":ticket}))
        }),
        spec("link.replace", "Replace with EffectCraft Composition…", &["Clip", "EffectCraft"], r#"{"path":str?,"open":bool=false,"port":u32?}"#, |s, p| {
            // `open`: a new composition in the project open in the running EffectCraft (as Premiere
            // with After Effects); else a new project at `path`.
            let live = if p.get("open").and_then(Value::as_bool).unwrap_or(false) {
                let port = match p.get("port") {
                    None | Some(Value::Null) => crate::linked_render::effectcraft_control_port(),
                    Some(v) => v.as_u64().and_then(|n| u16::try_from(n).ok()).filter(|n| *n != 0).ok_or_else(|| bad("link.replace", "port must be 1-65535"))?,
                };
                Some(port)
            } else {
                None
            };
            let path = match live {
                Some(_) => "",
                None => str_p(p, "path").ok_or_else(|| bad("link.replace", "choose a new .ecproj destination"))?,
            };
            if live.is_none() && (!std::path::Path::new(path).is_absolute() || !path.to_ascii_lowercase().ends_with(".ecproj")) {
                return Err(bad("link.replace", "destination must be an absolute .ecproj path"));
            }
            if s.project.linked_compositions.values().filter(|link| !link.baked).count().saturating_add(s.links.imports.len()) >= 256 {
                return Err(EngineError::Other("at most 256 active composition links per project; bake unused links first".into()));
            }
            if s.links.imports.len() >= 16 {
                return Err(bad("link.replace", "too many pending imports"));
            }
            let plan = Arc::new(crate::linked_handoff::Plan::prepare(s)?);
            let warning = plan.warning();
            let ticket = s.links.next_ticket;
            s.links.next_ticket = ticket.checked_add(1).ok_or_else(|| EngineError::Other("link request ids exhausted".into()))?;
            s.links.imports.push(Request {
                ticket,
                item: None,
                link: LinkedComposition { project_path: path.into(), composition_id: 0, baked: false },
                previous: None,
                bake: false,
                replacement: Some(plan),
                live,
            });
            s.links.next_scan = Instant::now();
            poll(s);
            Ok(json!({"request":ticket,"originalClipsRetainedUntilReady":true,"warning":warning}))
        }),
        spec("link.status", "Composition Link Status", &[], "{}", |s, _| {
            poll(s);
            Ok(json!(s.links.status))
        }),
        spec("link.refresh", "Refresh EffectCraft Links", &["Clip", "EffectCraft"], "{}", |s, _| {
            s.links.suspended.clear();
            s.links.status.clear();
            s.links.next_scan = Instant::now();
            poll(s);
            Ok(Value::Null)
        }),
        spec("link.relink", "Relink EffectCraft Composition…", &["Clip", "EffectCraft"], r#"{"item":id?,"path":str,"composition":id}"#, |s, p| {
            let id = selected(s, p, "link.relink")?;
            let link = source(p, "link.relink")?;
            s.edit("Relink EffectCraft Composition", |project, _| {
                project.linked_compositions.insert(id, link);
                Ok(())
            })?;
            s.links.suspended.retain(|i| *i != id);
            s.links.next_scan = Instant::now();
            Ok(Value::Null)
        }),
        spec("link.bake", "Bake EffectCraft Composition", &["Clip", "EffectCraft"], r#"{"item":id?}"#, |s, p| {
            let id = selected(s, p, "link.bake")?;
            if !s.links.bakes.contains(&id) {
                s.links.bakes.push(id);
            }
            s.links.suspended.retain(|i| *i != id);
            s.links.status.insert(format!("item:{}", id.0), json!({"state":"baking","stale":true}));
            s.links.next_scan = Instant::now();
            Ok(Value::Null)
        }),
        spec("link.restore", "Restore EffectCraft Link", &["Clip", "EffectCraft"], r#"{"item":id?}"#, |s, p| {
            let id = selected(s, p, "link.restore")?;
            s.edit("Restore EffectCraft Link", |project, _| {
                if let Some(link) = project.linked_compositions.get_mut(&id) {
                    link.baked = false;
                }
                Ok(())
            })?;
            s.links.suspended.retain(|i| *i != id);
            s.links.next_scan = Instant::now();
            Ok(Value::Null)
        }),
        spec("link.cancel", "Cancel EffectCraft Updates", &["Clip", "EffectCraft"], "{}", |s, _| {
            if let Some(worker) = &s.links.worker {
                worker.cancel.store(true, Ordering::Relaxed);
            }
            s.links.imports.clear();
            s.links.bakes.clear();
            s.links.suspended = s.project.linked_compositions.keys().copied().collect();
            for value in s.links.status.values_mut() {
                *value = json!({"state":"cancelled","stale":true});
            }
            Ok(Value::Null)
        }),
    ]
}

/// Prepare only the sources used by this sequence, including nests/subclips. This runs inside
/// the export job, so waiting for current composition movies never blocks the desktop UI.
pub fn prepare_export(
    project: &Arc<filmcraft_project::Project>,
    sequence: ItemId,
    executable: &std::path::Path,
    cache_root: &std::path::Path,
    services: &dyn crate::Services,
    progress: &filmcraft_export::Progress,
) -> std::result::Result<Arc<filmcraft_project::Project>, String> {
    let used = crate::project_tools::closure(project, &[sequence]);
    let links: Vec<_> = project.linked_compositions.iter().filter(|(id, link)| used.contains(id) && !link.baked).take(257).collect();
    if links.len() > 256 {
        return Err("at most 256 active composition links per export".into());
    }
    if links.is_empty() {
        return Ok(project.clone());
    }
    let executable = resolve_executable(executable)?;
    let mut prepared = (**project).clone();
    for _ in 0..16 {
        for &(id, link) in &links {
            *progress.status.lock().unwrap_or_else(|e| e.into_inner()) = format!("Updating EffectCraft composition {} before export", link.composition_id);
            let Some(media) = prepared.item_mut(*id).and_then(|i| match &mut i.kind {
                ItemKind::Media(m) => Some(m),
                _ => None,
            }) else {
                return Err(format!("linked item {} has no media", id.0));
            };
            if media.offline {
                return Err(format!("linked composition {} is offline", id.0));
            }
            let previous = match &media.media {
                MediaRef::File { path } => Some(path.as_str()),
                _ => None,
            };
            let rendered = cached_render(&executable, link, previous, cache_root, &progress.cancel, services)?;
            media.media = MediaRef::File { path: rendered.path };
            media.info = rendered.source.info().clone();
            media.proxy = None;
            media.identity = None;
        }
        // A save to an earlier link while a later one rendered must not leave a stale movie in
        // the export snapshot. Freeze the verified movies only after the entire set is current.
        let mut current = true;
        for &(id, link) in &links {
            if progress.cancel.load(Ordering::Relaxed) {
                return Err("export cancelled while updating EffectCraft links".into());
            }
            let path = prepared
                .item(*id)
                .and_then(|i| i.as_media())
                .and_then(|m| match &m.media {
                    MediaRef::File { path } => Some(path.as_str()),
                    _ => None,
                })
                .ok_or("linked media is missing")?;
            if !crate::linked_render::cache_is_current(&executable, link, path)? {
                current = false;
                break;
            }
        }
        if current {
            return Ok(Arc::new(prepared));
        }
    }
    Err("EffectCraft sources keep changing; finish saving before exporting".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_item_ids_refuse_completed_import_without_panicking_or_changing_document() {
        let mut session = Session::default();
        Arc::make_mut(&mut session.project).next_id = u64::MAX;
        let before = session.project.clone();
        let link = LinkedComposition { project_path: "/fixture.ecproj".into(), composition_id: 1, baked: false };
        let request = Request { ticket: 1, item: None, link: link.clone(), previous: None, bake: false, replacement: None, live: None };
        let source = Arc::new(filmcraft_media::generators::GeneratorSource::new(
            filmcraft_media::Generator::ColorMatte { color: [1.0, 0.0, 0.0, 1.0] },
            64,
            64,
            filmcraft_time::FrameRate::FPS_24,
            filmcraft_time::FrameRate::FPS_24.tick_of(6),
        ));
        let (sender, receiver) = mpsc::channel();
        session.links.imports.push(request.clone());
        session.links.worker = Some(Worker { cancel: Arc::new(AtomicBool::new(false)), receiver, requests: vec![request.clone()] });
        sender.send(Completed { request, result: Ok((link, CachedRender { path: "/fixture.mov".into(), source })) }).unwrap();
        drop(sender);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll(&mut session))).is_ok());
        assert_eq!(session.project, before);
        assert_eq!(session.links.status["request:1"]["state"], "error");
    }

    #[test]
    fn relinking_discards_an_already_completed_old_render() {
        let mut session = Session::default();
        let result = session.execute("file.newColorMatte", json!({"color":"#ff8000","width":64,"height":64,"seconds":0.25})).unwrap();
        let id = ItemId(result["item"].as_u64().unwrap());
        let source = session.media.full_res_source(&session.project, id, session.services.as_ref()).unwrap();
        let link = LinkedComposition { project_path: "/old.ecproj".into(), composition_id: 1, baked: false };
        let project = Arc::make_mut(&mut session.project);
        project.item_mut(id).unwrap().as_media_mut().unwrap().media = MediaRef::File { path: "/last-good.mov".into() };
        project.linked_compositions.insert(id, link.clone());
        let request =
            Request { ticket: 0, item: Some(id), link: link.clone(), previous: Some("/last-good.mov".into()), bake: false, replacement: None, live: None };
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        session.links.worker = Some(Worker { cancel: cancel.clone(), receiver, requests: vec![request.clone()] });
        sender.send(Completed { request, result: Ok((link, CachedRender { path: "/obsolete.mov".into(), source })) }).unwrap();
        drop(sender);
        session.execute("link.relink", json!({"item":id.0,"path":"/new.ecproj","composition":2})).unwrap();
        let expected = session.project.clone();
        poll(&mut session);
        assert_eq!(session.project, expected, "old render must not overwrite a relinked item");
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn renderer_preference_survives_restart_outside_the_document() {
        let dir = crate::media_test_util::tmp_dir("link-preference");
        let executable = dir.join("effectcraft-cli");
        let mut session = Session::default();
        session.start_autosave(crate::autosave::AutosaveConfig::new(&dir)).unwrap();
        session.execute("link.configure", json!({"executable":executable})).unwrap();
        let saved_project = serde_json::to_string(&session.project).unwrap();
        assert!(!saved_project.contains("effectcraft-cli"));
        session.shutdown();
        let mut reopened = Session::default();
        reopened.start_autosave(crate::autosave::AutosaveConfig::new(&dir)).unwrap();
        assert_eq!(reopened.links.executable, executable);
        reopened.shutdown();
    }

    #[test]
    fn malformed_link_commands_leave_document_unchanged() {
        let mut session = Session::default();
        let before = session.project.clone();
        for params in
            [json!({}), json!({"path":"relative.ecproj","composition":1}), json!({"path":"/x","composition":0}), json!({"path":"/x","composition":-1})]
        {
            assert!(session.execute("link.import", params).is_err());
        }
        assert!(session.execute("link.relink", json!({"item":123,"path":"/x","composition":1})).is_err());
        assert_eq!(session.project, before);
        assert!(session.links.imports.is_empty());
    }

    fn wait_for(session: &mut Session, state_key: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            poll(session);
            let status = session.links.status.get(state_key).cloned().unwrap_or(Value::Null);
            if status["state"] == "ready" || status["state"] == "error" {
                return status;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {state_key}: {status}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "requires FILMCRAFT_EFFECTCRAFT_TEST_CLI"]
    fn native_link_import_refresh_reopen_and_export() {
        let executable = PathBuf::from(std::env::var_os("FILMCRAFT_EFFECTCRAFT_TEST_CLI").expect("set CLI path"));
        let dir = crate::media_test_util::tmp_dir("link-session");
        let source = dir.join("source.ecproj");
        let created = std::process::Command::new(&executable)
            .args([
                "run",
                "comp.new",
                r##"{"name":"Link test","width":64,"height":64,"frameRate":24,"duration":0.25}"##,
                "layer.newSolid",
                r##"{"name":"Square","width":32,"height":32,"color":"#ff8000"}"##,
                "--empty",
                "--save-as",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
        let mut session = Session::default();
        session.links.cache_root = dir.join("cache");
        session.execute("link.configure", json!({"executable":executable})).unwrap();
        let result = session.execute("link.import", json!({"path":source,"composition":1})).unwrap();
        let status = wait_for(&mut session, &format!("request:{}", result["request"]));
        assert_eq!(status["state"], "ready", "{status}");
        let id = ItemId(status["item"].as_u64().unwrap());
        assert!(!session.project.item(id).unwrap().as_media().unwrap().info.has_audio());
        let original_path = status["path"].as_str().unwrap().to_string();
        session.execute("edit.undo", json!({})).unwrap();
        assert!(session.project.linked_compositions.is_empty());
        session.execute("edit.redo", json!({})).unwrap();
        assert!(session.project.linked_compositions.contains_key(&id));
        session.execute("file.newSequence", json!({"fromItem":id.0,"name":"Linked sequence"})).unwrap();
        let saved = dir.join("edit.fcproj");
        session.execute("file.save", json!({"path":saved})).unwrap();
        session.execute("file.open", json!({"path":saved})).unwrap();
        assert_eq!(session.project.linked_compositions[&id].composition_id, 1);
        // Saving an innocuous change must invalidate the source; no manual movie replacement.
        let mut doc: Value = serde_json::from_slice(&std::fs::read(&source).unwrap()).unwrap();
        doc["items"]["1"]["name"] = json!("Saved update");
        let mut changed = 0;
        for item in doc["items"].as_object_mut().unwrap().values_mut() {
            if item["kind"]["type"] == "Solid" {
                item["kind"]["color"] = json!([0.0, 1.0, 0.0]);
                changed += 1;
            }
        }
        assert_eq!(changed, 1);
        std::fs::write(&source, serde_json::to_vec(&doc).unwrap()).unwrap();
        session.execute("link.refresh", json!({})).unwrap();
        let status = wait_for(&mut session, &format!("item:{}", id.0));
        assert_eq!(status["state"], "ready", "{status}");
        assert_ne!(status["path"], original_path);
        let current_source = session.media.full_res_source(&session.project, id, session.services.as_ref()).unwrap();
        let pixels = current_source.video_frame(filmcraft_media::FrameRequest::full(filmcraft_time::Tick::ZERO)).unwrap().to_rgba8();
        let center = (32 * 64 + 32) * 4;
        assert!(pixels[center + 1] > 200 && pixels[center] < 10, "saved green square must appear in FilmCraft");
        session.execute("link.bake", json!({"item":id.0})).unwrap();
        assert_eq!(wait_for(&mut session, &format!("item:{}", id.0))["state"], "ready");
        assert!(session.project.linked_compositions[&id].baked);
        session.execute("link.restore", json!({"item":id.0})).unwrap();
        assert!(!session.project.linked_compositions[&id].baked);
        // Save again without refreshing the preview: final export must still render this save.
        for item in doc["items"].as_object_mut().unwrap().values_mut() {
            if item["kind"]["type"] == "Solid" {
                item["kind"]["color"] = json!([0.0, 0.0, 1.0]);
            }
        }
        std::fs::write(&source, serde_json::to_vec(&doc).unwrap()).unwrap();
        let output = dir.join("finished.mov");
        session.execute("file.exportMedia", json!({"path":output,"format":"prores","audio":false,"wait":true})).unwrap();
        assert!(output.is_file());
        let exported = filmcraft_codecs::open_bytes("finished.mov", std::fs::read(&output).unwrap().into()).unwrap();
        let pixels = exported.video_frame(filmcraft_media::FrameRequest::full(filmcraft_time::Tick::ZERO)).unwrap().to_rgba8();
        assert!(pixels[center + 2] > 200 && pixels[center + 1] < 10, "export must contain the newer blue square");
        std::fs::remove_file(&source).unwrap();
        let failed_output = dir.join("must-not-export.mov");
        let result = session.execute("file.exportMedia", json!({"path":failed_output,"format":"prores","audio":false,"wait":true}));
        assert!(result.is_err(), "missing source must fail final export: {result:?}");
        assert!(!failed_output.exists());
    }
}

#[cfg(all(test, unix))]
mod initial_save_race_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    #[ignore = "requires FILMCRAFT_EFFECTCRAFT_TEST_CLI"]
    fn initial_link_retries_a_save_during_its_first_render() {
        let executable = std::env::var("FILMCRAFT_EFFECTCRAFT_TEST_CLI").expect("set CLI path");
        let dir = crate::media_test_util::tmp_dir("initial-link-save-race");
        let project = dir.join("source.ecproj");
        let created = std::process::Command::new(&executable)
            .args(["exec", "comp.new", "--empty", "--params", r#"{"width":16,"height":16,"frameRate":24,"duration":0.125}"#, "--save-as"])
            .arg(&project)
            .output()
            .unwrap();
        assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
        symlink(&executable, dir.join("real-cli")).unwrap();
        let wrapper = dir.join("wrapper");
        std::fs::write(
            &wrapper,
            br##"#!/bin/sh
if [ "$1" = render ]; then
  printf 'render\n' >> "$0.renders"
  if [ ! -f "$0.changed" ]; then
    printf '\n' >> "${0%/*}/source.ecproj"
    : > "$0.changed"
    exit 9
  fi
fi
exec "${0%/*}/real-cli" "$@"
"##,
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut session = Session::default();
        session.links.cache_root = dir.join("cache");
        session.execute("link.configure", json!({"executable":wrapper})).unwrap();
        let result = session.execute("link.import", json!({"path":project,"composition":1,"wait":true})).unwrap();
        assert_eq!(result["state"], "ready");
        assert_eq!(std::fs::read_to_string(dir.join("wrapper.renders")).unwrap().lines().count(), 2);
        assert_eq!(session.project.linked_compositions.len(), 1);
    }
}
