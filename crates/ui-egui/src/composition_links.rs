//! Native composition picker and saved-link status. Rendering remains in the engine.
use crate::{FilmcraftApp, native_dialogs::FileDialog};
use filmcraft_engine::linked_render::CompositionChoice;
use filmcraft_project::ItemId;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Choices = Arc<Mutex<Option<Result<Vec<CompositionChoice>, String>>>>;
#[derive(Clone, Default)]
struct Draft {
    document_generation: u64,
    path: String,
    renderer: String,
    item: Option<ItemId>,
    original_link: Option<filmcraft_project::linked_composition::LinkedComposition>,
    selected: Option<u64>,
    choices: Vec<CompositionChoice>,
    loading: Option<Choices>,
    error: String,
}
fn draft_id() -> egui::Id {
    egui::Id::new("effectcraft-link-dialog")
}

pub fn open(app: &FilmcraftApp, ctx: &egui::Context, relink: bool) -> Result<Value, String> {
    let item = if relink { Some(*app.session.state.project_selection.first().ok_or("select a linked composition in the Project panel")?) } else { None };
    let mut draft = Draft {
        document_generation: app.session.links.document_generation,
        renderer: app.session.links.executable.to_string_lossy().into_owned(),
        item,
        ..Default::default()
    };
    if let Some(id) = item {
        let link = app.session.project.linked_compositions.get(&id).ok_or("selected item is not a linked composition")?;
        draft.original_link = Some(link.clone());
        draft.path = link.project_path.clone();
        draft.selected = Some(link.composition_id);
    }
    ctx.data_mut(|m| m.insert_temp(draft_id(), Some(draft)));
    Ok(json!({"dialog":"effectcraftLink"}))
}

fn refusal_id() -> egui::Id {
    egui::Id::new("effectcraft-replace-refused")
}

/// Shows why Replace with EffectCraft Composition didn't start, in a dialog (the status bar
/// line was easy to miss).
pub fn refuse(ctx: &egui::Context, message: &str) {
    ctx.data_mut(|m| m.insert_temp(refusal_id(), Some(message.to_string())));
}

fn refusal(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let Some(Some(message)) = ctx.data(|m| m.get_temp::<Option<String>>(refusal_id())) else { return };
    let message = message.strip_prefix("Replace with EffectCraft Composition: ").unwrap_or(&message).to_string();
    let mut close = false;
    let modal = egui::Modal::new(refusal_id()).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading(tl!("Replace with EffectCraft Composition"));
        ui.add_space(8.0);
        let label = ui.label(&message);
        app.auto.add_response("effectcraftReplace.refused", &label, &message);
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let ok = ui.button(tl!("OK"));
            app.auto.add_response("effectcraftReplace.ok", &ok, "Close");
            close |= ok.clicked();
        });
    });
    if close || modal.should_close() {
        ctx.data_mut(|m| m.insert_temp::<Option<String>>(refusal_id(), None));
    }
}

