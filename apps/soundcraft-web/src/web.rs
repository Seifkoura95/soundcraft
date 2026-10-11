//! The browser shell: the eframe web runner around [`SoundApp`].

use soundcraft_engine::Engine;
use soundcraft_ui_egui::{Services, SoundApp};
use std::sync::Arc;
#[cfg(not(feature = "simplehub"))]
use wasm_bindgen::JsCast as _;

#[cfg(not(feature = "simplehub"))]
const CANVAS_ID: &str = "soundcraft_canvas";
#[cfg(not(feature = "simplehub"))]
const LOADING_ID: &str = "soundcraft_loading";

/// The standalone page: the app on the page's own canvas, on the demo session (`?empty` for a
/// blank one), with no file services (there is no file system to speak of).
#[cfg(not(feature = "simplehub"))]
pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            log::error!("missing <canvas id=\"{CANVAS_ID}\">");
            return;
        };
        let mut options = eframe::WebOptions::default();
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(move |cc| {
                    log_backend(cc);
                    let engine = if query().contains("empty") { Engine::default() } else { soundcraft_engine::demo::demo_engine() };
                    Ok(Box::new(WebShell(app_with(engine, Services::default()))))
                }),
            )
            .await;
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match result {
                Ok(()) => el.remove(),
                Err(e) => el.set_inner_html(&format!(
                    "<p>{} failed to start: {e:?}</p><p>A browser with WebGPU or WebGL2 is required.</p>",
                    soundcraft_ui_egui::APP_NAME
                )),
            }
        }
    });
}

#[cfg(not(feature = "simplehub"))]
fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}

fn log_backend(cc: &eframe::CreationContext<'_>) {
    if let Some(rs) = &cc.wgpu_render_state {
        log::info!("soundcraft-web: wgpu backend {:?}", rs.adapter.get_info().backend);
    }
}

/// The app on `engine`, playing through WebAudio (cpal's `wasm-bindgen` backend).
fn app_with(engine: Engine, services: Services) -> SoundApp {
    let player = soundcraft_playback::Player::new(Arc::new(engine.session().clone()));
    SoundApp::new(engine, Some(player), services)
}

struct WebShell(SoundApp);

impl eframe::App for WebShell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.0.logic(ctx);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.0.raw_input_hook(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(ui);
    }
}

/// Simple Audio inside SimpleHub. The page mounts the app on a canvas it owns and hands over a
/// bridge object (`open(purpose): Promise<{name, bytes}|null>`, `save(name, bytes): Promise<void>`)
/// that moves files to and from Simple Cloud; nothing here touches the browser's file picker.
/// `purpose` is `"open"` for File › Open (a `.scraft.zip` session bundle or a bare `.scraft`),
/// `"import"` for File › Import › Audio, `"midi"` for Import › MIDI and `"video"` for Import ›
/// Video, so the host can filter what it offers. Save, Bounce and the exports call `save`.
/// Playback and recording go through WebAudio as on the standalone page. The `#[wasm_bindgen]`
/// items here are the module's JS exports (`start_simple_audio`, `AudioHandle`).
#[cfg(feature = "simplehub")]
mod simplehub {
    use super::{Engine, Services, WebShell, app_with, log_backend};
    use soundcraft_ui_egui::hostfiles::{Arrival, Inbox};
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    /// A running instance. SimpleHub calls `destroy` when the module unmounts.
    #[wasm_bindgen]
    pub struct AudioHandle {
        runner: eframe::WebRunner,
    }

    #[wasm_bindgen]
    impl AudioHandle {
        pub fn destroy(&self) {
            self.runner.destroy();
        }

        pub fn has_panicked(&self) -> bool {
            self.runner.has_panicked()
        }
    }

    /// Starts Simple Audio on `canvas`, with `bridge` providing open and save.
    #[wasm_bindgen]
    pub async fn start_simple_audio(canvas: web_sys::HtmlCanvasElement, bridge: JsValue) -> Result<AudioHandle, JsValue> {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        let runner = eframe::WebRunner::new();
        runner
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(move |cc| {
                    log_backend(cc);
                    let inbox: Inbox = Inbox::default();
                    let app = app_with(Engine::default(), services(bridge, inbox, cc.egui_ctx.clone()));
                    Ok(Box::new(WebShell(app)))
                }),
            )
            .await?;
        Ok(AudioHandle { runner })
    }

    /// Files through the bridge: `open(purpose)` for File › Open and the imports (the bytes go to
    /// the inbox), `save(name, bytes)` for Save, Bounce and the exports (fire and forget).
    fn services(bridge: JsValue, inbox: Inbox, ctx: egui::Context) -> Services {
        let open_bridge = bridge.clone();
        let open_inbox = inbox.clone();
        Services {
            open_async: Some(Box::new(move |purpose: &str| {
                let bridge = open_bridge.clone();
                let inbox = open_inbox.clone();
                let ctx = ctx.clone();
                let purpose = purpose.to_string();
                wasm_bindgen_futures::spawn_local(async move {
                    match call_open(&bridge, &purpose).await {
                        Ok(Some((name, bytes))) => {
                            inbox.lock().unwrap_or_else(|e| e.into_inner()).push(Arrival { purpose, name, bytes });
                            ctx.request_repaint();
                        }
                        Ok(None) => {}
                        Err(e) => log::error!("SimpleHub open ({purpose}) failed: {e:?}"),
                    }
                });
            })),
            download: Some(Box::new(move |name: &str, bytes: &[u8]| {
                let bridge = bridge.clone();
                let name = name.to_string();
                let bytes = js_sys::Uint8Array::from(bytes);
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(e) = call_save(&bridge, &name, &bytes).await {
                        log::error!("SimpleHub save of {name} failed: {e:?}");
                    }
                });
            })),
            inbox: Some(inbox),
            ..Default::default()
        }
    }

    fn method(bridge: &JsValue, name: &str) -> Result<js_sys::Function, JsValue> {
        js_sys::Reflect::get(bridge, &JsValue::from_str(name))?
            .dyn_into::<js_sys::Function>()
            .map_err(|_| JsValue::from_str(&format!("bridge.{name} is not a function")))
    }

    async fn call_open(bridge: &JsValue, purpose: &str) -> Result<Option<(String, Vec<u8>)>, JsValue> {
        let promise: js_sys::Promise = method(bridge, "open")?.call1(bridge, &JsValue::from_str(purpose))?.dyn_into()?;
        let result = JsFuture::from(promise).await?;
        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }
        let name = js_sys::Reflect::get(&result, &JsValue::from_str("name"))?.as_string().unwrap_or_else(|| "Untitled".into());
        let bytes: js_sys::Uint8Array = js_sys::Reflect::get(&result, &JsValue::from_str("bytes"))?.dyn_into()?;
        Ok(Some((name, bytes.to_vec())))
    }

    async fn call_save(bridge: &JsValue, name: &str, bytes: &js_sys::Uint8Array) -> Result<(), JsValue> {
        let ret = method(bridge, "save")?.call2(bridge, &JsValue::from_str(name), bytes)?;
        if let Ok(promise) = ret.dyn_into::<js_sys::Promise>() {
            JsFuture::from(promise).await?;
        }
        Ok(())
    }
}
