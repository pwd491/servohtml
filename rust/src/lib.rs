//! csweb: C ABI around Servo for embedding into a CS16Client

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use libservo::{
    Code, DeviceIntPoint, DeviceIntRect, DevicePoint, InputEvent, Key, KeyState, KeyboardEvent,
    Location, Modifiers, MouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    NamedKey, RenderingContext, Servo, ServoBuilder, SoftwareRenderingContext, WebView,
    WebViewBuilder, WebViewDelegate, WebViewPoint, WheelDelta, WheelEvent, WheelMode,
};
use url::Url;

const ABI_VERSION: u32 = 1;

/// how often the worker wakes up to pump Servo when there are no commands.
const POLL_INTERVAL: Duration = Duration::from_millis(4);
/// upper bound for repaint + CPU readback per view (about 60 fps).
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);

type LogCallback = Option<unsafe extern "C" fn(level: i32, msg: *const c_char, ...)>;

#[repr(C)]
pub struct ServoViewConfig {
    pub abi_version: u32,
    pub width: u32,
    pub height: u32,
    pub url: *const c_char,
    pub resources_dir: *const c_char,
    pub native_display: *mut c_void,
    pub share_context: *mut c_void,
    pub log: LogCallback,
}

#[repr(C)]
pub struct ServoViewFrame {
    pub gl_texture: u32,
    pub width: u32,
    pub height: u32,
    pub frame_id: u64,
    pub flipped_y: i32,
    pub pixels: *const u8,
    pub stride: u32,
}

// ---------------------------------------------------------------------------
// Shared state and commands
// ---------------------------------------------------------------------------

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Latest frame produced by the worker.
#[derive(Default)]
struct Shared {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    frame_id: u64,
}

enum Cmd {
    Create {
        id: u64,
        width: u32,
        height: u32,
        url: Url,
        shared: Arc<Mutex<Shared>>,
        reply: Sender<Result<(), String>>,
    },
    Destroy {
        id: u64,
        done: Sender<()>,
    },
    Resize {
        id: u64,
        width: u32,
        height: u32,
    },
    Load {
        id: u64,
        url: Url,
    },
    MouseMove {
        id: u64,
        x: f32,
        y: f32,
    },
    MouseButton {
        id: u64,
        button: i32,
        pressed: bool,
        x: f32,
        y: f32,
    },
    Wheel {
        id: u64,
        dx: f32,
        dy: f32,
        x: f32,
        y: f32,
    },
    Key {
        id: u64,
        down: bool,
        codepoint: u32,
        special: i32,
        mods: u32,
    },
    Text {
        id: u64,
        text: String,
    },
}

// ---------------------------------------------------------------------------
// Worker thread (owns Servo and all views)
// ---------------------------------------------------------------------------

struct Delegate {
    frame_ready: Cell<bool>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.frame_ready.set(true);
    }
    fn notify_load_status_changed(&self, _wv: WebView, status: libservo::LoadStatus) {
        log::info!("load status: {status:?}");
    }
    fn notify_url_changed(&self, _wv: WebView, url: Url) {
        log::info!("url: {url}");
    }
    fn notify_page_title_changed(&self, _wv: WebView, title: Option<String>) {
        log::info!("title: {title:?}");
    }
}

/// Field order matters for drop order: the web view goes first, the context last.
struct WorkerView {
    webview: WebView,
    delegate: Rc<Delegate>,
    shared: Arc<Mutex<Shared>>,
    last_paint: Instant,
    context: Rc<SoftwareRenderingContext>,
}

struct Worker {
    servo: Option<&'static Servo>,
    views: HashMap<u64, WorkerView>,
}

impl Worker {
    fn new() -> Self {
        Self {
            servo: None,
            views: HashMap::new(),
        }
    }