pub fn show(app: &mut FilmcraftApp, ctx: &egui::Context) {
    refusal(app, ctx);
    manager(app, ctx);
    let Some(Some(mut draft)) = ctx.data(|m| m.get_temp::<Option<Draft>>(draft_id())) else { return };
    if draft.document_generation != app.session.links.document_generation
        || draft.item.is_some_and(|item| app.session.project.linked_compositions.get(&item) != draft.original_link.as_ref())
    {
        ctx.data_mut(|m| m.insert_temp::<Option<Draft>>(draft_id(), None));
        return;
    }
    if let Some(result) = draft.loading.as_ref().and_then(|l| l.lock().unwrap_or_else(|e| e.into_inner()).take()) {
        draft.loading = None;
        match result {
            Ok(choices) => {
                if !choices.iter().any(|c| Some(c.id) == draft.selected) {
                    draft.selected = choices.first().map(|c| c.id);
                }
                draft.choices = choices;
                draft.error.clear();
            }
            Err(error) => {
                draft.error = error;
                draft.choices.clear();
                draft.selected = None;
            }
        }
    }
    let mut open = true;
    let (mut browse, mut scan, mut apply, mut renderer_browse) = (false, false, false, false);
    egui::Window::new(if draft.item.is_some() { tl!("Relink EffectCraft Composition") } else { tl!("Link EffectCraft Composition") })
        .id(draft_id())
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(crate::widgets::dialog_center(ctx))
        .show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(tl!("Choose a saved EffectCraft project. FilmCraft updates its picture after each save."));
            ui.horizontal(|ui| {
                let response = ui.text_edit_singleline(&mut draft.path);
                app.auto.add_response("effectcraftLink.path", &response, "EffectCraft project");
                if response.changed() {
                    draft.choices.clear();
                    draft.selected = None;
                    draft.loading = None;
                }
                let response = ui.button(tl!("Browse…"));
                app.auto.add_response("effectcraftLink.browse", &response, "Choose EffectCraft project");
                browse = response.clicked();
            });
            let response = ui.add_enabled(draft.loading.is_none() && !draft.path.is_empty(), egui::Button::new(tl!("Load compositions")));
            app.auto.add_response("effectcraftLink.load", &response, tl!("Load compositions"));
            scan = response.clicked();
            if draft.loading.is_some() {
                ui.label(tl!("Reading project…"));
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            egui::ScrollArea::vertical().max_height(180.0).show_rows(ui, 22.0, draft.choices.len(), |ui, rows| {
                for index in rows {
                    let Some(choice) = draft.choices.get(index) else {
                        continue;
                    };
                    let response = ui.radio_value(&mut draft.selected, Some(choice.id), &choice.name);
                    app.auto.add_response(&format!("effectcraftLink.composition.{}", choice.id), &response, &choice.name);
                }
            });
            ui.separator();
            ui.label(tl!("Visual only · Audio stays in FilmCraft"));
            let section = ui.collapsing(tl!("EffectCraft renderer setup"), |ui| {
                ui.label(tl!("Choose the EffectCraft command-line executable. FilmCraft remembers it."));
                let response = ui.text_edit_singleline(&mut draft.renderer);
                app.auto.add_response("effectcraftLink.renderer", &response, "EffectCraft executable");
                let response = ui.button(tl!("Choose executable…"));
                app.auto.add_response("effectcraftLink.rendererBrowse", &response, "Choose EffectCraft executable");
                renderer_browse = response.clicked();
            });
            app.auto.add_response("effectcraftLink.setup", &section.header_response, tl!("EffectCraft renderer setup"));
            if !draft.error.is_empty() {
                ui.colored_label(egui::Color32::LIGHT_RED, &draft.error);
            }
            let response = ui.add_enabled(
                draft.selected.is_some() && draft.loading.is_none(),
                egui::Button::new(if draft.item.is_some() { tl!("Relink composition") } else { tl!("Link composition") }),
            );
            app.auto.add_response("effectcraftLink.apply", &response, tl!("Link composition"));
            apply = response.clicked();
        });
    if scan {
        let result: Choices = Arc::default();
        let worker_result = result.clone();
        let path = draft.path.clone();
        match std::thread::Builder::new().name("effectcraft-composition-picker".into()).spawn(move || {
            let result = std::panic::catch_unwind(|| filmcraft_engine::linked_render::compositions(std::path::Path::new(&path)))
                .unwrap_or_else(|_| Err("could not read EffectCraft project; see crash log".into()));
            *worker_result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        }) {
            Ok(_) => {
                draft.loading = Some(result);
                draft.error.clear();
            }
            Err(error) => draft.error = error.to_string(),
        }
    }
    if apply {
        let result = (|| {
            if draft.renderer != app.session.links.executable.to_string_lossy() {
                app.session.execute("link.configure", json!({"executable":draft.renderer})).map_err(|e| e.to_string())?;
            }
            let command = if draft.item.is_some() { "link.relink" } else { "link.import" };
            app.session.execute(command, json!({"path":draft.path,"composition":draft.selected,"item":draft.item.map(|i| i.0)})).map_err(|e| e.to_string())
        })();
        match result {
            Ok(_) => {
                open = false;
                open_manager(ctx);
                app.ui.status = tl!("Updating EffectCraft composition…").into();
            }
            Err(error) => draft.error = error,
        }
    }
    if crate::widgets::escape_closes(ctx) {
        open = false;
    }
    ctx.data_mut(|m| m.insert_temp(draft_id(), open.then_some(draft)));
    if browse || renderer_browse {
        let ctx = ctx.clone();
        let dialog = if renderer_browse { FileDialog::open_files(&[]) } else { FileDialog::open_files(&["ecproj"]) };
        if let Err(error) = app.pick(dialog, move |_, paths| {
            if let Some(path) = paths.first() {
                ctx.data_mut(|m| {
                    if let Some(Some(mut draft)) = m.get_temp::<Option<Draft>>(draft_id()) {
                        if renderer_browse {
                            draft.renderer = path.clone();
                        } else {
                            draft.path = path.clone();
                            draft.choices.clear();
                            draft.selected = None;
                            draft.loading = None;
                        }
                        m.insert_temp(draft_id(), Some(draft));
                    }
                });
            }
            Ok(Value::Null)
        }) {
            app.ui.status = error;
        }
    }
}

pub fn open_manager(ctx: &egui::Context) {
    ctx.data_mut(|m| m.insert_temp(egui::Id::new("effectcraft-links-manager"), true));
}

