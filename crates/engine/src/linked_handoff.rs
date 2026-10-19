//! Selected-video handoff. The new project is saved/rendered before one undoable replacement;
//! concurrent timeline changes abort the replacement, and original audio is never removed.
use crate::{EngineError, Result, Session, linked_render::CachedRender};
use filmcraft_project::{
    ClipId, ItemId, ItemKind, Label, MediaClip, MediaRef, Project, ProjectItem, Sequence, TrackId, TrackKind, linked_composition::LinkedComposition,
};
use filmcraft_time::{Tick, TimeRange};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub sequence: ItemId,
    original: Arc<Sequence>,
    dependencies: BTreeMap<ItemId, ProjectItem>,
    fragment: Arc<Project>,
    selected: BTreeSet<ClipId>,
    target: TrackId,
    start: Tick,
    duration: Tick,
    /// Clip effects (and effect masks) left out of the handoff: EffectCraft can't import them yet.
    pub dropped: Vec<String>,
    /// Transitions on the selected clips' edges, removed with the replacement.
    pub transitions_removed: Vec<String>,
}

impl Plan {
    pub fn prepare(session: &Session) -> Result<Self> {
        let fail = |text: &str| EngineError::Other(format!("Replace with EffectCraft Composition: {text}"));
        let sequence = session.state.active_sequence.ok_or(EngineError::NoSequence)?;
        let original = match session.project.item(sequence).map(|item| &item.kind) {
            Some(ItemKind::Sequence(sequence)) => sequence.clone(),
            _ => return Err(EngineError::NoSequence),
        };
        original.check_bounds().map_err(|e| fail(&e))?;
        let selected: BTreeSet<_> =
            original.video_tracks.iter().flat_map(|t| &t.items).filter(|c| session.state.selection.contains(&c.id)).map(|c| c.id).collect();
        if selected.is_empty() || selected.len() > 256 {
            return Err(fail("select between 1 and 256 video clips"));
        }
        let mut removed_transitions: Vec<String> = Vec::new();
        let mut start = Tick::MAX;
        let mut end = Tick::ZERO;
        let mut first_track = usize::MAX;
        let mut last_track = 0usize;
        for (index, track) in original.video_tracks.iter().enumerate() {
            for clip in track.items.iter().filter(|c| selected.contains(&c.id)) {
                if track.locked || !track.enabled || track.muted {
                    return Err(fail("selected video tracks must be enabled and unlocked"));
                }
                // Transitions on the selected clips' edges are removed with the replacement (one undo step).
                for t in track.transitions.iter().filter(|t| t.from == Some(clip.id) || t.to == Some(clip.id)) {
                    let name = format!("{} transition", t.effect.def().map_or_else(|| t.effect.effect.clone(), |d| d.name.to_string()));
                    if !removed_transitions.contains(&name) {
                        removed_transitions.push(name);
                    }
                }
                start = start.min(clip.start);
                end = end.max(clip.end());
                first_track = first_track.min(index);
                last_track = last_track.max(index);
            }
        }
        if start < Tick::ZERO || end <= start {
            return Err(fail("invalid selected range"));
        }
        for track in original.video_tracks.iter().skip(first_track).take(last_track.saturating_sub(first_track).saturating_add(1)) {
            if track.items.iter().any(|c| !selected.contains(&c.id) && c.start < end && c.end() > start) {
                return Err(fail("include the intervening video clips so replacement preserves layer order"));
            }
        }
        let target = original.video_tracks.get(last_track).ok_or_else(|| fail("target track is missing"))?.id;
        let duration = Tick(end.0.checked_sub(start.0).ok_or_else(|| fail("selected range is too large"))?);
        let mut fragment = (*session.project).clone();
        let seq = fragment.sequence_mut(sequence).ok_or(EngineError::NoSequence)?;
        for track in &mut seq.video_tracks {
            track.items.retain(|c| selected.contains(&c.id));
            track.transitions.clear();
            for clip in &mut track.items {
                clip.start = Tick(clip.start.0.checked_sub(start.0).ok_or_else(|| fail("invalid clip position"))?);
            }
        }
        seq.audio_tracks.clear();
        seq.caption_tracks.clear();
        seq.markers.clear();
        seq.mark_in = None;
        seq.mark_out = None;
        seq.work_area = None;
        seq.start_timecode = 0;
        let keep = crate::project_tools::closure(&fragment, &[sequence]);
        fragment = crate::project_tools::project_subset(&fragment, &keep);
        let dependencies = keep.iter().filter(|id| **id != sequence).filter_map(|id| session.project.item(*id).map(|item| (*id, item.clone()))).collect();
        let mut dropped: Vec<String> = Vec::new();
        let transitions_removed = removed_transitions;
        for (id, item) in &mut fragment.items {
            if session.project.linked_compositions.get(id).is_some_and(|link| !link.baked) {
                return Err(fail("bake existing composition links before sending them to another composition"));
            }
            match &mut item.kind {
                ItemKind::Sequence(seq) => {
                    let seq = Arc::make_mut(seq);
                    seq.audio_tracks.clear();
                    seq.caption_tracks.clear();
                    seq.markers.clear();
                    for clip in seq.video_tracks.iter_mut().flat_map(|t| &mut t.items) {
                        if clip.multicam.is_some()
                            || clip.graphic.is_some()
                            || clip.hold_filters
                            || clip.field_options.is_some()
                            || !clip.time_interpolation.is_default()
                        {
                            return Err(fail("multi-camera, graphics, field options and non-default time interpolation need to be rendered first"));
                        }
                        // EffectCraft imports Motion, Opacity and speed only: the rest is left out, with a warning.
                        for effect in clip.effects.iter_mut() {
                            let name = effect.def().map_or_else(|| effect.effect.clone(), |d| d.name.to_string());
                            if !matches!(effect.effect.as_str(), "motion" | "opacity" | "time_remap") {
                                if !dropped.contains(&name) {
                                    dropped.push(name);
                                }
                            } else if !effect.masks.is_empty() {
                                let masks = format!("{name} masks");
                                if !dropped.contains(&masks) {
                                    dropped.push(masks);
                                }
                                effect.masks.clear();
                            }
                        }
                        clip.effects.retain(|e| matches!(e.effect.as_str(), "motion" | "opacity" | "time_remap"));
                        if clip.effects.iter().any(|e| e.effect == "time_remap" && e.params.values().any(|p| p.is_animated())) {
                            return Err(fail("variable-speed clips need to be rendered first"));
                        }
                    }
                }
                ItemKind::Media(media) => {
                    if media.offline || media.interpret != Default::default() {
                        return Err(fail("offline or reinterpreted footage needs to be resolved before replacement"));
                    }
                    if let MediaRef::File { path } = &mut media.media
                        && !std::path::Path::new(path).is_absolute()
                    {
                        let base = session
                            .path
                            .as_deref()
                            .and_then(|p| std::path::Path::new(p).parent())
                            .ok_or_else(|| fail("save the FilmCraft project to resolve relative media paths"))?;
                        *path = base.join(&*path).to_string_lossy().into_owned();
                    }
                    media.proxy = None;
                }
                _ => {}
            }
        }
        fragment.linked_compositions.clear();
        Ok(Self {
            sequence,
            original: original.clone(),
            dependencies,
            fragment: Arc::new(fragment),
            selected,
            target,
            start,
            duration,
            dropped,
            transitions_removed,
        })
    }