    /// Servo can be created only once per process: build it lazily, keep it forever.
    fn servo(&mut self) -> &'static Servo {
        *self
            .servo
            .get_or_insert_with(|| Box::leak(Box::new(ServoBuilder::default().build())))
    }

    fn create(
        &mut self,
        id: u64,
        width: u32,
        height: u32,
        url: Url,
        shared: Arc<Mutex<Shared>>,
    ) -> Result<(), String> {
        let context = Rc::new(
            SoftwareRenderingContext::new(PhysicalSize::new(width, height))
                .map_err(|e| format!("Rendering context creation failed: {e:?}"))?,
        );
        context
            .make_current()
            .map_err(|e| format!("make_current failed: {e:?}"))?;

        let servo = self.servo();

        let delegate = Rc::new(Delegate {
            // Force the very first paint.
            frame_ready: Cell::new(true),
        });
        let rendering_context: Rc<dyn RenderingContext> = context.clone();
        let webview = WebViewBuilder::new(servo, rendering_context)
            .url(url)
            .delegate(delegate.clone())
            .build();
        webview.show();
        webview.set_focused(true);
        servo.spin_event_loop();

        self.views.insert(
            id,
            WorkerView {
                webview,
                delegate,
                shared,
                last_paint: Instant::now() - MIN_FRAME_INTERVAL,
                context,
            },
        );
        Ok(())
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Create {
                id,
                width,
                height,
                url,
                shared,
                reply,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    self.create(id, width, height, url, shared)
                }))
                .unwrap_or_else(|_| Err("Panic while creating view".to_owned()));
                let _ = reply.send(result);
            }
            Cmd::Destroy { id, done } => {
                self.views.remove(&id); // drops webview first, context last
                let _ = done.send(());
            }
            Cmd::Resize { id, width, height } => {
                if let Some(v) = self.views.get(&id) {
                    let size = PhysicalSize::new(width, height);
                    let _ = v.context.make_current();
                    v.context.resize(size);
                    v.webview.resize(size);
                    v.delegate.frame_ready.set(true);
                }
            }
            Cmd::Load { id, url } => {
                if let Some(v) = self.views.get(&id) {
                    v.webview.load(url);
                }
            }
            Cmd::MouseMove { id, x, y } => {
                self.input(id, InputEvent::MouseMove(MouseMoveEvent::new(point(x, y))));
            }
            Cmd::MouseButton {
                id,
                button,
                pressed,
                x,
                y,
            } => {
                let button = match button {
                    0 => MouseButton::Primary,
                    1 => MouseButton::Secondary,
                    2 => MouseButton::Auxiliary,
                    other => MouseButton::Other(other as u16),
                };
                let action = if pressed {
                    MouseButtonAction::Down
                } else {
                    MouseButtonAction::Up
                };
                self.input(
                    id,
                    InputEvent::MouseButton(MouseButtonEvent::new(action, button, point(x, y))),
                );
            }
            Cmd::Wheel { id, dx, dy, x, y } => {
                let delta = WheelDelta {
                    x: dx as f64,
                    y: dy as f64,
                    z: 0.0,
                    mode: WheelMode::DeltaLine,
                };
                self.input(
                    id,
                    InputEvent::Wheel(WheelEvent::new(delta, point(x, y), Modifiers::empty())),
                );
            }
            Cmd::Key {
                id,
                down,
                codepoint,
                special,
                mods,
            } => {
                let key = if codepoint != 0 {
                    match char::from_u32(codepoint) {
                        Some(c) => Key::Character(c.to_string()),
                        None => return,
                    }
                } else {
                    match special_key(special) {
                        Some(key) => key,
                        None => return,
                    }
                };
                let state = if down { KeyState::Down } else { KeyState::Up };
                self.input(id, key_event(state, key, modifiers_from_bits(mods)));
            }
            Cmd::Text { id, text } => {
                for c in text.chars() {
                    let key = Key::Character(c.to_string());
                    self.input(
                        id,
                        key_event(KeyState::Down, key.clone(), Modifiers::empty()),
                    );
                    self.input(id, key_event(KeyState::Up, key, Modifiers::empty()));
                }
            }
        }
    }

    fn input(&mut self, id: u64, event: InputEvent) {
        if let Some(v) = self.views.get(&id) {
            v.webview.notify_input_event(event);
        }
    }

    /// Pump Servo and publish new frames.
    fn tick(&mut self) {
        let Some(servo) = self.servo else { return };

        // With several views the "current" context is arbitrary; any of them will do for the spin.
        if let Some(v) = self.views.values().next() {
            let _ = v.context.make_current();
        }
        servo.spin_event_loop();

        let now = Instant::now();
        for v in self.views.values_mut() {
            if !v.delegate.frame_ready.get() {
                continue;
            }
            if now.duration_since(v.last_paint) < MIN_FRAME_INTERVAL {
                continue; // stay "ready", paint on a later tick
            }
            v.delegate.frame_ready.set(false);
            v.last_paint = now;
            paint_view(v);
        }
    }
}