pub fn hint(app: &FilmcraftApp) -> Option<String> {
    let states = &app.session.links.status;
    if let Some(error) = states.values().find(|v| v["state"] == "error") {
        return Some(
            tlf!("EffectCraft link needs attention — {error}", error = error["error"].as_str().unwrap_or(tl!("open EffectCraft Links for details")))
                .chars()
                .take(180)
                .collect(),
        );
    }
    if let Some(warning) = states.values().find_map(|v| v["warning"].as_str()) {
        return Some(tlf!("EffectCraft link — {warning}", warning = warning).chars().take(180).collect());
    }
    if states.values().any(|v| v["state"] == "updating" || v["state"] == "baking") {
        return Some(tl!("Updating EffectCraft links — showing the last completed picture").into());
    }
    if states.values().any(|v| v["state"] == "cancelled") {
        return Some(tl!("EffectCraft updates paused — showing the last completed picture").into());
    }
    None
}

fn manager(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let id = egui::Id::new("effectcraft-links-manager");
    if !ctx.data(|m| m.get_temp::<bool>(id)).unwrap_or(false) {
        return;
    }
    let mut open = true;
    let mut action = None;
    let mut open_source = None;
    let mut relink = None;
    egui::Window::new(tl!("EffectCraft Links")).id(id).open(&mut open).default_width(520.0).show(ctx, |ui| {
        ui.label(tl!("Saved changes update automatically. An older picture stays visible while an update renders."));
        ui.horizontal(|ui| {
            for (command, label) in [("link.refresh", tl!("Refresh links")), ("link.cancel", tl!("Cancel updates"))] {
                let response = ui.button(label);
                app.auto.add_response(&format!("effectcraftLinks.{command}"), &response, label);
                if response.clicked() {
                    action = Some((command, json!({})));
                }
            }
        });
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            for (item, link) in &app.session.project.linked_compositions {
                let Some(media) = app.session.project.item(*item) else { continue };
                ui.separator();
                ui.strong(&media.name);
                let status = app.session.links.status.get(&format!("item:{}", item.0));
                let state = if link.baked {
                    tl!("Baked — automatic updates off")
                } else {
                    match status.and_then(|v| v["state"].as_str()) {
                        Some("ready") => tl!("Up to date"),
                        Some("error") => tl!("Update failed — previous picture retained"),
                        Some("cancelled") => tl!("Updates cancelled — previous picture retained"),
                        Some("baking") => tl!("Preparing baked movie…"),
                        _ => tl!("Checking / updating…"),
                    }
                };
                let response = ui.label(state);
                app.auto.add_response(&format!("effectcraftLinks.{}.status", item.0), &response, state);
                if let Some(error) = status.and_then(|v| v["error"].as_str()) {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if let Some(warning) = status.and_then(|v| v["warning"].as_str()) {
                    ui.colored_label(egui::Color32::from_rgb(240, 190, 80), warning);
                }
                ui.small(&link.project_path);
                ui.horizontal(|ui| {
                    let response = ui.button(tl!("Open source project"));
                    app.auto.add_response(&format!("effectcraftLinks.{}.open", item.0), &response, "Open source in its application");
                    if response.clicked() {
                        open_source = Some(link.project_path.clone());
                    }
                    let response = ui.button(tl!("Relink…"));
                    app.auto.add_response(&format!("effectcraftLinks.{}.relink", item.0), &response, tl!("Relink composition"));
                    if response.clicked() {
                        relink = Some(*item);
                    }
                    let label = if link.baked { tl!("Restore link") } else { tl!("Bake movie") };
                    let response = ui.button(label);
                    app.auto.add_response(&format!("effectcraftLinks.{}.bake", item.0), &response, label);
                    if response.clicked() {
                        action = Some((if link.baked { "link.restore" } else { "link.bake" }, json!({"item":item.0})));
                    }
                });
            }
            for (key, status) in &app.session.links.status {
                if key.starts_with("request:") && status["state"] != "ready" {
                    ui.separator();
                    ui.label(if status["state"] == "updating" { tl!("Importing composition…") } else { tl!("Composition import needs attention") });
                    if let Some(error) = status["error"].as_str() {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                }
            }
        });
    });
    ctx.data_mut(|m| m.insert_temp(id, open));
    if let Some((command, params)) = action
        && let Err(error) = app.session.execute(command, params)
    {
        app.ui.status = error.to_string();
    }
    if let Some(path) = open_source
        && let Err(error) = crate::panels::menu_dialogs::open_path(app, ctx, &path, false)
    {
        app.ui.status = error;
    }
    if let Some(item) = relink {
        app.session.state.project_selection = vec![item];
        if let Err(error) = self::open(app, ctx, true) {
            app.ui.status = error;
        }
    }
}
