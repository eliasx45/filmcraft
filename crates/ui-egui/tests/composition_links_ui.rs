#![cfg(not(target_arch = "wasm32"))]

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::{FilmcraftApp, control::ControlRequest};
use serde_json::{Value, json};
use std::sync::mpsc;

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: mpsc::Sender<ControlRequest>,
}
impl Driver {
    fn step(&mut self) {
        let ctx = self.harness.ctx.clone();
        let mut input = std::mem::take(self.harness.input_mut());
        eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut input);
        *self.harness.input_mut() = input;
        self.harness.step();
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        let (request, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(request).unwrap();
        for _ in 0..600 {
            self.step();
            if let Ok(result) = reply.try_recv() {
                assert_eq!(result["ok"], true, "{method} {params}: {result}");
                for _ in 0..3 {
                    self.step();
                }
                return result["result"].clone();
            }
        }
        panic!("no reply to {method}");
    }
}

#[test]
fn composition_picker_reads_names_and_exposes_automation_controls() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("composition-picker");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("titles.ecproj");
    std::fs::write(&source, br#"{"schema":2,"items":{"7":{"name":"Opening title","kind":{"type":"Comp"}},"9":{"name":"Lower third","kind":{"type":"Comp"}}}}"#)
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let mut app = FilmcraftApp::new(Session::default()).with_control(rx);
    app.hooks.pick_files = Some(Box::new(move |_| vec![source.to_string_lossy().into_owned()]));
    let harness = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).with_max_steps(10000).build_eframe(move |_| app);
    let mut driver = Driver { harness, tx };
    driver.call("ui.menu.invoke", json!({"id":"link.import","params":{}}));
    driver.call("ui.click", json!({"id":"effectcraftLink.browse"}));
    driver.call("ui.click", json!({"id":"effectcraftLink.load"}));
    for _ in 0..300 {
        driver.step();
        if driver.harness.state().auto.find("effectcraftLink.composition.9").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(driver.harness.state().auto.find("effectcraftLink.composition.7").is_some());
    assert!(driver.harness.state().auto.find("effectcraftLink.composition.9").is_some());
    driver.call("ui.click", json!({"id":"effectcraftLink.composition.9"}));
    assert!(driver.harness.state().auto.find("effectcraftLink.apply").unwrap().enabled.unwrap());
    assert!(driver.harness.state().ui_error.is_none());
    if let Some(dir) = std::env::var_os("FILMCRAFT_UI_SHOTS") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        driver.harness.render().unwrap().save(dir.join("composition-link-picker.png")).unwrap();
    }
    driver.call("engine.execute", json!({"command":"file.newProject","params":{"name":"Other document"}}));
    assert_eq!(driver.harness.state().session.project.name, "Other document");
    assert!(driver.harness.state().auto.find("effectcraftLink.apply").is_none(), "a link dialog must not target the next document");
}

#[test]
#[ignore = "requires FILMCRAFT_EFFECTCRAFT_TEST_CLI"]
fn replace_menu_keeps_original_until_link_is_ready() {
    let executable = std::env::var("FILMCRAFT_EFFECTCRAFT_TEST_CLI").expect("set CLI path");
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("composition-replacement-{nonce}"));
    std::fs::create_dir_all(&root).unwrap();
    let destination = root.join("Editable title.ecproj");
    let mut session = Session::default();
    session.links.cache_root = root.join("cache");
    session.execute("link.configure", json!({"executable":executable})).unwrap();
    let media = session.execute("file.newColorMatte", json!({"color":"#ff8000","width":64,"height":64,"seconds":0.25})).unwrap();
    session.execute("file.newSequence", json!({"fromItem":media["item"],"name":"Link workflow"})).unwrap();
    let clip = session.active_sequence().unwrap().video_tracks[0].items[0].id;
    session.state.selection = vec![clip];
    let original = session.project.clone();
    let (tx, rx) = mpsc::channel();
    let mut app = FilmcraftApp::new(session).with_control(rx);
    let picked = destination.clone();
    app.hooks.pick_save_as = Some(Box::new(move |_, _, _| Some(picked.to_string_lossy().into_owned())));
    let harness = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).with_max_steps(10000).build_eframe(move |_| app);
    let mut driver = Driver { harness, tx };
    driver.call("ui.menu.invoke", json!({"id":"link.replace"}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        driver.step();
        if !driver.harness.state().session.project.linked_compositions.is_empty() {
            break;
        }
        assert_eq!(driver.harness.state().session.project, original);
        let errors: Vec<_> = driver.harness.state().session.links.status.values().filter(|v| v["state"] == "error").collect();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(std::time::Instant::now() < deadline, "replacement timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(destination.exists());
    let session = &driver.harness.state().session;
    let clip = session
        .active_sequence()
        .unwrap()
        .video_tracks
        .iter()
        .flat_map(|track| &track.items)
        .find(|clip| session.project.linked_compositions.contains_key(&clip.item))
        .unwrap();
    let motion = clip.effects.iter().find(|effect| effect.effect == "motion").unwrap();
    for name in ["position", "anchor"] {
        let value = motion.param(name).unwrap().value.as_vec2().unwrap();
        assert!(value.x.is_finite() && value.y.is_finite(), "replacement inspector coordinates must be finite");
    }
    for _ in 0..15 {
        driver.step();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(driver.harness.state().ui_error.is_none());
    assert!(driver.harness.state().auto.find("effectcraftLinks.link.refresh").is_some());
    if let Some(dir) = std::env::var_os("FILMCRAFT_UI_SHOTS") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        driver.harness.render().unwrap().save(dir.join("composition-replacement.png")).unwrap();
    }
    // A normal EffectCraft save must update the displayed Program picture without Refresh.
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&destination).unwrap()).unwrap();
    for item in saved["items"].as_object_mut().unwrap().values_mut() {
        if item["kind"]["type"] == "Solid" {
            item["kind"]["color"] = json!([0.0, 1.0, 0.0]);
        }
    }
    std::fs::write(&destination, serde_json::to_vec(&saved).unwrap()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        for _ in 0..5 {
            driver.step();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let rect = driver.harness.state().auto.find("program.picture").unwrap().rect;
        let scale = driver.harness.ctx.pixels_per_point();
        let x = ((rect[0] + rect[2] * 0.37) * scale) as u32;
        let y = ((rect[1] + rect[3] * 0.43) * scale) as u32;
        let image = driver.harness.render().unwrap();
        let pixel = image.get_pixel(x, y).0;
        if pixel[1] > 180 && pixel[0] < 60 && pixel[2] < 60 {
            if let Some(dir) = std::env::var_os("FILMCRAFT_UI_SHOTS") {
                image.save(std::path::PathBuf::from(dir).join("composition-updated-after-save.png")).unwrap();
            }
            break;
        }
        assert!(std::time::Instant::now() < deadline, "saved source did not update the Program picture: {pixel:?}");
    }
    driver.call("ui.menu.invoke", json!({"id":"edit.undo"}));
    assert_eq!(driver.harness.state().session.project, original);
    assert!(destination.exists());
}