fn paint_view(v: &mut WorkerView) {
    if v.context.make_current().is_err() {
        return;
    }
    v.webview.paint();

    let size = v.context.size();
    let rect = DeviceIntRect::new(
        DeviceIntPoint::new(0, 0),
        DeviceIntPoint::new(size.width as i32, size.height as i32),
    );
    let Some(image) = v.context.read_to_image(rect) else {
        return;
    };

    let raw = image.into_raw();
    let non_white = raw
        .chunks_exact(4)
        .filter(|p| p[0] != 255 || p[1] != 255 || p[2] != 255)
        .count();
        
    let mut shared = lock(&v.shared);
    shared.pixels = raw;
    shared.width = size.width;
    shared.height = size.height;
    shared.frame_id += 1;

    log::info!(
        "frame {}x{} non-white px: {}",
        size.width,
        size.height,
        non_white
    );
}

fn worker_main(rx: Receiver<Cmd>) {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("ServoHTML"),
    );
    std::panic::set_hook(Box::new(|info| {
        log::error!("PANIC: {info}");
    }));

    let mut worker = Worker::new();

    loop {
        let first = match rx.recv_timeout(POLL_INTERVAL) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let mut pending = first;
        while let Some(cmd) = pending.take().or_else(|| rx.try_recv().ok()) {
            if catch_unwind(AssertUnwindSafe(|| worker.handle(cmd))).is_err() {
                eprintln!("servo_html: panic while handling a command");
            }
        }

        if catch_unwind(AssertUnwindSafe(|| worker.tick())).is_err() {
            eprintln!("servo_html: panic while pumping Servo");
        }
    }

    // Servo must never be dropped (it cannot be created twice in one process).
    std::mem::forget(worker);
}

static WORKER: OnceLock<Option<Sender<Cmd>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn worker() -> Option<&'static Sender<Cmd>> {
    WORKER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            thread::Builder::new()
                .name("servo-html".to_owned())
                .stack_size(16 * 1024 * 1024)
                .spawn(move || worker_main(rx))
                .ok()
                .map(|_| tx)
        })
        .as_ref()
}

// ---------------------------------------------------------------------------
// Caller side (engine thread)
// ---------------------------------------------------------------------------

pub struct ServoView {
    id: u64,
    tx: Sender<Cmd>,
    shared: Arc<Mutex<Shared>>,
    /// Buffer handed out to the caller; valid until the next acquire.
    front: Vec<u8>,
    last_frame_id: u64,
    log: LogCallback,
}

fn log_message(callback: LogCallback, level: i32, message: &str) {
    if let (Some(callback), Ok(message)) = (callback, CString::new(message)) {
        unsafe { callback(level, c"%s".as_ptr(), message.as_ptr()) };
    }
}

fn read_c_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok().map(str::to_owned) }
}

