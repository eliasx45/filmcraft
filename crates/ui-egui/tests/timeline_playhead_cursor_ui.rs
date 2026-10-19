//! Headless UI tests for Move Playhead to Cursor (Premiere's `move.cti.to.cursor`, R in the
//! Elias Premiere preset).

use egui::pos2;
use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_time::Tick;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::panels::timeline::playhead_to_pointer;
use serde_json::json;

fn demo() -> Harness<'static, FilmcraftApp> {
    let mut session = Session::default();
    session.execute("file.openDemoProject", json!({})).expect("demo project");
    session.execute("sequence.snap", json!({"on": false})).expect("snapping off");
    let app = FilmcraftApp::new(session);
    let mut harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_step_dt(1.0 / 60.0).build_eframe(move |_cc| app);
    for _ in 0..4 {
        harness.step();
    }
    harness
}

#[test]
fn the_playhead_moves_to_the_frame_under_the_pointer() {
    let mut h = demo();
    let layout = h.state().tl.layout.clone().expect("timeline laid out");
    let row = layout.rows.first().expect("a track row").rect;
    let x = layout.x_of(Tick::from_seconds_f64(3.0));
    let app = h.state_mut();
    app.session.set_playhead(Tick::ZERO);
    assert!(playhead_to_pointer(app, pos2(x, row.center().y)));
    let rate = app.session.active_sequence().expect("sequence").settings.frame_rate;
    assert_eq!(app.session.playhead(), rate.snap_nearest(Tick::from_seconds_f64(3.0)));
}

#[test]
fn a_pointer_outside_the_timeline_leaves_the_playhead() {
    let mut h = demo();
    let layout = h.state().tl.layout.clone().expect("timeline laid out");
    let app = h.state_mut();
    let start = app.session.playhead();
    // left of the track content (over the track headers) and above the ruler
    assert!(!playhead_to_pointer(app, pos2(layout.content.min.x - 5.0, layout.content.center().y)));
    assert!(!playhead_to_pointer(app, pos2(layout.content.center().x, layout.ruler.min.y - 5.0)));
    assert_eq!(app.session.playhead(), start);
}