    /// The warning for effects left out of the handoff, if any.
    pub fn warning(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.dropped.is_empty() {
            parts.push(format!("not sent to EffectCraft (it can't import them yet): {}", self.dropped.join(", ")));
        }
        if !self.transitions_removed.is_empty() {
            parts.push(format!("removed: {}", self.transitions_removed.join(", ")));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    pub fn is_current(&self, project: &Project) -> bool {
        project.sequence(self.sequence).is_some_and(|seq| {
            std::ptr::eq(seq, self.original.as_ref()) || (seq.video_tracks == self.original.video_tracks && seq.settings == self.original.settings)
        }) && self.dependencies.iter().all(|(id, item)| project.item(*id) == Some(item))
    }

    pub fn interchange(&self) -> std::result::Result<Vec<u8>, String> {
        let (bytes, report) = filmcraft_interchange::export(&self.fragment, self.sequence, filmcraft_interchange::Format::Otio, &Default::default())
            .map_err(|e| e.to_string())?;
        if report.has_warnings() {
            return Err(format!("selection cannot be transferred faithfully: {report}"));
        }
        if bytes.len() > 64 * 1024 * 1024 {
            return Err("selection exceeds the 64 MiB handoff limit".into());
        }
        Ok(bytes)
    }

    pub fn apply(&self, session: &mut Session, link: LinkedComposition, cache: &CachedRender) -> Result<ItemId> {
        if !self.is_current(&session.project) {
            return Err(EngineError::Other(
                "timeline changed during handoff; original clips kept, saved EffectCraft project is available to link manually".into(),
            ));
        }
        let info = cache.source.info().clone();
        let source_size = info.video.as_ref().map(|v| (v.width, v.height)).ok_or_else(|| EngineError::Other("EffectCraft produced no video stream".into()))?;
        if info.duration < self.duration {
            return Err(EngineError::Other("EffectCraft render is shorter than the selected clips; original clips kept".into()));
        }
        session.edit("Replace with EffectCraft Composition", |project, state| {
            project.next_id.checked_add(2).ok_or_else(|| EngineError::Other("project item ids exhausted".into()))?;
            let item = project.add_item(
                "EffectCraft Composition",
                Label::Purple,
                ItemKind::Media(MediaClip {
                    media: MediaRef::File { path: cache.path.clone() },
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
            project.linked_compositions.insert(item, link);
            let mut replacement = project
                .make_track_item(item, TrackKind::Video, self.start, TimeRange::new(Tick::ZERO, self.duration), self.original.settings.frame_rate)
                .ok_or_else(|| EngineError::Other("could not create replacement clip".into()))?;
            for effect in &mut replacement.effects {
                filmcraft_project::resolve_auto_points(effect, source_size, (self.original.settings.width, self.original.settings.height));
            }
            let replacement_id = replacement.id;
            let seq = project.sequence_mut(self.sequence).ok_or(EngineError::NoSequence)?;
            for track in &mut seq.video_tracks {
                track.items.retain(|c| !self.selected.contains(&c.id));
                track.transitions.retain(|t| !t.from.is_some_and(|id| self.selected.contains(&id)) && !t.to.is_some_and(|id| self.selected.contains(&id)));
            }
            let track = seq.track_mut(self.target).ok_or_else(|| EngineError::Other("replacement track is missing".into()))?;
            track.items.push(replacement);
            track.items.sort_by_key(|c| c.start);
            state.selection = vec![replacement_id];
            state.project_selection = vec![item];
            Ok(item)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::time::Duration;
    use web_time::Instant;

    fn wait(session: &mut Session, ticket: &Value) -> Value {
        let key = format!("request:{ticket}");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            crate::linked_compositions::poll(session);
            if let Some(status) = session.links.status.get(&key)
                && matches!(status["state"].as_str(), Some("ready" | "error"))
            {
                return status.clone();
            }
            assert!(Instant::now() < deadline, "handoff timed out: {:?}", session.links.status);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn clip_effects_are_left_out_of_the_handoff_with_a_warning() {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        let clip = session.active_sequence().unwrap().video_tracks.iter().flat_map(|t| &t.items).find(|c| c.effect("drop_shadow").is_some()).unwrap().id;
        session.state.selection = vec![clip];
        let plan = Plan::prepare(&session).unwrap();
        assert_eq!(plan.dropped, ["Drop Shadow"]);
        assert!(plan.warning().unwrap().contains("Drop Shadow"));
        let sent = plan.fragment.sequence(plan.sequence).unwrap();
        let effects: Vec<_> = sent.video_tracks.iter().flat_map(|t| &t.items).flat_map(|c| &c.effects).map(|e| e.effect.as_str()).collect();
        assert!(!effects.contains(&"drop_shadow") && effects.contains(&"motion"), "{effects:?}");
        // the FilmCraft timeline itself keeps the effect until the replacement lands
        assert!(session.active_sequence().unwrap().video_tracks.iter().flat_map(|t| &t.items).any(|c| c.id == clip && c.effect("drop_shadow").is_some()));
    }

    #[test]
    fn transitions_on_the_selected_clips_are_named_for_removal() {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        let clip = session
            .active_sequence()
            .unwrap()
            .video_tracks
            .iter()
            .find_map(|t| t.transitions.iter().find(|tr| tr.effect.effect == "push").and_then(|tr| tr.to))
            .unwrap();
        session.state.selection = vec![clip];
        let plan = Plan::prepare(&session).unwrap();
        assert_eq!(plan.transitions_removed, ["Push transition"]);
        assert!(plan.warning().unwrap().contains("removed: Push transition"));
        // the timeline keeps it until the replacement lands
        assert!(session.active_sequence().unwrap().video_tracks.iter().any(|t| t.transitions.iter().any(|tr| tr.to == Some(clip))));
    }

    #[test]
    fn changed_timeline_aborts_replacement_without_overwriting_the_edit() {
        let mut session = Session::default();
        let result = session.execute("file.newColorMatte", json!({"color":"#ff8000","width":64,"height":64,"seconds":0.25})).unwrap();
        let media = ItemId(result["item"].as_u64().unwrap());
        session.execute("file.newSequence", json!({"fromItem":media.0})).unwrap();
        let sequence = session.state.active_sequence.unwrap();
        session.state.selection = vec![session.active_sequence().unwrap().video_tracks[0].items[0].id];
        let plan = Plan::prepare(&session).unwrap();
        assert!(plan.is_current(&session.project));
        let source = session.media.full_res_source(&session.project, media, session.services.as_ref()).unwrap();
        let cache = CachedRender { path: "/never-published.mov".into(), source };
        session
            .edit("User moves clip", |project, _| {
                project.sequence_mut(sequence).unwrap().video_tracks[0].items[0].start = Tick(1);
                Ok(())
            })
            .unwrap();
        let edited = session.project.clone();
        let link = LinkedComposition { project_path: "/new.ecproj".into(), composition_id: 1, baked: false };
        assert!(plan.apply(&mut session, link, &cache).is_err());
        assert_eq!(session.project, edited);
    }

    #[test]
    #[ignore = "requires FILMCRAFT_EFFECTCRAFT_TEST_CLI"]
    fn replace_trimmed_video_preserves_audio_pixels_and_undo() {
        for (animated, layered) in [(false, false), (true, false), (false, true)] {
            exercise_handoff(animated, layered);
        }
    }

    fn exercise_handoff(animated: bool, layered: bool) {
        let executable = std::env::var("FILMCRAFT_EFFECTCRAFT_TEST_CLI").expect("set CLI path");
        let dir = crate::media_test_util::tmp_dir("linked-handoff");
        let footage = dir.join("source.mov");
        crate::media_test_util::make_movie(&footage, filmcraft_media::DemoScene::Aurora, 64, 64, 12);
        let second = dir.join("overlay.mov");
        if layered {
            crate::media_test_util::make_movie(&second, filmcraft_media::DemoScene::OceanSunset, 64, 64, 12);
        }
        let files = if layered { vec![footage.as_path(), second.as_path()] } else { vec![footage.as_path()] };
        let (mut session, _, sequence) = crate::media_test_util::session_with(&files);
        session.links.cache_root = dir.join("cache");
        session.execute("link.configure", json!({"executable":executable})).unwrap();
        let rate = filmcraft_time::FrameRate::FPS_24;
        let project = Arc::make_mut(&mut session.project);
        let extra_track = filmcraft_project::TrackId(project.alloc_id());
        let seq = project.sequence_mut(sequence).unwrap();
        let clip = &mut seq.video_tracks[0].items[0];
        clip.start = rate.tick_of(5);
        clip.source_in = rate.tick_of(3);
        clip.duration = rate.tick_of(6);
        if animated {
            let opacity = clip.effects.iter_mut().find(|e| e.effect == "opacity").unwrap().param_mut("opacity").unwrap();
            opacity.keyframes = vec![
                filmcraft_project::Keyframe::new(rate.tick_of(3), filmcraft_project::ParamValue::Float(20.0)),
                filmcraft_project::Keyframe::new(rate.tick_of(9), filmcraft_project::ParamValue::Float(100.0)),
            ];
        }
        session.state.selection = vec![clip.id];
        if layered {
            let mut overlay = seq.video_tracks[0].items.remove(1);
            overlay.start = rate.tick_of(7);
            overlay.source_in = rate.tick_of(1);
            overlay.duration = rate.tick_of(3);
            overlay.effects.iter_mut().find(|e| e.effect == "opacity").unwrap().param_mut("opacity").unwrap().value =
                filmcraft_project::ParamValue::Float(50.0);
            session.state.selection.push(overlay.id);
            let mut track = filmcraft_project::Track::new(extra_track, TrackKind::Video, "Overlay".into());
            track.items.push(overlay);
            seq.video_tracks.push(track);
        }
        let before = session.project.clone();
        let audio = before.sequence(sequence).unwrap().audio_tracks.clone();
        let references: Vec<_> = [5, 7, 10].into_iter().map(|frame| crate::media_test_util::frame_rgba(&mut session, frame, 1.0).2).collect();
        let destination = dir.join("selection.ecproj");
        let result = session.execute("link.replace", json!({"path":destination})).unwrap();
        assert_eq!(session.project, before, "original remains until the asynchronous handoff succeeds");
        let status = wait(&mut session, &result["request"]);
        assert_eq!(status["state"], "ready", "{status}");
        assert!(destination.is_file());
        let seq = session.project.sequence(sequence).unwrap();
        assert_eq!(seq.audio_tracks, audio);
        assert_eq!(seq.video_tracks.iter().map(|t| t.items.len()).sum::<usize>(), 1);
        let replacement = seq.video_tracks.iter().flat_map(|t| &t.items).next().unwrap();
        assert_eq!((replacement.start, replacement.source_in, replacement.duration), (rate.tick_of(5), Tick::ZERO, rate.tick_of(6)));
        let link = &session.project.linked_compositions[&replacement.item];
        let ec: Value = serde_json::from_slice(&std::fs::read(&destination).unwrap()).unwrap();
        let composition = &ec["items"][link.composition_id.to_string()]["kind"];
        assert_eq!(composition["duration"], rate.tick_of(6).0);
        let layers = composition["layers"].as_array().unwrap();
        assert_eq!(layers.len(), if layered { 2 } else { 1 }, "visual-only handoff");
        let base = layers.iter().find(|layer| layer["in_point"] == 0).unwrap();
        assert_eq!(base["out_point"], rate.tick_of(6).0);
        assert_eq!(base["start_time"], -rate.tick_of(3).0);
        for (frame, reference) in [5, 7, 10].into_iter().zip(references) {
            let actual = crate::media_test_util::frame_rgba(&mut session, frame, 1.0).2;
            let psnr = crate::media_test_util::psnr(&reference, &actual);
            eprintln!("handoff animated={animated} layered={layered} frame {frame}: {psnr:.2} dB");
            assert!(psnr > 35.0, "handoff output differs at frame {frame}: {psnr} dB");
        }
        session.execute("edit.undo", json!({})).unwrap();
        assert_eq!(session.project, before, "one undo restores the exact original project");
        assert!(destination.exists(), "undo must not delete the editable project");
        let sentinel = std::fs::read(&destination).unwrap();
        session.state.selection = before.sequence(sequence).unwrap().video_tracks.iter().flat_map(|t| &t.items).map(|c| c.id).collect();
        let result = session.execute("link.replace", json!({"path":destination})).unwrap();
        let status = wait(&mut session, &result["request"]);
        assert_eq!(status["state"], "error");
        assert_eq!(std::fs::read(&destination).unwrap(), sentinel, "existing project never overwritten");
        assert_eq!(session.project, before);
    }
}