fn send(view: *mut ServoView, make: impl FnOnce(u64) -> Cmd) {
    if view.is_null() {
        return;
    }
    let view = unsafe { &*view };
    let _ = view.tx.send(make(view.id));
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_abi_version() -> u32 {
    ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_create(cfg: *const ServoViewConfig) -> *mut ServoView {
    if cfg.is_null() {
        return std::ptr::null_mut();
    }
    let log = unsafe { (*cfg).log };

    let result = catch_unwind(AssertUnwindSafe(|| -> Result<Box<ServoView>, String> {
        let cfg = unsafe { &*cfg };
        if cfg.abi_version != ABI_VERSION {
            return Err(format!("Unsupported ABI version: {}", cfg.abi_version));
        }
        if cfg.width == 0 || cfg.height == 0 {
            return Err("Invalid viewport size".to_owned());
        }
        let url_string = read_c_string(cfg.url).unwrap_or_else(|| "about:blank".to_owned());
        let url = Url::parse(&url_string).map_err(|e| format!("Invalid URL: {e}"))?;

        let tx = worker()
            .ok_or_else(|| "Failed to start the Servo thread".to_owned())?
            .clone();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(Cmd::Create {
            id,
            width: cfg.width,
            height: cfg.height,
            url,
            shared: shared.clone(),
            reply: reply_tx,
        })
        .map_err(|_| "The Servo thread is not running".to_owned())?;

        // Blocks until Servo and the rendering context are up (first call is slow).
        reply_rx
            .recv()
            .map_err(|_| "The Servo thread died while creating the view".to_owned())??;

        Ok(Box::new(ServoView {
            id,
            tx,
            shared,
            front: Vec::new(),
            last_frame_id: 0,
            log,
        }))
    }));

    match result {
        Ok(Ok(view)) => {
            log_message(log, 1, "Servo ready");
            Box::into_raw(view)
        }
        Ok(Err(error)) => {
            log_message(log, 3, &error);
            std::ptr::null_mut()
        }
        Err(_) => {
            log_message(log, 3, "Panic while creating view");
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_destroy(view: *mut ServoView) {
    if view.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let view = unsafe { Box::from_raw(view) };
        let (done_tx, done_rx) = mpsc::channel();
        if view.tx.send(Cmd::Destroy { id: view.id, done: done_tx }).is_ok() {
            let _ = done_rx.recv_timeout(Duration::from_secs(5));
        }
        drop(view);
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_resize(view: *mut ServoView, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    send(view, |id| Cmd::Resize { id, width, height });
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_load_url(view: *mut ServoView, url: *const c_char) {
    if view.is_null() {
        return;
    }
    let log = unsafe { (*view).log };
    let Some(text) = read_c_string(url) else { return };
    match Url::parse(&text) {
        Ok(url) => send(view, |id| Cmd::Load { id, url }),
        Err(error) => log_message(log, 3, &format!("Invalid URL: {error}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_acquire(view: *mut ServoView, out: *mut ServoViewFrame) -> i32 {
    if view.is_null() || out.is_null() {
        return 0;
    }
    let result = catch_unwind(AssertUnwindSafe(|| -> i32 {
        let view = unsafe { &mut *view };

        let mut shared = lock(&view.shared);
        if shared.frame_id == view.last_frame_id || shared.pixels.is_empty() {
            return 0;
        }
        // Take the new frame without copying; the worker gets our old buffer back and overwrites it.
        std::mem::swap(&mut view.front, &mut shared.pixels);
        view.last_frame_id = shared.frame_id;
        let (width, height) = (shared.width, shared.height);
        drop(shared);

        unsafe {
            *out = ServoViewFrame {
                gl_texture: 0,
                width,
                height,
                frame_id: view.last_frame_id,
                flipped_y: 0,
                pixels: view.front.as_ptr(),
                stride: width * 4,
            };
        }
        1
    }));
    result.unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_mouse_move(view: *mut ServoView, x: f32, y: f32) {
    send(view, |id| Cmd::MouseMove { id, x, y });
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_mouse_button(
    view: *mut ServoView,
    button: i32,
    pressed: i32,
    x: f32,
    y: f32,
) {
    send(view, |id| Cmd::MouseButton {
        id,
        button,
        pressed: pressed != 0,
        x,
        y,
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_wheel(view: *mut ServoView, dx: f32, dy: f32, x: f32, y: f32) {
    send(view, |id| Cmd::Wheel { id, dx, dy, x, y });
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_key(
    view: *mut ServoView,
    down: i32,
    codepoint: u32,
    special: i32,
    mods: u32,
) {
    send(view, |id| Cmd::Key {
        id,
        down: down != 0,
        codepoint,
        special,
        mods,
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn cs_web_text(view: *mut ServoView, text: *const c_char) {
    let Some(text) = read_c_string(text) else { return };
    send(view, |id| Cmd::Text { id, text });
}

// ---------------------------------------------------------------------------
// Input helpers (used on the worker thread)
// ---------------------------------------------------------------------------

fn point(x: f32, y: f32) -> WebViewPoint {
    WebViewPoint::Device(DevicePoint::new(x, y))
}

fn special_key(special: i32) -> Option<Key> {
    Some(Key::Named(match special {
        1 => NamedKey::Enter,
        2 => NamedKey::Backspace,
        3 => NamedKey::Tab,
        4 => NamedKey::Escape,
        5 => NamedKey::ArrowUp,
        6 => NamedKey::ArrowDown,
        7 => NamedKey::ArrowLeft,
        8 => NamedKey::ArrowRight,
        9 => NamedKey::Home,
        10 => NamedKey::End,
        11 => NamedKey::PageUp,
        12 => NamedKey::PageDown,
        13 => NamedKey::Insert,
        14 => NamedKey::Delete,
        _ => return None,
    }))
}

fn modifiers_from_bits(bits: u32) -> Modifiers {
    let mut modifiers = Modifiers::empty();
    if bits & 1 != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if bits & 2 != 0 {
        modifiers |= Modifiers::CONTROL;
    }
    if bits & 4 != 0 {
        modifiers |= Modifiers::ALT;
    }
    if bits & 8 != 0 {
        modifiers |= Modifiers::META;
    }
    modifiers
}

fn key_event(state: KeyState, key: Key, modifiers: Modifiers) -> InputEvent {
    // The physical `Code` is not known from the engine's key numbers, so it is left unidentified.
    InputEvent::Keyboard(KeyboardEvent::new_without_event(
        state,
        key,
        Code::Unidentified,
        Location::Standard,
        modifiers,
        false,
        false,
    ))
}