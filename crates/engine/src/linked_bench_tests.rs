//! Opt-in measurements for the native saved-composition path, not a real-time promise.
use crate::Session;
use filmcraft_project::{ItemId, MediaRef};
use filmcraft_time::FrameRate;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires FILMCRAFT_EFFECTCRAFT_TEST_CLI; writes synthetic benchmark report"]
fn measure_saved_link_preview_and_full_quality_export() {
    let executable = std::env::var("FILMCRAFT_EFFECTCRAFT_TEST_CLI").expect("set CLI path");
    let root = crate::media_test_util::tmp_dir("link-benchmark");
    let project_path = root.join("title.ecproj");
    let created = std::process::Command::new(&executable)
        .args([
            "run",
            "comp.new",
            r##"{"name":"Synthetic title","width":1920,"height":1080,"frameRate":24,"duration":1}"##,
            "layer.newSolid",
            r##"{"name":"Orange rectangle","width":960,"height":540,"color":"#ff8000"}"##,
            "--empty",
            "--save-as",
        ])
        .arg(&project_path)
        .output()
        .unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let mut session = Session::default();
    session.links.cache_root = root.join("cache");
    session.execute("link.configure", json!({"executable":executable})).unwrap();
    let start = Instant::now();
    let imported = session.execute("link.import", json!({"path":project_path,"composition":1,"wait":true})).unwrap();
    let initial_ms = start.elapsed().as_secs_f64() * 1000.0;
    let item = ItemId(imported["item"].as_u64().unwrap());
    session.execute("file.newSequence", json!({"fromItem":item.0,"name":"Linked benchmark"})).unwrap();
    let rate = FrameRate::FPS_24;
    let mut scrub_ms = Vec::new();
    for frame in [23, 0, 12, 2, 20, 5] {
        session.set_playhead(rate.tick_of(frame));
        let start = Instant::now();
        let picture = session.render_program(0.5).unwrap();
        assert_eq!((picture.w, picture.h), (960, 540));
        scrub_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let start = Instant::now();
    for frame in 0..24 {
        session.set_playhead(rate.tick_of(frame));
        let _ = session.render_program(0.5).unwrap();
    }
    let playback_ms = start.elapsed().as_secs_f64() * 1000.0;
    let old_path = match &session.project.item(item).unwrap().as_media().unwrap().media {
        MediaRef::File { path } => path.clone(),
        _ => panic!("cache must be a movie"),
    };
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&project_path).unwrap()).unwrap();
    for value in doc["items"].as_object_mut().unwrap().values_mut() {
        if value["kind"]["type"] == "Solid" {
            value["kind"]["color"] = json!([0.0, 1.0, 0.0]);
        }
    }
    let start = Instant::now();
    filmcraft_format::atomic_write(&project_path, &serde_json::to_vec(&doc).unwrap()).unwrap();
    loop {
        session.poll_persistence();
        let current = &session.project.item(item).unwrap().as_media().unwrap().media;
        if matches!(current, MediaRef::File { path } if path != &old_path) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(120), "automatic save refresh timed out: {:?}", session.links.status);
        std::thread::sleep(Duration::from_millis(10));
    }
    let refresh_ms = start.elapsed().as_secs_f64() * 1000.0;
    let output = root.join("final.mov");
    let start = Instant::now();
    session.execute("file.exportMedia", json!({"path":output,"format":"prores","audio":false,"wait":true})).unwrap();
    let export_ms = start.elapsed().as_secs_f64() * 1000.0;
    let source = session.media.probe_file(output.to_str().unwrap(), session.services.as_ref()).unwrap();
    let video = source.info().video.as_ref().unwrap();
    assert_eq!((video.width, video.height), (1920, 1080), "export is full quality after half-resolution previews");
    let report = json!({
        "scene":"one 960x540 solid centered in a transparent 1920x1080 composition, 24 frames at 24 fps",
        "build":"FilmCraft engine tests with external EffectCraft renderer, CPU rendering", "architecture":std::env::consts::ARCH,
        "renderer":executable, "initialLinkMs":initial_ms, "automaticSaveRefreshMs":refresh_ms,
        "halfResolutionScrubMs":scrub_ms, "halfResolutionSequential24FramesMs":playback_ms,
        "fullResolutionExportMs":export_ms,
        "limitations":"synthetic simple scene; synchronous engine frame requests exclude desktop presentation, audio and real-world projects"
    });
    let report_dir = filmcraft_testkit::workspace_root().join("target/fixtures/dynamic-link");
    std::fs::create_dir_all(&report_dir).unwrap();
    std::fs::write(report_dir.join("benchmark.json"), serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    eprintln!("{report}");
}
