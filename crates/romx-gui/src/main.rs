#![cfg_attr(windows, windows_subsystem = "windows")]

slint::include_modules!();

use image::ImageReader;
use rfd::FileDialog;
use romx_core::{
    application_version, classify_gb_payload, export_lpl_with_output_handling, format_extension,
    import_lpl_with_error_handling, plan_lpl_import, platform_name_from_id,
    read_metadata_cover_path, read_path, ExportLplOptions, ImportLplPlan, LPLX_METADATA_KEY,
    ROMX_LPLX_METADATA_FIELDS, SPEC_VERSION,
};
use serde_json::{Map, Value};
use slint::{Image, ModelRc, SharedPixelBuffer, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc, Mutex, OnceLock,
};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSColor, NSView};
#[cfg(target_os = "macos")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

const ROM_EXTENSIONS: &[&str] = &[
    "gb", "gbc", "gba", "nes", "fds", "sfc", "smc", "nds", "3ds", "cci", "cia", "md", "gen", "smd",
    "bin",
];
const ROMX_EXTENSIONS: &[&str] = &["romx"];
struct LocaleCatalog {
    languages: Vec<HashMap<String, String>>,
}

impl LocaleCatalog {
    fn load() -> Self {
        let mut languages = Vec::with_capacity(2);
        for (name, embedded) in [
            ("en.json", include_str!("../locales/en.json")),
            ("zh-CN.json", include_str!("../locales/zh-CN.json")),
        ] {
            let contents = read_locale_file(name).unwrap_or_else(|| embedded.to_owned());
            let entries = serde_json::from_str::<Value>(&contents)
                .ok()
                .and_then(|value| value.as_object().cloned())
                .map(|object| {
                    object
                        .into_iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|text| (key, text.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            languages.push(entries);
        }
        Self { languages }
    }

    fn text(&self, key: &str, language_index: i32) -> String {
        let index = language_index.clamp(0, self.languages.len().saturating_sub(1) as i32) as usize;
        self.languages
            .get(index)
            .and_then(|language| language.get(key))
            .cloned()
            .or_else(|| {
                self.languages
                    .first()
                    .and_then(|language| language.get(key))
                    .cloned()
            })
            .unwrap_or_else(|| key.to_owned())
    }

    fn text_or(&self, key: &str, language_index: i32, fallback: &str) -> String {
        let text = self.text(key, language_index);
        if text == key {
            fallback.to_owned()
        } else {
            text
        }
    }
}

fn read_locale_file(name: &str) -> Option<String> {
    let mut candidates = Vec::with_capacity(3);
    if let Some(directory) = env::var_os("ROMX_LOCALE_DIR") {
        candidates.push(PathBuf::from(directory).join(name));
    }
    if let Ok(executable) = env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.join("locales").join(name));
            // A macOS application stores bundled resources under
            // Contents/Resources while the executable lives in
            // Contents/MacOS.
            if cfg!(target_os = "macos") {
                if let Some(contents) = directory.parent() {
                    candidates.push(contents.join("Resources").join("locales").join(name));
                }
            }
        }
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("locales")
            .join(name),
    );
    candidates
        .into_iter()
        .find_map(|path| fs::read_to_string(path).ok())
}

static LOCALES: OnceLock<LocaleCatalog> = OnceLock::new();

fn locale_catalog() -> &'static LocaleCatalog {
    LOCALES.get_or_init(LocaleCatalog::load)
}

struct LplWorkspace {
    source_path: Option<PathBuf>,
    work_path: Option<PathBuf>,
    edit_path: Option<PathBuf>,
    rom_dir: String,
    image_dir: String,
    current_index: Option<usize>,
    temp_dir: PathBuf,
    next_single_index: u32,
    next_list_index: u32,
    list_index: Option<u32>,
    cancel_flag: Option<Arc<AtomicBool>>,
    pending: Option<PendingConversion>,
    single_conflict: Option<PendingSingleConflict>,
    romx_cover_path: Option<PathBuf>,
    romx_metadata: Option<Value>,
    plan_cache: Option<ImportLplPlan>,
    plan_path_index: Option<HashMap<String, usize>>,
    rom_entries: Vec<PathBuf>,
    preview_cache: Arc<Mutex<PreviewCache>>,
    preview_generation: Arc<AtomicUsize>,
    preview_sender: Sender<PreviewTask>,
    prompt_sender: Arc<Mutex<Option<Sender<PromptResponse>>>>,
}

#[derive(Clone)]
struct DecodedPreview {
    width: u32,
    height: u32,
    pixels: Arc<Vec<u8>>,
}

struct PreviewCache {
    entries: VecDeque<(String, DecodedPreview)>,
    capacity: usize,
}

impl PreviewCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
        }
    }

    fn get(&mut self, key: &str) -> Option<DecodedPreview> {
        let position = self
            .entries
            .iter()
            .position(|(entry_key, _)| entry_key == key)?;
        let entry = self.entries.remove(position)?;
        let preview = entry.1.clone();
        self.entries.push_back(entry);
        Some(preview)
    }

    fn insert(&mut self, key: String, preview: DecodedPreview) {
        self.entries.retain(|(entry_key, _)| entry_key != &key);
        self.entries.push_back((key, preview));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

enum PreviewSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

struct PreviewTask {
    request_id: usize,
    key: String,
    source: PreviewSource,
    cache: Arc<Mutex<PreviewCache>>,
    generation: Arc<AtomicUsize>,
    window: slint::Weak<MainWindow>,
}

fn start_preview_worker() -> Sender<PreviewTask> {
    let (sender, receiver) = mpsc::channel::<PreviewTask>();
    thread::spawn(move || {
        while let Ok(task) = receiver.recv() {
            let PreviewTask {
                request_id,
                key,
                source,
                cache,
                generation,
                window,
            } = task;
            if generation.load(Ordering::Relaxed) != request_id {
                continue;
            }
            let result = match source {
                PreviewSource::File(path) => decode_preview_path(&path),
                PreviewSource::Bytes(bytes) => decode_preview_bytes(&bytes),
            };
            if generation.load(Ordering::Relaxed) == request_id {
                if let Ok(preview) = &result {
                    if let Ok(mut cache) = cache.lock() {
                        cache.insert(key, preview.clone());
                    }
                }
            }
            if generation.load(Ordering::Relaxed) != request_id {
                continue;
            }
            let generation_for_ui = generation.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if generation_for_ui.load(Ordering::Relaxed) != request_id {
                    return;
                }
                if let Some(window) = window.upgrade() {
                    match result {
                        Ok(preview) => window.set_cover_preview(decoded_preview_to_slint(&preview)),
                        Err(_) => window.set_cover_preview(Image::default()),
                    }
                }
            });
        }
    });
    sender
}

#[derive(Clone, Copy)]
enum PromptResponse {
    Skip,
    SkipAll,
    Rename,
    RenameAll,
    Replace,
    ReplaceAll,
    Continue,
    Stop,
}

#[derive(Clone)]
struct PendingConversion {
    lpl_path: PathBuf,
    save_path: PathBuf,
    cover_target: Option<(u32, u32)>,
}

struct SingleConversion {
    lpl_path: PathBuf,
    payload_path: Option<PathBuf>,
    cover_target: Option<(u32, u32)>,
    save_path: PathBuf,
    output_path: PathBuf,
    replace: bool,
}

type PendingSingleConflict = SingleConversion;

impl LplWorkspace {
    fn new() -> Self {
        let temp_dir = std::env::temp_dir()
            .join("romx-gui-lpl")
            .join(std::process::id().to_string());
        // A previous process may have exited before its `Drop` handler ran.
        // The directory is process-scoped, so removing this exact path is
        // safe and prevents stale workspaces from accumulating.
        let _ = fs::remove_dir_all(&temp_dir);
        let preview_cache = Arc::new(Mutex::new(PreviewCache::new(6)));
        let preview_generation = Arc::new(AtomicUsize::new(0));
        let preview_sender = start_preview_worker();
        Self {
            source_path: None,
            work_path: None,
            edit_path: None,
            rom_dir: String::new(),
            image_dir: String::new(),
            current_index: None,
            temp_dir,
            next_single_index: 1,
            next_list_index: 1,
            list_index: None,
            cancel_flag: None,
            pending: None,
            single_conflict: None,
            romx_cover_path: None,
            romx_metadata: None,
            plan_cache: None,
            plan_path_index: None,
            rom_entries: Vec::new(),
            preview_cache,
            preview_generation,
            preview_sender,
            prompt_sender: Arc::new(Mutex::new(None)),
        }
    }

    fn reset(&mut self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
        self.source_path = None;
        self.work_path = None;
        self.edit_path = None;
        self.rom_dir.clear();
        self.image_dir.clear();
        self.current_index = None;
        self.list_index = None;
        self.cancel_flag = None;
        self.pending = None;
        self.single_conflict = None;
        self.romx_cover_path = None;
        self.romx_metadata = None;
        self.plan_cache = None;
        self.plan_path_index = None;
        self.rom_entries.clear();
        self.preview_generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut cache) = self.preview_cache.lock() {
            cache.clear();
        }
        if let Ok(mut sender) = self.prompt_sender.lock() {
            *sender = None;
        }
    }

    fn clear_romx_cover(&mut self) {
        if let Some(path) = self.romx_cover_path.take() {
            let _ = fs::remove_file(path);
        }
        self.romx_metadata = None;
    }

    fn next_single_path(&mut self) -> PathBuf {
        let index = self.next_single_index;
        self.next_single_index = self.next_single_index.saturating_add(1);
        self.temp_dir.join(format!("temp-single-{index:02}.lplx"))
    }

    fn next_list_path(&mut self) -> (u32, PathBuf) {
        let index = self.next_list_index;
        self.next_list_index = self.next_list_index.saturating_add(1);
        (
            index,
            self.temp_dir.join(format!("temp-list-{index:02}.lplx")),
        )
    }

    fn new_edit_path(&self) -> Option<PathBuf> {
        self.list_index
            .map(|index| self.temp_dir.join(format!("temp-list-{index:02}-2.lplx")))
    }
}

#[derive(Clone, Copy)]
enum OutputConflictMode {
    Ask,
    SkipAlways,
    RenameAlways,
    ReplaceAlways,
}

fn renamed_target(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let suffix = |number: usize| {
        let name = if number == 1 {
            format!("{stem}_new")
        } else {
            format!("{stem}_new_{number:02}")
        };
        if extension.is_empty() {
            parent.join(name)
        } else {
            parent.join(format!("{name}.{extension}"))
        }
    };
    (1..)
        .map(suffix)
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| parent.join(format!("{stem}_new.{extension}")))
}

fn final_output_name(staged: &Path) -> Option<String> {
    staged
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| value.strip_suffix(".tmp").unwrap_or(value).to_owned())
}

fn commit_staged(staged: &Path, target: &Path, replace: bool) -> Result<PathBuf, String> {
    if !replace && target.exists() {
        return Err(format!("Output file already exists: {}", target.display()));
    }
    if replace && target.exists() {
        fs::remove_file(target)
            .map_err(|error| format!("Failed to remove the old output: {error}"))?;
    }
    if let Err(rename_error) = fs::rename(staged, target) {
        fs::copy(staged, target).map_err(|copy_error| {
            format!("Failed to commit output: {rename_error}; copy also failed: {copy_error}")
        })?;
        fs::remove_file(staged)
            .map_err(|error| format!("Failed to clean up the staged output: {error}"))?;
    }
    Ok(target.to_owned())
}

impl Drop for LplWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

fn path_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("untitled")
        .to_owned()
}

fn payload_format(path: &str) -> Result<String, String> {
    let format = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if ROM_EXTENSIONS.contains(&format.as_str()) {
        Ok(format)
    } else {
        Err(format!(
            "Unsupported ROM format: {}",
            if format.is_empty() {
                "no extension"
            } else {
                &format
            }
        ))
    }
}

fn set_status(window: &MainWindow, key: impl AsRef<str>, english: impl AsRef<str>) {
    let key = key.as_ref();
    let english = english.as_ref();
    let message = locale_catalog().text_or(key, window.get_language_index(), english);
    window.set_status_text(message.into());
}

fn localized_text(window: &MainWindow, key: &str, fallback: &str) -> String {
    locale_catalog().text_or(key, window.get_language_index(), fallback)
}

/// Make the native macOS title bar opaque while keeping the system traffic
/// light and resize controls. The other platforms do not need any native
/// window calls, so this function is compiled as a no-op there.
#[cfg(target_os = "macos")]
fn configure_native_titlebar(window: &MainWindow) -> bool {
    let window_handle = window.window().window_handle();
    let Ok(handle) = window_handle.window_handle() else {
        return false;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return false;
    };
    let Some(view) = (unsafe { Retained::<NSView>::retain(handle.ns_view.as_ptr().cast()) }) else {
        return false;
    };
    let Some(native_window) = view.window() else {
        return false;
    };
    native_window.setTitlebarAppearsTransparent(false);
    native_window.setOpaque(true);
    // Winit can leave the window alpha below 1 when a transparent titlebar
    // was requested during creation. Reset it as well as the opaque flag so
    // the system titlebar cannot show the desktop behind the window.
    unsafe { native_window.setAlphaValue(1.0) };
    let white = unsafe { NSColor::whiteColor() };
    native_window.setBackgroundColor(Some(&white));
    true
}

#[cfg(target_os = "macos")]
fn retry_native_titlebar(weak: slint::Weak<MainWindow>, attempts_left: u8) {
    slint::Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(window) = weak.upgrade() {
            let configured = configure_native_titlebar(&window);
            if !configured || attempts_left > 0 {
                retry_native_titlebar(window.as_weak(), attempts_left.saturating_sub(1));
            }
        }
    });
}

fn decode_preview_path(path: &Path) -> Result<DecodedPreview, String> {
    let image = ImageReader::open(path)
        .map_err(|error| error.to_string())?
        .with_guessed_format()
        .map_err(|error| error.to_string())?
        .decode()
        .map_err(|error| error.to_string())?;
    Ok(decoded_preview(image))
}

fn decode_preview_bytes(bytes: &[u8]) -> Result<DecodedPreview, String> {
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?
        .decode()
        .map_err(|error| error.to_string())?;
    Ok(decoded_preview(image))
}

fn decoded_preview(image: image::DynamicImage) -> DecodedPreview {
    // Keep the original cover dimensions for preview. Slint scales the image
    // to the fixed UI box, and avoiding an extra thumbnail/RGBA allocation
    // keeps directory browsing responsive and allows every selected cover to
    // be replaced reliably.
    let image = image.to_rgb8();
    let width = image.width();
    let height = image.height();
    DecodedPreview {
        width,
        height,
        pixels: Arc::new(image.into_raw()),
    }
}

fn decoded_preview_to_slint(preview: &DecodedPreview) -> Image {
    let buffer = SharedPixelBuffer::clone_from_slice(
        preview.pixels.as_slice(),
        preview.width,
        preview.height,
    );
    Image::from_rgb8(buffer)
}

fn normalized_path_string(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        let mut value = value;
        if let Some(rest) = value.strip_prefix("//?/UNC/") {
            value = format!("//{rest}");
        } else if let Some(rest) = value.strip_prefix("//?/") {
            value = rest.to_owned();
        }
        return value.to_lowercase();
    }
    #[cfg(not(windows))]
    {
        value
    }
}

fn preview_path_key(path: &Path) -> String {
    let path = absolute_path(path);
    let value = normalized_path_string(&path);
    let signature = fs::metadata(path)
        .ok()
        .map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_nanos())
                .unwrap_or_default();
            format!(":{}:{modified}", metadata.len())
        })
        .unwrap_or_default();
    format!("{value}{signature}")
}

fn request_preview_source(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    key: String,
    source: PreviewSource,
) {
    let (cache, generation, sender) = {
        let state = workspace.borrow();
        (
            state.preview_cache.clone(),
            state.preview_generation.clone(),
            state.preview_sender.clone(),
        )
    };
    let request_id = generation.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    if let Ok(mut cache) = cache.lock() {
        if let Some(preview) = cache.get(&key) {
            window.set_cover_preview(decoded_preview_to_slint(&preview));
            return;
        }
    }
    let _ = sender.send(PreviewTask {
        request_id,
        key,
        source,
        cache,
        generation,
        window: window.as_weak(),
    });
}

fn request_preview_path(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    path: Option<&Path>,
) {
    let Some(path) = path else {
        let generation = workspace.borrow().preview_generation.clone();
        generation.fetch_add(1, Ordering::Relaxed);
        window.set_cover_preview(Image::default());
        return;
    };
    request_preview_source(
        window,
        workspace,
        preview_path_key(path),
        PreviewSource::File(path.to_owned()),
    );
}

fn request_preview_bytes(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    key: String,
    bytes: &[u8],
) {
    request_preview_source(
        window,
        workspace,
        format!("bytes:{key}"),
        PreviewSource::Bytes(bytes.to_owned()),
    );
}

fn resolution(window: &MainWindow) -> Result<Option<(u32, u32)>, String> {
    match window.get_resolution_index() {
        0 => Ok(None),
        1 => Ok(Some((256, 224))),
        2 => Ok(Some((320, 240))),
        3 => Ok(Some((512, 512))),
        4 => Ok(Some((640, 480))),
        5 => {
            let value = window.get_custom_resolution();
            let value = value.trim();
            let (width, height) = value
                .split_once('×')
                .or_else(|| value.split_once('x'))
                .or_else(|| value.split_once('X'))
                .ok_or_else(|| "Custom resolution must use WIDTH × HEIGHT".to_owned())?;
            let width = width
                .trim()
                .parse::<u32>()
                .map_err(|_| "Custom width must be a positive integer".to_owned())?;
            let height = height
                .trim()
                .parse::<u32>()
                .map_err(|_| "Custom height must be a positive integer".to_owned())?;
            if width == 0 || height == 0 || width > 8192 || height > 8192 {
                return Err("Custom resolution must be between 1 and 8192".into());
            }
            Ok(Some((width, height)))
        }
        _ => Err("Unknown cover resolution option".into()),
    }
}

fn load_metadata(path: &str) -> Result<Value, Box<dyn std::error::Error>> {
    if path.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    if !value.is_object() {
        return Err("Metadata root must be a JSON object".into());
    }
    Ok(value)
}

fn text_or_empty(value: &Value, key: &str) -> String {
    let result = value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if result.is_empty() && key == "label" {
        value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        result
    }
}

fn metadata_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .get(LPLX_METADATA_KEY)
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get(key))
        .or_else(|| value.get(key))
}

fn metadata_text(value: &Value, key: &str) -> String {
    metadata_value(value, key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn metadata_genre_text(value: &Value) -> String {
    match metadata_value(value, "genre") {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Some(Value::String(value)) => value.clone(),
        _ => String::new(),
    }
}

fn parse_genre(value: &str) -> Value {
    let mut genres = Vec::new();
    for genre in value.split([',', '，', ';', '；']) {
        let genre = genre.trim();
        if !genre.is_empty() && !genres.iter().any(|item: &String| item == genre) {
            genres.push(genre.to_owned());
        }
    }
    Value::Array(genres.into_iter().map(Value::String).collect())
}

fn platform_for_path(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "gb" => "gb",
        "gbc" => "gbc",
        "gba" => "gba",
        "nes" | "fds" => "nes",
        "sfc" | "smc" => "snes",
        "nds" => "nds",
        "3ds" | "cci" | "cia" => "3ds",
        "md" | "gen" | "smd" | "bin" => "genesis",
        _ => "gba",
    }
}

fn supported_platform(value: &str) -> bool {
    matches!(
        value,
        "gb" | "gbc" | "gba" | "nes" | "snes" | "nds" | "3ds" | "genesis"
    )
}

fn set_metadata_form(window: &MainWindow, value: &Value) {
    let name = text_or_empty(value, "name");
    let name = if name.is_empty() {
        text_or_empty(value, "label")
    } else {
        name
    };
    let platform = metadata_text(value, "platform");
    let origin = {
        let origin = metadata_text(value, "origin");
        if origin.is_empty() {
            metadata_text(value, "country")
        } else {
            origin
        }
    };
    window.set_display_title(name.into());
    window.set_genre(metadata_genre_text(value).into());
    window.set_platform(
        if supported_platform(&platform) {
            platform
        } else {
            "gba".to_owned()
        }
        .into(),
    );
    window.set_developer(metadata_text(value, "developer").into());
    window.set_release_date(metadata_text(value, "release_date").into());
    window.set_origin(origin.into());
}

fn build_metadata_with_base(
    window: &MainWindow,
    base: Option<&Value>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let source = if window.get_metadata_path().trim().is_empty() {
        base.cloned().unwrap_or_else(|| Value::Object(Map::new()))
    } else {
        load_metadata(&window.get_metadata_path())?
    };
    let source = source
        .as_object()
        .ok_or("Metadata root must be a JSON object")?;
    let mut object = Map::new();
    object.insert("schema_version".into(), Value::String(SPEC_VERSION.into()));
    for key in ["developer", "origin"] {
        if let Some(value) = source
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            object.insert(key.into(), Value::String(value.to_owned()));
        }
    }
    let fallback = path_stem(&window.get_rom_path());
    let label = if window.get_display_title().trim().is_empty() {
        fallback
    } else {
        window.get_display_title().trim().to_owned()
    };
    object.insert("name".into(), Value::String(label));

    object.insert("genre".into(), parse_genre(&window.get_genre()));
    let developer_value = window.get_developer();
    let developer = developer_value.trim();
    if developer.is_empty() {
        object.remove("developer");
    } else {
        object.insert("developer".into(), Value::String(developer.to_owned()));
    }
    let origin_value = window.get_origin();
    let origin = origin_value.trim();
    if origin.is_empty() {
        object.remove("origin");
    } else {
        object.insert("origin".into(), Value::String(origin.to_owned()));
    }
    let release_date_value = window.get_release_date();
    let release_date = release_date_value.trim();
    if release_date.is_empty() {
        object.remove("release_date");
    } else {
        object.insert(
            "release_date".into(),
            Value::String(release_date.to_owned()),
        );
    }
    Ok(Value::Object(object))
}

fn lplx_metadata_object_mut(item: &mut Map<String, Value>) -> &mut Map<String, Value> {
    item.entry(LPLX_METADATA_KEY)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("LPLX metadata must be an object")
}

fn set_lplx_metadata_value(item: &mut Map<String, Value>, key: &str, value: Value) {
    lplx_metadata_object_mut(item).insert(key.into(), value);
}

fn copy_metadata_to_lpl_item(item: &mut Map<String, Value>, metadata: &Value) {
    let Some(metadata) = metadata.as_object() else {
        return;
    };
    let mut display_name = None;
    for (key, value) in metadata {
        match key.as_str() {
            "name" => {
                display_name = Some(value.clone());
                set_lplx_metadata_value(item, key, value.clone());
            }
            key if key != "crc32" && ROMX_LPLX_METADATA_FIELDS.contains(&key) => {
                set_lplx_metadata_value(item, key, value.clone());
            }
            "x-retroarch" => {
                let Some(retroarch) = value.as_object() else {
                    continue;
                };
                for (retroarch_key, retroarch_value) in retroarch {
                    let lpl_key = if retroarch_key == "source_crc32" {
                        "crc32"
                    } else {
                        retroarch_key
                    };
                    if [
                        "path",
                        "label",
                        "core_path",
                        "core_name",
                        "crc32",
                        "db_name",
                        "entry_slot",
                        "subsystem_ident",
                        "subsystem_name",
                        "subsystem_roms",
                        "runtime_hours",
                        "runtime_minutes",
                        "runtime_seconds",
                        "last_played_year",
                        "last_played_month",
                        "last_played_day",
                        "last_played_hour",
                        "last_played_minute",
                        "last_played_second",
                    ]
                    .contains(&lpl_key)
                    {
                        item.insert(lpl_key.into(), retroarch_value.clone());
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(display_name) = display_name {
        item.insert("label".into(), display_name);
    }
    let metadata_object = lplx_metadata_object_mut(item);
    metadata_object
        .entry("schema_version")
        .or_insert_with(|| Value::String(SPEC_VERSION.into()));
}

fn ensure_lplx_metadata(item: &mut Map<String, Value>, label: &str) {
    let legacy = ROMX_LPLX_METADATA_FIELDS
        .iter()
        .filter_map(|key| {
            if *key == "crc32" {
                return None;
            }
            let value = item.get(*key)?.clone();
            // A string in the legacy `cover` key is a cover path; an object
            // is the ROMX cover descriptor and belongs in metadata.
            let should_move = *key != "cover" || value.is_object();
            should_move.then(|| ((*key).to_owned(), value))
        })
        .collect::<Vec<_>>();
    let metadata = lplx_metadata_object_mut(item);
    for (key, value) in legacy {
        metadata.entry(key).or_insert(value);
    }
    metadata
        .entry("schema_version")
        .or_insert_with(|| Value::String(SPEC_VERSION.into()));
    metadata
        .entry("name")
        .or_insert_with(|| Value::String(label.to_owned()));
    for key in ROMX_LPLX_METADATA_FIELDS {
        match *key {
            "crc32" => {}
            "cover" if item.get(*key).is_some_and(Value::is_string) => {}
            _ => {
                item.remove(*key);
            }
        }
    }
}

fn create_single_lpl(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    rom_path: &Path,
) -> Result<PathBuf, String> {
    let rom_path = absolute_path(rom_path);
    let cover_reference = if !window.get_cover_path().trim().is_empty() {
        Some(absolute_path(Path::new(window.get_cover_path().trim())))
    } else {
        workspace.borrow().romx_cover_path.clone()
    };
    let base_metadata = workspace.borrow().romx_metadata.clone();
    let metadata = match build_metadata_with_base(window, base_metadata.as_ref()) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(format!("Metadata generation failed: {error}"));
        }
    };
    let mut item = Map::new();
    item.insert(
        "path".into(),
        Value::String(rom_path.to_string_lossy().into_owned()),
    );
    copy_metadata_to_lpl_item(&mut item, &metadata);
    let platform = window.get_platform().trim().to_owned();
    item.insert(
        "platform".into(),
        Value::String(if supported_platform(&platform) {
            platform
        } else {
            platform_for_path(&rom_path.to_string_lossy()).to_owned()
        }),
    );
    if let Some(cover_path) = cover_reference.as_deref().filter(|path| path.exists()) {
        item.insert(
            "cover_path".into(),
            Value::String(absolute_path(cover_path).to_string_lossy().into_owned()),
        );
    }
    let document = serde_json::json!({
        "version": "1.5",
        "default_core_path": "DETECT",
        "default_core_name": "DETECT",
        "items": [Value::Object(item)],
    });
    let lpl_path = {
        let mut state = workspace.borrow_mut();
        fs::create_dir_all(&state.temp_dir)
            .map_err(|error| format!("Failed to create the temporary directory: {error}"))?;
        state.next_single_path()
    };
    if let Err(error) = write_json_file(&lpl_path, &document) {
        return Err(format!(
            "Failed to write the temporary single-file LPL: {error}"
        ));
    }
    Ok(lpl_path)
}

fn choose_rom(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let dialog = FileDialog::new()
        .set_title("Choose game ROM")
        .add_filter("Supported ROM", ROM_EXTENSIONS)
        .add_filter("ROMX", ROMX_EXTENSIONS);
    let Some(path) = dialog.pick_file() else {
        return;
    };
    let path_string = path.to_string_lossy().into_owned();
    let is_romx = is_romx_file(&path);
    let had_romx_state = {
        let state = workspace.borrow();
        state.romx_cover_path.is_some() || state.romx_metadata.is_some()
    };
    workspace.borrow_mut().clear_romx_cover();
    if had_romx_state {
        window.set_cover_path("".into());
        window.set_cover_preview(Image::default());
        window.set_metadata_path("".into());
    }
    window.set_rom_path(path_string.clone().into());
    if is_romx {
        match read_metadata_cover_path(&path) {
            Ok(document) => {
                let metadata = document
                    .metadata
                    .clone()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                workspace.borrow_mut().romx_metadata = Some(metadata.clone());
                set_metadata_form(window, &metadata);
                window.set_platform(
                    platform_name_from_id(document.footer.platform_id)
                        .unwrap_or_else(|| platform_for_path(&path_string))
                        .into(),
                );
                if window.get_display_title().trim().is_empty() {
                    window.set_display_title(path_stem(&path_string).into());
                }
                window.set_metadata_path("".into());
                window.set_cover_path("".into());
                if let Some(cover) = document.cover.as_deref() {
                    let cover_path = {
                        let mut state = workspace.borrow_mut();
                        if let Err(error) = fs::create_dir_all(&state.temp_dir).and_then(|_| {
                            fs::write(state.temp_dir.join("loaded-romx-cover.png"), cover)
                        }) {
                            set_status(
                                window,
                                format!("Failed to save the ROMX cover: {error}"),
                                format!("ROMX cover save failed: {error}"),
                            );
                            return;
                        }
                        let path = state.temp_dir.join("loaded-romx-cover.png");
                        state.romx_cover_path = Some(path.clone());
                        path
                    };
                    request_preview_path(window, workspace, Some(&cover_path));
                } else {
                    window.set_cover_preview(Image::default());
                }
                set_status(
                    window,
                    format!("Loaded ROMX: {path_string}. Edit metadata and convert when ready"),
                    format!("ROMX loaded: {path_string}; edit metadata and convert to update"),
                );
            }
            Err(error) => {
                window.set_cover_preview(Image::default());
                set_status(
                    window,
                    format!("Failed to read ROMX: {error}"),
                    format!("ROMX read failed: {error}"),
                );
            }
        }
    } else {
        if window.get_display_title().trim().is_empty() {
            window.set_display_title(path_stem(&path_string).into());
        }
        window.set_platform(platform_for_path(&path_string).into());
    }
    if !is_romx {
        set_status(
            window,
            format!("Selected ROM: {path_string}"),
            format!("ROM selected: {path_string}"),
        );
    }
}

fn choose_cover(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let Some(path) = FileDialog::new()
        .set_title("Choose cover image")
        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
        .pick_file()
    else {
        return;
    };
    let path_string = path.to_string_lossy().into_owned();
    window.set_cover_path(path_string.clone().into());
    request_preview_path(window, workspace, Some(&path));
    set_status(
        window,
        format!("Selected image: {path_string}"),
        format!("Cover selected: {path_string}"),
    );
}

fn choose_metadata(window: &MainWindow) {
    let Some(path) = FileDialog::new()
        .set_title("Choose metadata JSON")
        .add_filter("JSON", &["json"])
        .pick_file()
    else {
        return;
    };
    let path_string = path.to_string_lossy().into_owned();
    match load_metadata(&path_string) {
        Ok(metadata) => {
            window.set_metadata_path(path_string.clone().into());
            set_metadata_form(window, &metadata);
            set_status(
                window,
                format!("Loaded metadata: {path_string}"),
                format!("Metadata loaded: {path_string}"),
            );
        }
        Err(error) => set_status(
            window,
            format!("Failed to read metadata: {error}"),
            format!("Metadata read failed: {error}"),
        ),
    }
}

fn choose_directory(window: &MainWindow) {
    let Some(path) = FileDialog::new()
        .set_title("Choose directory")
        .pick_folder()
    else {
        return;
    };
    let path_string = path.to_string_lossy().into_owned();
    window.set_save_path(path_string.clone().into());
    set_status(
        window,
        format!("Selected directory: {path_string}"),
        format!("Folder selected: {path_string}"),
    );
}

fn read_json_file(path: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn write_json_file(path: &Path, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("lplx.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(path);
        fs::rename(&temporary, path).map_err(|_| error)?;
    }
    Ok(())
}

fn absolute_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

fn path_identity(path: &Path) -> String {
    normalized_path_string(&absolute_path(path))
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    path_identity(left) == path_identity(right)
}

fn resolve_workspace_rom(lpl_path: &Path, value: &str, rom_dir: &str) -> PathBuf {
    let original = PathBuf::from(value);
    if !rom_dir.trim().is_empty() {
        if let Some(name) = original.file_name() {
            let candidate = Path::new(rom_dir.trim()).join(name);
            if candidate.is_file() {
                return absolute_path(&candidate);
            }
            if let Some(found) = fs::read_dir(rom_dir.trim()).ok().and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.is_file()
                            && path
                                .file_name()
                                .is_some_and(|value| value.eq_ignore_ascii_case(name))
                    })
            }) {
                return absolute_path(&found);
            }
        }
    }
    let candidate = if original.is_absolute() {
        original
    } else {
        lpl_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(original)
    };
    absolute_path(&candidate)
}

struct WorkspaceCoverIndex {
    by_stem: HashMap<String, PathBuf>,
}

fn build_workspace_cover_index(image_dir: &str) -> WorkspaceCoverIndex {
    let mut index = WorkspaceCoverIndex {
        by_stem: HashMap::new(),
    };
    if image_dir.trim().is_empty() {
        return index;
    }
    let directory = Path::new(image_dir.trim());
    let supported = ["png", "jpg", "jpeg", "webp", "gif", "bmp"];
    let mut candidates = fs::read_dir(directory)
        .ok()
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .filter(|path| {
                    path.extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|extension| {
                            supported.contains(&extension.to_ascii_lowercase().as_str())
                        })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    candidates.sort_unstable_by_key(|path| {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        supported
            .iter()
            .position(|value| value.eq_ignore_ascii_case(extension))
            .unwrap_or(99)
    });
    for path in candidates {
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            index.by_stem.entry(stem.to_lowercase()).or_insert(path);
        }
    }
    index
}

fn find_workspace_cover(
    index: &WorkspaceCoverIndex,
    rom_path: &Path,
    label: &str,
) -> Option<PathBuf> {
    let names = [
        rom_path.file_stem()?.to_string_lossy().to_lowercase(),
        label.to_lowercase(),
    ];
    names
        .into_iter()
        .find_map(|name| index.by_stem.get(&name).map(|path| absolute_path(path)))
}

fn resolve_original_cover(lpl_path: &Path, item: &Map<String, Value>) -> Option<PathBuf> {
    ["cover_path", "thumbnail_path", "cover", "thumbnail"]
        .into_iter()
        .filter_map(|key| item.get(key).and_then(Value::as_str))
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                lpl_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(path)
            }
        })
        .find(|path| path.exists())
        .map(|path| absolute_path(&path))
}

fn set_lpl_cover_path(item: &mut Map<String, Value>, cover: Option<&Path>) {
    let existing_key = ["cover_path", "thumbnail_path", "cover", "thumbnail"]
        .into_iter()
        .find(|key| item.contains_key(*key))
        .unwrap_or("cover_path");
    for key in ["cover_path", "thumbnail_path", "cover", "thumbnail"] {
        item.remove(key);
    }
    if let Some(path) = cover {
        item.insert(
            existing_key.into(),
            Value::String(absolute_path(path).to_string_lossy().into_owned()),
        );
    }
}

fn prepare_lpl_workspace(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    force: bool,
) -> Result<(), String> {
    let source = window.get_lpl_path();
    if source.trim().is_empty() {
        return Err("Choose an LPL file first".into());
    }
    let rom_dir = window.get_lpl_rom_dir();
    let image_dir = window.get_lpl_image_dir();
    let source_path = absolute_path(Path::new(source.trim()));
    let base_path = {
        let state = workspace.borrow();
        if state.current_index.is_some() && !force {
            return Ok(());
        }
        if !force
            && state.source_path.as_deref() == Some(source_path.as_path())
            && state.work_path.as_ref().is_some_and(|path| path.is_file())
            && state.rom_dir == rom_dir.trim()
            && state.image_dir == image_dir.trim()
        {
            return Ok(());
        }
        // Once an item has been saved, reusing the current temporary LPL keeps
        // those edits when the user changes the ROM or image directory. A new
        // source playlist still starts from the original file.
        if state.source_path.as_deref() == Some(source_path.as_path())
            && state.work_path.as_ref().is_some_and(|path| path.is_file())
        {
            state
                .work_path
                .clone()
                .expect("work_path was checked above")
        } else {
            source_path.clone()
        }
    };
    let mut document =
        read_json_file(&base_path).map_err(|error| format!("Failed to read LPL: {error}"))?;
    let mut state = workspace.borrow_mut();
    state.reset();
    fs::create_dir_all(&state.temp_dir)
        .map_err(|error| format!("Failed to create the temporary directory: {error}"))?;
    let cover_index = build_workspace_cover_index(image_dir.trim());
    let items = document
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "LPL file is missing the items array".to_owned())?;
    for value in items {
        let Some(item) = value.as_object_mut() else {
            continue;
        };
        let Some(source_rom) = item.get("path").and_then(Value::as_str) else {
            continue;
        };
        let rom_path = resolve_workspace_rom(&base_path, source_rom, rom_dir.trim());
        item.insert(
            "path".into(),
            Value::String(rom_path.to_string_lossy().into_owned()),
        );
        let label = item
            .get("label")
            .and_then(Value::as_str)
            .or_else(|| {
                item.get(LPLX_METADATA_KEY)
                    .and_then(Value::as_object)
                    .and_then(|metadata| metadata.get("name"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_else(|| {
                rom_path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("")
            });
        let label = label.to_owned();
        let cover = find_workspace_cover(&cover_index, &rom_path, &label)
            .or_else(|| resolve_original_cover(&base_path, item));
        set_lpl_cover_path(item, cover.as_deref());
        ensure_lplx_metadata(item, &label);
    }
    let (list_index, work_path) = state.next_list_path();
    write_json_file(&work_path, &document)
        .map_err(|error| format!("Failed to write the temporary LPL: {error}"))?;
    state.source_path = Some(source_path);
    state.work_path = Some(work_path);
    state.list_index = Some(list_index);
    state.rom_dir = rom_dir.trim().to_owned();
    state.image_dir = image_dir.trim().to_owned();
    state.plan_cache = None;
    state.plan_path_index = None;
    state.current_index = None;
    window.set_lpl_work_path(
        state
            .work_path
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    Ok(())
}

fn preflight_lpl(path: &Path) -> Result<String, String> {
    let document = read_json_file(path).map_err(|error| format!("Failed to read LPL: {error}"))?;
    let items = document
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "LPL file is missing the items array".to_owned())?;
    let mut missing_roms = Vec::new();
    let mut invalid_covers = Vec::new();
    let mut unsupported = Vec::new();
    let mut duplicates = Vec::new();
    let mut seen = std::collections::HashMap::<String, usize>::new();
    for (position, value) in items.iter().enumerate() {
        let index = position + 1;
        let Some(item) = value.as_object() else {
            missing_roms.push(format!("#{index}: invalid entry"));
            continue;
        };
        let rom_text = item.get("path").and_then(Value::as_str).unwrap_or_default();
        let rom_path = PathBuf::from(rom_text);
        if !rom_path.is_file() {
            missing_roms.push(format!("#{index}: {}", rom_path.display()));
            continue;
        }
        let key = path_identity(&rom_path);
        if let Some(previous) = seen.insert(key, index) {
            duplicates.push(format!("#{previous} / #{index}"));
        }
        let extension = rom_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if !ROM_EXTENSIONS
            .iter()
            .any(|value| value.eq_ignore_ascii_case(extension))
        {
            unsupported.push(format!("#{index}: .{extension}"));
        }
        let cover = ["cover_path", "thumbnail_path", "cover", "thumbnail"]
            .into_iter()
            .filter_map(|key| item.get(key).and_then(Value::as_str))
            .map(PathBuf::from)
            .map(|candidate| {
                if candidate.is_absolute() {
                    candidate
                } else {
                    path.parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(candidate)
                }
            })
            .find(|candidate| candidate.is_file());
        if let Some(cover) = cover {
            let invalid = ImageReader::open(&cover)
                .map_err(|_| ())
                .and_then(|reader| reader.with_guessed_format().map_err(|_| ()))
                .and_then(|reader| reader.into_dimensions().map_err(|_| ()))
                .is_err();
            if invalid {
                invalid_covers.push(format!("#{index}: {}", cover.display()));
            }
        } else if item.keys().any(|key| {
            ["cover_path", "thumbnail_path", "cover", "thumbnail"].contains(&key.as_str())
        }) {
            invalid_covers.push(format!("#{index}: cover is missing"));
        }
    }
    let mut report = vec![format!("Total entries: {}", items.len())];
    report.push(format!(
        "Scannable: {}",
        items.len().saturating_sub(missing_roms.len())
    ));
    report.push(format!("Missing ROMs: {}", missing_roms.len()));
    report.push(format!("Duplicate files: {}", duplicates.len()));
    report.push(format!("Invalid covers: {}", invalid_covers.len()));
    report.push(format!("Unsupported formats: {}", unsupported.len()));
    for (title, values) in [
        ("Missing ROMs", missing_roms),
        ("Duplicate files", duplicates),
        ("Invalid covers", invalid_covers),
        ("Unsupported formats", unsupported),
    ] {
        if !values.is_empty() {
            report.push(format!(
                "{title}：{}",
                values.into_iter().take(12).collect::<Vec<_>>().join("、")
            ));
        }
    }
    Ok(report.join("\n"))
}

fn begin_preflight(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    lpl_path: PathBuf,
    save_path: PathBuf,
    cover_target: Option<(u32, u32)>,
) {
    workspace.borrow_mut().pending = Some(PendingConversion {
        lpl_path,
        save_path,
        cover_target,
    });
    window.set_preflight_stage(0);
    window.set_preflight_report("".into());
    window.set_preflight_visible(true);
}

fn scan_pending_conversion(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let Some(pending) = workspace.borrow().pending.clone() else {
        set_status(window, "no_pending_conversion", "No pending conversion");
        return;
    };
    let weak = window.as_weak();
    window.set_preflight_scanning(true);
    thread::spawn(move || {
        let result = preflight_lpl(&pending.lpl_path);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.set_preflight_scanning(false);
                // The user may have cancelled while the worker was scanning.
                if !window.get_preflight_visible() {
                    return;
                }
                match result {
                    Ok(report) => {
                        window.set_preflight_report(report.into());
                        window.set_preflight_stage(1);
                    }
                    Err(error) => {
                        window.set_preflight_visible(false);
                        set_status(&window, &error, &error);
                    }
                }
            }
        });
    });
}

fn request_conflict_choice(
    prompt_sender: &Arc<Mutex<Option<Sender<PromptResponse>>>>,
    weak: &slint::Weak<MainWindow>,
    cancel: &Arc<AtomicBool>,
    path: &Path,
) -> PromptResponse {
    let (sender, receiver): (Sender<PromptResponse>, Receiver<PromptResponse>) = mpsc::channel();
    if let Ok(mut slot) = prompt_sender.lock() {
        *slot = Some(sender);
    }
    let path = path.to_string_lossy().into_owned();
    let weak = weak.clone();
    if slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_conflict_path(path.into());
            window.set_conflict_visible(true);
        }
    })
    .is_err()
    {
        if let Ok(mut slot) = prompt_sender.lock() {
            if let Some(sender) = slot.take() {
                let _ = sender.send(PromptResponse::Stop);
            }
        }
    }
    loop {
        if cancel.load(Ordering::Relaxed) {
            return PromptResponse::Stop;
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(response) => return response,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return PromptResponse::Stop,
        }
    }
}

fn resolve_output_destination(
    conflict_mode: &Arc<Mutex<OutputConflictMode>>,
    prompt_sender: &Arc<Mutex<Option<Sender<PromptResponse>>>>,
    weak: &slint::Weak<MainWindow>,
    cancel: &Arc<AtomicBool>,
    destination: &Path,
) -> Result<Option<(PathBuf, bool)>, romx_core::RomxError> {
    if !destination.exists() {
        return Ok(Some((destination.to_owned(), false)));
    }
    let mode = conflict_mode
        .lock()
        .map(|value| *value)
        .unwrap_or(OutputConflictMode::Ask);
    match mode {
        OutputConflictMode::SkipAlways => Ok(None),
        OutputConflictMode::RenameAlways => Ok(Some((renamed_target(destination), false))),
        OutputConflictMode::ReplaceAlways => Ok(Some((destination.to_owned(), true))),
        OutputConflictMode::Ask => {
            match request_conflict_choice(prompt_sender, weak, cancel, destination) {
                PromptResponse::Skip => Ok(None),
                PromptResponse::SkipAll => {
                    if let Ok(mut value) = conflict_mode.lock() {
                        *value = OutputConflictMode::SkipAlways;
                    }
                    Ok(None)
                }
                PromptResponse::Rename => Ok(Some((renamed_target(destination), false))),
                PromptResponse::RenameAll => {
                    if let Ok(mut value) = conflict_mode.lock() {
                        *value = OutputConflictMode::RenameAlways;
                    }
                    Ok(Some((renamed_target(destination), false)))
                }
                PromptResponse::Replace => Ok(Some((destination.to_owned(), true))),
                PromptResponse::ReplaceAll => {
                    if let Ok(mut value) = conflict_mode.lock() {
                        *value = OutputConflictMode::ReplaceAlways;
                    }
                    Ok(Some((destination.to_owned(), true)))
                }
                _ => Err(romx_core::RomxError::Cancelled),
            }
        }
    }
}

fn request_error_choice(
    prompt_sender: &Arc<Mutex<Option<Sender<PromptResponse>>>>,
    weak: &slint::Weak<MainWindow>,
    cancel: &Arc<AtomicBool>,
    item_index: usize,
    error: &romx_core::RomxError,
) -> bool {
    if matches!(error, romx_core::RomxError::Cancelled) {
        return false;
    }
    let (sender, receiver): (Sender<PromptResponse>, Receiver<PromptResponse>) = mpsc::channel();
    if let Ok(mut slot) = prompt_sender.lock() {
        *slot = Some(sender);
    }
    let message = format!("Item {item_index}: {error}");
    let weak = weak.clone();
    if slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_error_message(message.into());
            window.set_error_visible(true);
        }
    })
    .is_err()
    {
        if let Ok(mut slot) = prompt_sender.lock() {
            if let Some(sender) = slot.take() {
                let _ = sender.send(PromptResponse::Stop);
            }
        }
    }
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(PromptResponse::Continue) => return true,
            Ok(_) => return false,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn start_pending_conversion(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let Some(pending) = workspace.borrow_mut().pending.take() else {
        return;
    };
    // Core writes one `<name>.tmp` file directly into the selected output
    // directory. Each file is committed only after it has been fully packed.
    let temp_output = pending.save_path.clone();
    if let Err(error) = fs::create_dir_all(&temp_output) {
        set_status(
            window,
            format!("Failed to create the temporary output directory: {error}"),
            format!("Temporary output directory failed: {error}"),
        );
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    workspace.borrow_mut().cancel_flag = Some(cancel.clone());
    window.set_preflight_visible(false);
    window.set_preflight_scanning(false);
    window.set_conversion_running(true);
    window.set_conversion_current(0);
    window.set_conversion_total(0);
    window.set_conversion_imported(0);
    window.set_conversion_skipped(0);
    let weak = window.as_weak();
    let prompt_sender = workspace.borrow().prompt_sender.clone();
    thread::spawn(move || {
        // Keep the paths from the temporary LPL intact. Core reads each
        // source image and performs PNG conversion/resizing when requested.
        let options = romx_core::ImportLplOptions {
            skip_missing: true,
            temporary_output: true,
            cover_target: pending.cover_target,
            ..Default::default()
        };
        let conflict_mode = Arc::new(Mutex::new(OutputConflictMode::Ask));
        let collision_skipped = Arc::new(AtomicUsize::new(0));
        let progress_collision_skipped = collision_skipped.clone();
        let progress_weak = weak.clone();
        let mut progress_callback =
            |current: usize, total: usize, imported: usize, skipped: usize| {
                let _ = slint::invoke_from_event_loop({
                    let progress_weak = progress_weak.clone();
                    let collision_counter = progress_collision_skipped.clone();
                    move || {
                        if let Some(window) = progress_weak.upgrade() {
                            window.set_conversion_current(current as i32);
                            window.set_conversion_total(total as i32);
                            window.set_conversion_imported(imported as i32);
                            window.set_conversion_skipped(
                                (skipped + collision_counter.load(Ordering::Relaxed)) as i32,
                            );
                        }
                    }
                });
            };
        let error_weak = weak.clone();
        let error_prompt_sender = prompt_sender.clone();
        let error_cancel = cancel.clone();
        let output_conflict_mode = conflict_mode.clone();
        let output_prompt_sender = prompt_sender.clone();
        let output_weak = weak.clone();
        let output_cancel = cancel.clone();
        let output_collision_skipped = collision_skipped.clone();
        let output_directory = temp_output.clone();
        let result = romx_core::import_lpl_with_output_handling(
            &pending.lpl_path,
            &temp_output,
            &options,
            &mut progress_callback,
            || cancel.load(Ordering::Relaxed),
            move |item_index, error| {
                request_error_choice(
                    &error_prompt_sender,
                    &error_weak,
                    &error_cancel,
                    item_index,
                    error,
                )
            },
            move |staged| {
                let filename = final_output_name(staged).ok_or_else(|| {
                    romx_core::RomxError::Invalid("Core produced an invalid output filename".into())
                })?;
                let destination = output_directory.join(filename);
                let destination_choice = resolve_output_destination(
                    &output_conflict_mode,
                    &output_prompt_sender,
                    &output_weak,
                    &output_cancel,
                    &destination,
                );
                let destination_choice = match destination_choice {
                    Ok(choice) => choice,
                    Err(error) => {
                        let _ = fs::remove_file(staged);
                        return Err(error);
                    }
                };
                match destination_choice {
                    Some((path, replace)) => match commit_staged(staged, &path, replace) {
                        Ok(committed) => Ok(Some(committed)),
                        Err(error) => {
                            let _ = fs::remove_file(staged);
                            Err(romx_core::RomxError::Invalid(error))
                        }
                    },
                    None => {
                        let _ = fs::remove_file(staged);
                        output_collision_skipped.fetch_add(1, Ordering::Relaxed);
                        Ok(None)
                    }
                }
            },
        );
        let result = if cancel.load(Ordering::Relaxed) {
            Err(romx_core::RomxError::Cancelled)
        } else {
            result
        };
        let final_result = result.and_then(|report| {
            // The GUI output directory contains only the converted ROMX
            // files. Core still creates a temporary manifest internally for
            // export compatibility, but it is not part of GUI output.
            let _ = fs::remove_file(&report.manifest_path);
            let collision_skipped = collision_skipped.load(Ordering::Relaxed);
            let imported = report.imported;
            Ok((report, imported, collision_skipped))
        });
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.set_conversion_running(false);
                window.set_conflict_visible(false);
                window.set_error_visible(false);
                match final_result {
                    Ok((report, imported, collision_skipped)) => {
                        window.set_conversion_current(report.total_items as i32);
                        window.set_conversion_total(report.total_items as i32);
                        window.set_conversion_imported(imported as i32);
                        window.set_conversion_skipped((report.skipped + collision_skipped) as i32);
                        refresh_lpl_output_files(&window);
                        set_status(
                            &window,
                            format!(
                                "Conversion complete: {} succeeded, {} skipped",
                                imported,
                                report.skipped + collision_skipped
                            ),
                            format!(
                                "Conversion complete: {} succeeded, {} skipped",
                                imported,
                                report.skipped + collision_skipped
                            ),
                        );
                    }
                    Err(romx_core::RomxError::Cancelled) => {
                        set_status(&window, "conversion_cancelled", "Conversion cancelled")
                    }
                    Err(error) => set_status(
                        &window,
                        format!("Conversion failed: {error}"),
                        format!("Conversion failed: {error}"),
                    ),
                }
            }
        });
    });
}

#[derive(Clone, Copy)]
enum LplFileListKind {
    Rom,
    Output,
}

fn lpl_file_entries(path: &str, kind: LplFileListKind) -> Vec<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut entries = match fs::read_dir(trimmed) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| match kind {
                LplFileListKind::Rom => path
                    .extension()
                    .and_then(|value| value.to_str())
                    .map(|value| ROM_EXTENSIONS.contains(&value.to_lowercase().as_str()))
                    .unwrap_or(false),
                LplFileListKind::Output => {
                    path.file_name().and_then(|value| value.to_str()) == Some("manifest.json")
                        || path
                            .extension()
                            .and_then(|value| value.to_str())
                            .map(|value| value.eq_ignore_ascii_case("romx"))
                            .unwrap_or(false)
                }
            })
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    entries.sort_unstable_by(|left, right| numeric_path_cmp(left, right));
    entries
}

fn numeric_path_cmp(left: &Path, right: &Path) -> std::cmp::Ordering {
    let left_name = file_name_string(left);
    let right_name = file_name_string(right);
    let left_stem = left
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let right_stem = right
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let left_digits = left_stem
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let right_digits = right_stem
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    match (
        left_digits.parse::<u64>().ok(),
        right_digits.parse::<u64>().ok(),
    ) {
        (Some(left_number), Some(right_number)) => left_number
            .cmp(&right_number)
            .then_with(|| left_name.to_lowercase().cmp(&right_name.to_lowercase())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left_name.to_lowercase().cmp(&right_name.to_lowercase()),
    }
}

fn file_name_string(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn paths_model(paths: &[PathBuf]) -> ModelRc<SharedString> {
    let values = paths
        .iter()
        .map(|path| file_name_string(path).into())
        .collect::<Vec<SharedString>>();
    ModelRc::from(Rc::new(VecModel::from(values)))
}

fn refresh_lpl_file_lists(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let rom_entries = lpl_file_entries(&window.get_lpl_rom_dir(), LplFileListKind::Rom);
    let output_entries = lpl_file_entries(&window.get_lpl_save_path(), LplFileListKind::Output);
    {
        let mut state = workspace.borrow_mut();
        state.rom_entries = rom_entries.clone();
    }
    window.set_lpl_rom_files(paths_model(&rom_entries));
    window.set_lpl_output_files(paths_model(&output_entries));
    window.set_lpl_rom_selected(-1);
    window.set_lpl_output_selected(-1);
}

fn refresh_lpl_output_files(window: &MainWindow) {
    let output_entries = lpl_file_entries(&window.get_lpl_save_path(), LplFileListKind::Output);
    window.set_lpl_output_files(paths_model(&output_entries));
    window.set_lpl_output_selected(-1);
}

fn cached_lpl_plan(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
) -> Result<ImportLplPlan, String> {
    if let Some(plan) = workspace.borrow().plan_cache.clone() {
        return Ok(plan);
    }
    let lpl_path = window.get_lpl_work_path();
    if lpl_path.trim().is_empty() {
        return Err("Temporary LPL is missing".into());
    }
    let plan = plan_lpl_import(Path::new(lpl_path.trim()), &Default::default())
        .map_err(|error| error.to_string())?;
    workspace.borrow_mut().plan_cache = Some(plan.clone());
    workspace.borrow_mut().plan_path_index = Some(
        plan.items
            .iter()
            .enumerate()
            .map(|(position, item)| (path_identity(&item.rom_path), position))
            .collect(),
    );
    Ok(plan)
}

fn refresh_lpl_preview(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    refresh_lpl_file_lists(window, workspace);
    let lpl_path = window.get_lpl_work_path();
    if lpl_path.trim().is_empty() {
        window.set_lpl_detail_title(
            localized_text(window, "select_lpl_preview", "Select ROM / ROMX").into(),
        );
        window.set_lpl_detail_format("-".into());
        window.set_lpl_detail_info(
            localized_text(
                window,
                "select_lpl_preview_info",
                "Select an LPL and ROM folder to preview entries and covers",
            )
            .into(),
        );
        window.set_cover_preview(Image::default());
        return;
    }

    match cached_lpl_plan(window, workspace) {
        Ok(plan) => {
            let available = plan.items.len();
            let skipped = plan.skipped;
            window.set_lpl_detail_title(format!("{} · {} items", plan.playlist, available).into());
            window.set_lpl_detail_format(format!("LPL · {} items", plan.total_items).into());
            window.set_lpl_detail_info(
                format!(
                    "Convertible: {} items{}",
                    available,
                    if skipped > 0 {
                        format!(", skipped: {}", skipped)
                    } else {
                        String::new()
                    }
                )
                .into(),
            );

            if let Some(cover_path) = plan
                .items
                .iter()
                .find_map(|item| item.cover_path.as_deref())
            {
                request_preview_path(window, workspace, Some(cover_path));
            } else {
                window.set_cover_preview(Image::default());
            }
        }
        Err(error) => {
            window.set_lpl_detail_title(
                localized_text(window, "lpl_read_failed", "Failed to read LPL").into(),
            );
            window.set_lpl_detail_format("-".into());
            window.set_lpl_detail_info(error.to_string().into());
            window.set_cover_preview(Image::default());
            set_status(
                window,
                format!("LPL preview failed: {error}"),
                format!("LPL preview failed: {error}"),
            );
        }
    }
}

fn metadata_string(value: Option<&Value>, key: &str) -> String {
    let result = value
        .and_then(|metadata| metadata.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if result.is_empty() && key == "label" {
        value
            .and_then(|metadata| metadata.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        result
    }
}

fn select_lpl_rom(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>, index: i32) {
    if index < 0 {
        return;
    }
    let path = workspace.borrow().rom_entries.get(index as usize).cloned();
    let Some(path) = path.as_deref() else {
        return;
    };
    let path_string = path.to_string_lossy().into_owned();
    let mut title = path_stem(&path_string);
    let mut format = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("ROM")
        .to_uppercase();
    let size = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut info = format!("ROM · {} bytes", size);
    let plan = cached_lpl_plan(window, workspace).ok();
    let path_key = path_identity(path);
    let selected_item_position = workspace
        .borrow()
        .plan_path_index
        .as_ref()
        .and_then(|index| index.get(&path_key).copied());
    let selected_item = selected_item_position
        .and_then(|position| plan.as_ref().and_then(|plan| plan.items.get(position)))
        .or_else(|| {
            plan.as_ref().and_then(|plan| {
                plan.items
                    .iter()
                    .find(|item| paths_equivalent(&item.rom_path, path))
            })
        });
    if let Some(item) = selected_item {
        title = item.label.clone();
        format = format!("{} · {}", item.platform, item.payload_format).to_uppercase();
        info = format!("{} · ROM {} bytes", item.platform, size);
    }

    window.set_lpl_rom_selected(index);
    window.set_lpl_output_selected(-1);
    window.set_lpl_selected_path(path_string.clone().into());
    let selected_item_index = selected_item.map(|item| item.index as i32).unwrap_or(0);
    window.set_lpl_selected_item_index(selected_item_index);
    if let Some(cover_path) = selected_item.and_then(|item| item.cover_path.as_deref()) {
        request_preview_path(window, workspace, Some(cover_path));
    } else {
        window.set_cover_preview(Image::default());
    }
    window.set_lpl_detail_title(title.into());
    window.set_lpl_detail_format(format.into());
    window.set_lpl_detail_info(info.into());
    set_status(window, "rom_selected", "ROM selected");
}

fn select_lpl_output(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>, index: i32) {
    if index < 0 {
        return;
    }
    let entries = lpl_file_entries(&window.get_lpl_save_path(), LplFileListKind::Output);
    let Some(path) = entries.get(index as usize) else {
        return;
    };
    let path_string = path.to_string_lossy().into_owned();
    window.set_lpl_rom_selected(-1);
    window.set_lpl_output_selected(index);
    window.set_lpl_selected_path(path_string.clone().into());
    window.set_lpl_selected_item_index(
        path.file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(0),
    );

    if path.file_name().and_then(|value| value.to_str()) == Some("manifest.json") {
        window.set_lpl_selected_path("".into());
        window.set_lpl_selected_item_index(0);
        let manifest = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        match manifest {
            Ok(manifest) => {
                let playlist = metadata_string(Some(&manifest), "playlist");
                let items = manifest
                    .get("items")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                window.set_cover_preview(Image::default());
                window.set_lpl_detail_title("manifest.json".into());
                window.set_lpl_detail_format("ROMX manifest".into());
                window.set_lpl_detail_info(
                    format!("Playlist: {} · items: {}", playlist, items).into(),
                );
                set_status(window, "manifest_selected", "Manifest selected");
            }
            Err(error) => set_status(
                window,
                format!("Manifest read failed: {error}"),
                format!("Manifest read failed: {error}"),
            ),
        }
        return;
    }

    match read_metadata_cover_path(path) {
        Ok(document) => {
            let metadata = document.metadata.as_ref();
            let title = {
                let label = metadata_string(metadata, "label");
                if label.is_empty() {
                    path_stem(&path_string)
                } else {
                    label
                }
            };
            let platform = platform_name_from_id(document.footer.platform_id)
                .unwrap_or("ROMX")
                .to_owned();
            let payload_format = document
                .entries
                .iter()
                .find(|entry| entry.entrypoint)
                .and_then(|entry| format_extension(entry.format_id))
                .unwrap_or("romx")
                .to_owned();
            let description = metadata_string(metadata, "description");
            let mut info = format!(
                "{} · ROM {} bytes · cover: {}",
                &platform,
                document.footer.rom.size,
                if document.cover.is_some() {
                    "yes"
                } else {
                    "no"
                }
            );
            if !description.is_empty() {
                info.push_str(&format!("\n{}", description));
            }
            window.set_lpl_detail_title(title.into());
            window.set_lpl_detail_format(
                if payload_format.is_empty() {
                    "ROMX".to_owned()
                } else {
                    format!("ROMX · {payload_format}")
                }
                .into(),
            );
            window.set_lpl_detail_info(info.into());
            if let Some(cover) = document.cover.as_deref() {
                request_preview_bytes(window, workspace, preview_path_key(path), cover);
            } else {
                window.set_cover_preview(Image::default());
            }
            set_status(window, "romx_selected", "ROMX selected");
        }
        Err(error) => {
            window.set_lpl_detail_title(
                localized_text(window, "romx_read_failed", "Failed to read ROMX").into(),
            );
            window.set_lpl_detail_format("-".into());
            window.set_lpl_detail_info(format!("{error}").into());
            window.set_cover_preview(Image::default());
            set_status(
                window,
                format!("ROMX read failed: {error}"),
                format!("ROMX read failed: {error}"),
            );
        }
    }
}

fn is_romx_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| {
                ROMX_EXTENSIONS
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(extension))
            })
}

fn collect_unpack_romx_files(path: &Path, output: &mut Vec<PathBuf>) {
    if is_romx_file(path) {
        output.push(path.to_owned());
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let candidate = entry.path();
        if candidate.is_dir() {
            collect_unpack_romx_files(&candidate, output);
        } else if is_romx_file(&candidate) {
            output.push(candidate);
        }
    }
}

fn unpack_romx_entries(window: &MainWindow) -> Vec<PathBuf> {
    let input = window.get_unpack_input_path();
    let path = Path::new(input.trim());
    let mut entries = Vec::new();
    if !input.trim().is_empty() {
        collect_unpack_romx_files(path, &mut entries);
    }
    entries.sort_unstable_by(|left, right| numeric_path_cmp(left, right));
    entries
}

fn preview_unpack_romx(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>, path: &Path) {
    let path_string = path.to_string_lossy().into_owned();
    match read_metadata_cover_path(path) {
        Ok(document) => {
            let metadata = document.metadata.as_ref();
            let label = metadata_string(metadata, "label");
            let title = if label.is_empty() {
                path_stem(&path_string)
            } else {
                label
            };
            let payload_format = document
                .entries
                .iter()
                .find(|entry| entry.entrypoint)
                .and_then(|entry| format_extension(entry.format_id))
                .unwrap_or("romx")
                .to_owned();
            let platform = platform_name_from_id(document.footer.platform_id)
                .unwrap_or("ROMX")
                .to_owned();
            let description = metadata_string(metadata, "description");
            let mut info = format!(
                "{} · ROM {} bytes · cover: {}",
                &platform,
                document.footer.rom.size,
                if document.cover.is_some() {
                    "yes"
                } else {
                    "no"
                }
            );
            if !description.is_empty() {
                info.push_str(&format!("\n{description}"));
            }
            window.set_lpl_detail_title(title.into());
            window.set_lpl_detail_format(
                if payload_format.is_empty() {
                    "ROMX".to_owned()
                } else {
                    format!("ROMX · {payload_format}")
                }
                .into(),
            );
            window.set_lpl_detail_info(info.into());
            if let Some(cover) = document.cover.as_deref() {
                request_preview_bytes(window, workspace, preview_path_key(path), cover);
            } else {
                window.set_cover_preview(Image::default());
            }
            set_status(window, "romx_selected", "ROMX selected");
        }
        Err(error) => {
            window.set_lpl_detail_title(
                localized_text(window, "romx_read_failed", "Failed to read ROMX").into(),
            );
            window.set_lpl_detail_format("-".into());
            window.set_lpl_detail_info(error.to_string().into());
            window.set_cover_preview(Image::default());
            set_status(
                window,
                format!("ROMX read failed: {error}"),
                format!("ROMX read failed: {error}"),
            );
        }
    }
}

fn refresh_unpack_preview(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let entries = unpack_romx_entries(window);
    window.set_unpack_romx_files(paths_model(&entries));
    window.set_unpack_romx_selected(-1);
    if let Some(first) = entries.first() {
        window.set_unpack_romx_selected(0);
        preview_unpack_romx(window, workspace, first);
    } else {
        window.set_lpl_detail_title(
            localized_text(window, "select_romx_source", "Select a ROMX file or folder").into(),
        );
        window.set_lpl_detail_format("-".into());
        window.set_lpl_detail_info(
            localized_text(
                window,
                "select_romx_source_info",
                "Select an entry to preview its cover and game information",
            )
            .into(),
        );
        window.set_cover_preview(Image::default());
    }
}

fn choose_unpack_source(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>, folder: bool) {
    let dialog = FileDialog::new()
        .set_title(if folder {
            "Choose ROMX folder"
        } else {
            "Choose ROMX file"
        })
        .add_filter("ROMX", ROMX_EXTENSIONS);
    let selected = if folder {
        dialog.pick_folder()
    } else {
        dialog.pick_file()
    };
    let Some(path) = selected else {
        return;
    };
    let path_string = path.to_string_lossy().into_owned();
    window.set_unpack_input_path(path_string.clone().into());
    refresh_unpack_preview(window, workspace);
    set_status(
        window,
        format!("Input selected: {path_string}"),
        format!("Input selected: {path_string}"),
    );
}

#[derive(Clone, Copy)]
enum UnpackDirectoryKind {
    Rom,
    Cover,
    Lpl,
}

fn unpack_playlist_name(input: &Path) -> String {
    input
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("ROMX")
        .to_owned()
}

fn content_root_from_lpl_dir(lpl_dir: &Path) -> PathBuf {
    lpl_dir.to_owned()
}

fn content_root_from_rom_dir(rom_dir: &Path, playlist: &str) -> PathBuf {
    let parent = rom_dir.parent().unwrap_or(rom_dir);
    let directory_is_playlist = rom_dir
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(playlist));
    let roms_dir = if directory_is_playlist {
        parent
    } else {
        rom_dir
    };
    if roms_dir
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("roms"))
    {
        roms_dir.parent().unwrap_or(roms_dir).to_owned()
    } else {
        parent.to_owned()
    }
}

fn default_unpack_paths(
    input: &Path,
    lpl_value: &str,
    rom_value: &str,
    cover_value: &str,
) -> (String, PathBuf, PathBuf, PathBuf, PathBuf) {
    let playlist = unpack_playlist_name(input);
    let content_root = if !lpl_value.is_empty() {
        content_root_from_lpl_dir(Path::new(lpl_value))
    } else if !rom_value.is_empty() {
        content_root_from_rom_dir(Path::new(rom_value), &playlist)
    } else {
        input.parent().unwrap_or_else(|| Path::new(".")).to_owned()
    };
    let lpl_dir = content_root.join("retroarch").join("playlists");
    let rom_dir = if rom_value.is_empty() {
        content_root.join("roms").join(&playlist)
    } else {
        PathBuf::from(rom_value)
    };
    let cover_dir = if cover_value.is_empty() {
        content_root
            .join("retroarch")
            .join("thumbnails")
            .join(&playlist)
            .join("Named_Snaps")
    } else {
        PathBuf::from(cover_value)
    };
    (playlist, content_root, lpl_dir, rom_dir, cover_dir)
}

fn choose_unpack_directory(window: &MainWindow, kind: UnpackDirectoryKind) {
    let Some(path) = FileDialog::new()
        .set_title("Choose directory")
        .pick_folder()
    else {
        return;
    };
    let value = path.to_string_lossy().into_owned();
    match kind {
        UnpackDirectoryKind::Rom => window.set_unpack_rom_dir(value.clone().into()),
        UnpackDirectoryKind::Cover => window.set_unpack_cover_dir(value.clone().into()),
        UnpackDirectoryKind::Lpl => {
            window.set_unpack_lpl_dir(value.clone().into());
            let input_value = window.get_unpack_input_path();
            if !input_value.trim().is_empty() {
                let (_, _, _, default_rom, default_cover) =
                    default_unpack_paths(Path::new(input_value.trim()), &value, "", "");
                window.set_unpack_rom_dir(default_rom.to_string_lossy().into_owned().into());
                window.set_unpack_cover_dir(default_cover.to_string_lossy().into_owned().into());
            }
        }
    }
    set_status(
        window,
        format!("Folder selected: {value}"),
        format!("Folder selected: {value}"),
    );
}

fn select_unpack_romx(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>, index: i32) {
    if index < 0 {
        return;
    }
    let entries = unpack_romx_entries(window);
    let Some(path) = entries.get(index as usize) else {
        return;
    };
    window.set_unpack_romx_selected(index);
    preview_unpack_romx(window, workspace, path);
}

fn start_unpack_conversion(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    batch: bool,
) {
    let input = PathBuf::from(window.get_unpack_input_path().trim());
    if (batch && !input.is_dir()) || (!batch && !input.is_file()) {
        set_status(
            window,
            if batch {
                "batch_conversion_requires_folder"
            } else {
                "file_conversion_requires_file"
            },
            if batch {
                "Batch conversion requires a ROMX folder"
            } else {
                "File conversion requires one ROMX file"
            },
        );
        return;
    }
    let lpl_value = window.get_unpack_lpl_dir();
    let rom_value = window.get_unpack_rom_dir();
    let cover_value = window.get_unpack_cover_dir();
    let (playlist, content_root, lpl_dir, rom_dir, cover_dir) = default_unpack_paths(
        &input,
        lpl_value.trim(),
        rom_value.trim(),
        cover_value.trim(),
    );
    let custom_rom_path = window.get_unpack_lpl_rom_path().trim().to_owned();
    let custom_cover_path = window.get_unpack_lpl_cover_path().trim().to_owned();
    let options = ExportLplOptions {
        playlist_name: Some(playlist.clone()),
        lpl_path: Some(lpl_dir.join(format!("{playlist}.lpl"))),
        rom_dir: Some(rom_dir),
        cover_dir: Some(cover_dir),
        lpl_rom_prefix: (!custom_rom_path.is_empty()).then_some(custom_rom_path),
        lpl_cover_prefix: (!custom_cover_path.is_empty()).then_some(custom_cover_path),
        temporary_output: true,
        ..Default::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    workspace.borrow_mut().cancel_flag = Some(cancel.clone());
    let prompt_sender = workspace.borrow().prompt_sender.clone();
    let weak = window.as_weak();
    window.set_conversion_running(true);
    window.set_conversion_current(0);
    window.set_conversion_total(0);
    window.set_conversion_imported(0);
    window.set_conversion_skipped(0);
    thread::spawn(move || {
        let conflict_mode = Arc::new(Mutex::new(OutputConflictMode::Ask));
        let progress_weak = weak.clone();
        let mut progress = |current: usize, total: usize, exported: usize, skipped: usize| {
            let progress_weak = progress_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = progress_weak.upgrade() {
                    window.set_conversion_current(current as i32);
                    window.set_conversion_total(total as i32);
                    window.set_conversion_imported(exported as i32);
                    window.set_conversion_skipped(skipped as i32);
                }
            });
        };
        let error_prompt = prompt_sender.clone();
        let error_weak = weak.clone();
        let error_cancel = cancel.clone();
        let output_prompt = prompt_sender.clone();
        let output_weak = weak.clone();
        let output_cancel = cancel.clone();
        let output_conflict_mode = conflict_mode.clone();
        let result = export_lpl_with_output_handling(
            &input,
            &content_root,
            &options,
            &mut progress,
            || cancel.load(Ordering::Relaxed),
            move |index, error| {
                request_error_choice(&error_prompt, &error_weak, &error_cancel, index, error)
            },
            move |staged| {
                let filename = final_output_name(staged).ok_or_else(|| {
                    romx_core::RomxError::Invalid("Core produced an invalid output filename".into())
                })?;
                let destination = staged
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(filename);
                let choice = resolve_output_destination(
                    &output_conflict_mode,
                    &output_prompt,
                    &output_weak,
                    &output_cancel,
                    &destination,
                );
                let choice = match choice {
                    Ok(choice) => choice,
                    Err(error) => {
                        let _ = fs::remove_file(staged);
                        return Err(error);
                    }
                };
                match choice {
                    Some((target, replace)) => match commit_staged(staged, &target, replace) {
                        Ok(committed) => Ok(Some(committed)),
                        Err(error) => {
                            let _ = fs::remove_file(staged);
                            Err(romx_core::RomxError::Invalid(error))
                        }
                    },
                    None => {
                        let _ = fs::remove_file(staged);
                        Ok(None)
                    }
                }
            },
        );
        let result = if cancel.load(Ordering::Relaxed) {
            Err(romx_core::RomxError::Cancelled)
        } else {
            result
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.set_conversion_running(false);
                window.set_conflict_visible(false);
                window.set_error_visible(false);
                match result {
                    Ok(report) => {
                        window.set_conversion_current(report.total_items as i32);
                        window.set_conversion_total(report.total_items as i32);
                        window.set_conversion_imported(report.exported as i32);
                        window.set_conversion_skipped(report.skipped as i32);
                        set_status(
                            &window,
                            format!(
                                "Extraction complete: {} succeeded, {} skipped, LPL: {}",
                                report.exported,
                                report.skipped,
                                report.lpl_path.display()
                            ),
                            format!(
                                "Extraction complete: {} succeeded, {} skipped, LPL: {}",
                                report.exported,
                                report.skipped,
                                report.lpl_path.display()
                            ),
                        );
                    }
                    Err(romx_core::RomxError::Cancelled) => {
                        set_status(&window, "extraction_cancelled", "Extraction cancelled")
                    }
                    Err(error) => set_status(
                        &window,
                        format!("Extraction failed: {error}"),
                        format!("Extraction failed: {error}"),
                    ),
                }
            }
        });
    });
}

fn lpl_item_value<'a>(item: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    item.get(LPLX_METADATA_KEY)
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get(key))
        .or_else(|| item.get(key))
        .or_else(|| {
            item.get("x-retroarch")
                .and_then(Value::as_object)
                .and_then(|retroarch| retroarch.get("extra"))
                .and_then(Value::as_object)
                .and_then(|extra| extra.get(key))
        })
}

fn set_lpl_item_value(item: &mut Map<String, Value>, key: &str, value: Value) {
    if ROMX_LPLX_METADATA_FIELDS.contains(&key) {
        set_lplx_metadata_value(item, key, value.clone());
        if key == "name" {
            item.insert("label".into(), value);
        }
        return;
    }
    if key == "label" && item.contains_key(LPLX_METADATA_KEY) {
        set_lplx_metadata_value(item, "name", value.clone());
    }
    if item.contains_key(key) {
        item.insert(key.into(), value);
        return;
    }
    if let Some(extra) = item
        .get_mut("x-retroarch")
        .and_then(Value::as_object_mut)
        .and_then(|retroarch| retroarch.get_mut("extra"))
        .and_then(Value::as_object_mut)
    {
        if extra.contains_key(key) {
            extra.insert(key.into(), value);
            return;
        }
    }
    item.insert(key.into(), value);
}

fn remove_lplx_metadata_value(item: &mut Map<String, Value>, key: &str) {
    if let Some(metadata) = item
        .get_mut(LPLX_METADATA_KEY)
        .and_then(Value::as_object_mut)
    {
        metadata.remove(key);
    }
}

fn lpl_item_metadata(item: &Map<String, Value>) -> Value {
    let mut metadata = Map::new();
    for key in ["name", "genre", "developer", "release_date", "origin"] {
        if let Some(value) = lpl_item_value(item, key) {
            metadata.insert(key.into(), value.clone());
        }
    }
    if !metadata.contains_key("name") {
        if let Some(value) = item.get("label") {
            metadata.insert("name".into(), value.clone());
        }
    }
    if !metadata.contains_key("name") {
        metadata.insert("name".into(), Value::String(String::new()));
    }
    Value::Object(metadata)
}

fn lpl_item_cover_path(lpl_path: &Path, item: &Map<String, Value>) -> Option<PathBuf> {
    ["cover_path", "thumbnail_path", "cover", "thumbnail"]
        .into_iter()
        .filter_map(|key| item.get(key).and_then(Value::as_str))
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                lpl_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(path)
            }
        })
        .find(|path| path.is_file())
}

fn set_edit_cover(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    cover_path: Option<&Path>,
) {
    if let Some(path) = cover_path {
        window.set_cover_path(path.to_string_lossy().into_owned().into());
        request_preview_path(window, workspace, Some(path));
    } else {
        window.set_cover_path("".into());
        window.set_cover_preview(Image::default());
    }
}

fn begin_lpl_edit(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let item_index = window.get_lpl_selected_item_index();
    let work_path = window.get_lpl_work_path();
    if work_path.trim().is_empty() || item_index <= 0 {
        set_status(
            window,
            "select_rom_or_romx_first",
            "Select a ROM or ROMX file first",
        );
        return;
    }
    let document = match read_json_file(Path::new(work_path.trim())) {
        Ok(document) => document,
        Err(error) => {
            set_status(
                window,
                format!("Temporary LPL read failed: {error}"),
                format!("Temporary LPL read failed: {error}"),
            );
            return;
        }
    };
    let Some(item) = document
        .get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.get(item_index as usize - 1))
        .and_then(Value::as_object)
    else {
        set_status(window, "lpl_item_not_found", "LPL item not found");
        return;
    };
    let Some(rom_path) = item.get("path").and_then(Value::as_str) else {
        set_status(
            window,
            "lpl_item_missing_rom_path",
            "LPL item has no ROM path",
        );
        return;
    };
    let metadata = lpl_item_metadata(item);
    let cover_path = lpl_item_cover_path(Path::new(work_path.trim()), item);
    let edit_item = item.clone();
    let mut edit_document = document.clone();
    let Some(object) = edit_document.as_object_mut() else {
        set_status(
            window,
            "lpl_root_must_be_object",
            "LPL root must be an object",
        );
        return;
    };
    object.insert("items".into(), Value::Array(vec![Value::Object(edit_item)]));
    let Some(edit_path) = workspace.borrow().new_edit_path() else {
        set_status(
            window,
            "temporary_lpl_not_ready",
            "Temporary LPL is not ready",
        );
        return;
    };
    if let Some(old_path) = workspace.borrow_mut().edit_path.take() {
        let _ = fs::remove_file(old_path);
    }
    if let Err(error) = write_json_file(&edit_path, &edit_document) {
        set_status(
            window,
            format!("Temporary edit LPL write failed: {error}"),
            format!("Temporary edit LPL write failed: {error}"),
        );
        return;
    }
    workspace.borrow_mut().edit_path = Some(edit_path);
    window.set_rom_path(rom_path.into());
    window.set_metadata_path("".into());
    window.set_save_path(window.get_lpl_save_path());
    set_metadata_form(window, &metadata);
    if !supported_platform(&metadata_text(&metadata, "platform")) {
        window.set_platform(platform_for_path(&window.get_rom_path()).into());
    }
    set_edit_cover(window, workspace, cover_path.as_deref());
    workspace.borrow_mut().current_index = Some(item_index as usize);
    window.set_editing_lpl_file(true);
    window.set_active_page(0);
    window.set_pack_subpage(0);
    set_status(
        window,
        format!("Editing item {item_index}; save your changes when ready"),
        format!("Editing item {item_index}; save your changes when ready"),
    );
}

fn lpl_edit_context(
    workspace: &Rc<RefCell<LplWorkspace>>,
) -> Result<(usize, PathBuf, PathBuf), String> {
    let state = workspace.borrow();
    let item_index = state
        .current_index
        .ok_or_else(|| "No item is currently being edited".to_owned())?;
    let edit_path = state
        .edit_path
        .clone()
        .ok_or_else(|| "Temporary edit LPL is missing".to_owned())?;
    let work_path = state
        .work_path
        .clone()
        .ok_or_else(|| "Temporary LPL is missing".to_owned())?;
    Ok((item_index, edit_path, work_path))
}

fn update_lpl_edit_file(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
) -> Result<(usize, PathBuf, PathBuf, Option<PathBuf>), String> {
    let (item_index, edit_path, work_path) = lpl_edit_context(workspace)?;
    let mut document = read_json_file(&edit_path)
        .map_err(|error| format!("Temporary edit LPL read failed: {error}"))?;
    let Some(item) = document
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .and_then(|items| items.first_mut())
        .and_then(Value::as_object_mut)
    else {
        return Err("Temporary edit LPL item is missing".into());
    };
    let rom_path_text = window.get_rom_path();
    if rom_path_text.trim().is_empty() {
        return Err("ROM path cannot be empty".into());
    }
    let rom_path = absolute_path(Path::new(rom_path_text.trim()));
    item.insert(
        "path".into(),
        Value::String(rom_path.to_string_lossy().into_owned()),
    );
    let title = if window.get_display_title().trim().is_empty() {
        path_stem(&rom_path_text)
    } else {
        window.get_display_title().trim().to_owned()
    };
    set_lpl_item_value(item, "name", title.into());
    set_lpl_item_value(item, "genre", parse_genre(&window.get_genre()));
    set_lpl_item_value(
        item,
        "platform",
        Value::String(if supported_platform(window.get_platform().trim()) {
            window.get_platform().trim().to_owned()
        } else {
            platform_for_path(&rom_path_text).to_owned()
        }),
    );
    let developer = window.get_developer().trim().to_owned();
    let origin = window.get_origin().trim().to_owned();
    for (key, value) in [
        ("developer", developer.as_str()),
        ("origin", origin.as_str()),
    ] {
        if value.is_empty() {
            remove_lplx_metadata_value(item, key);
        } else {
            set_lpl_item_value(item, key, Value::String(value.to_owned()));
        }
    }
    if window.get_release_date().trim().is_empty() {
        remove_lplx_metadata_value(item, "release_date");
    } else {
        set_lpl_item_value(
            item,
            "release_date",
            Value::String(window.get_release_date().trim().to_owned()),
        );
    }
    let cover_path = if window.get_cover_path().trim().is_empty() {
        set_lpl_cover_path(item, None);
        None
    } else {
        let source = absolute_path(Path::new(window.get_cover_path().trim()));
        set_lpl_cover_path(item, Some(&source));
        Some(source)
    };
    write_json_file(&edit_path, &document)
        .map_err(|error| format!("Temporary edit LPL save failed: {error}"))?;
    Ok((item_index, edit_path, work_path, cover_path))
}

fn merge_lpl_edit(item_index: usize, edit_path: &Path, work_path: &Path) -> Result<(), String> {
    let edit_document = read_json_file(edit_path)
        .map_err(|error| format!("Temporary edit LPL read failed: {error}"))?;
    let edit_item = edit_document
        .get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned()
        .ok_or_else(|| "Temporary edit LPL item is missing".to_owned())?;
    let mut document =
        read_json_file(work_path).map_err(|error| format!("Temporary LPL read failed: {error}"))?;
    let items = document
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "Temporary LPL is missing the items array".to_owned())?;
    let Some(target) = items.get_mut(item_index.saturating_sub(1)) else {
        return Err("Temporary LPL item is missing".into());
    };
    *target = edit_item;
    write_json_file(work_path, &document)
        .map_err(|error| format!("Temporary LPL merge failed: {error}"))?;
    Ok(())
}

fn save_lpl_edit(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) -> bool {
    let (item_index, edit_path, work_path, cover_path) =
        match update_lpl_edit_file(window, workspace) {
            Ok(value) => value,
            Err(error) => {
                set_status(window, &error, &error);
                return false;
            }
        };
    if let Err(error) = merge_lpl_edit(item_index, &edit_path, &work_path) {
        set_status(window, &error, &error);
        return false;
    }
    {
        let mut state = workspace.borrow_mut();
        state.plan_cache = None;
        state.plan_path_index = None;
    }
    set_edit_cover(window, workspace, cover_path.as_deref());
    set_status(
        window,
        format!("Item {item_index} saved and merged into the temporary LPL"),
        format!("Item {item_index} saved and merged into the temporary LPL"),
    );
    true
}

fn return_from_lpl_edit(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    if workspace.borrow().edit_path.is_none() {
        set_status(
            window,
            "No item is currently being edited",
            "No LPL item is being edited",
        );
        return;
    }
    if let Some(edit_path) = workspace.borrow_mut().edit_path.take() {
        let _ = fs::remove_file(&edit_path);
        let _ = fs::remove_file(edit_path.with_extension("lplx.tmp"));
    }
    workspace.borrow_mut().current_index = None;
    window.set_editing_lpl_file(false);
    window.set_active_page(0);
    window.set_pack_subpage(1);
    refresh_lpl_preview(window, workspace);
    set_status(
        window,
        "Ended temporary LPL editing and returned to the LPL page",
        "Ended temporary edit LPL and returned to the LPL page",
    );
}

fn choose_lpl(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let Some(path) = FileDialog::new()
        .set_title("Choose LPL/LPLX file")
        .add_filter("LPL/LPLX", &["lpl", "lplx"])
        .pick_file()
    else {
        return;
    };
    workspace.borrow_mut().reset();
    window.set_editing_lpl_file(false);
    window.set_lpl_selected_path("".into());
    window.set_lpl_selected_item_index(0);
    window.set_lpl_work_path("".into());
    window.set_lpl_path(path.to_string_lossy().into_owned().into());
    if let Err(error) = prepare_lpl_workspace(window, workspace, true) {
        set_status(window, &error, &error);
        return;
    }
    refresh_lpl_preview(window, workspace);
    set_status(
        window,
        format!("LPL selected: {}", path.display()),
        format!("LPL selected: {}", path.display()),
    );
}

fn choose_lpl_directory(
    window: &MainWindow,
    kind: LplDirectoryKind,
    workspace: &Rc<RefCell<LplWorkspace>>,
) {
    let title = match kind {
        LplDirectoryKind::Rom => "Choose ROM folder",
        LplDirectoryKind::Image => "Choose image folder",
        LplDirectoryKind::Save => "Choose LPL output folder",
    };
    let Some(path) = FileDialog::new().set_title(title).pick_folder() else {
        return;
    };
    let path_string = path.to_string_lossy().into_owned();
    match kind {
        LplDirectoryKind::Rom => window.set_lpl_rom_dir(path_string.clone().into()),
        LplDirectoryKind::Image => window.set_lpl_image_dir(path_string.clone().into()),
        LplDirectoryKind::Save => window.set_lpl_save_path(path_string.clone().into()),
    }
    let force_prepare = workspace.borrow().current_index.is_none();
    if let Err(error) = prepare_lpl_workspace(window, workspace, force_prepare) {
        set_status(window, &error, &error);
        return;
    }
    refresh_lpl_preview(window, workspace);
    set_status(
        window,
        format!("Folder selected: {path_string}"),
        format!("Folder selected: {path_string}"),
    );
}

#[derive(Clone, Copy)]
enum LplDirectoryKind {
    Rom,
    Image,
    Save,
}

fn convert_lpl(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let lpl_path = window.get_lpl_path();
    let save_path = window.get_lpl_save_path();
    if lpl_path.trim().is_empty() || save_path.trim().is_empty() {
        set_status(
            window,
            "choose_lpl_and_save_path_first",
            "Choose an LPL file and output folder first",
        );
        return;
    }
    if let Err(error) = prepare_lpl_workspace(window, workspace, false) {
        set_status(window, &error, &error);
        return;
    }
    let Some(work_path) = workspace.borrow().work_path.clone() else {
        set_status(window, "temporary_lpl_missing", "Temporary LPL is missing");
        return;
    };
    begin_preflight(
        window,
        workspace,
        work_path,
        PathBuf::from(save_path.trim()),
        None,
    );
}

fn remove_single_temp(path: &Path, payload_path: Option<&Path>) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("lplx.tmp"));
    if let Some(payload_path) = payload_path {
        let _ = fs::remove_file(payload_path);
    }
}

fn prepare_single_input(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
) -> Result<(String, PathBuf, Option<PathBuf>), String> {
    let input_path = absolute_path(Path::new(window.get_rom_path().trim()));
    let is_romx = input_path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            ROMX_EXTENSIONS
                .iter()
                .any(|ext| ext.eq_ignore_ascii_case(value))
        });
    if !is_romx {
        let format = payload_format(input_path.to_string_lossy().as_ref())?;
        let format = if matches!(format.as_str(), "gb" | "gbc") {
            let bytes =
                fs::read(&input_path).map_err(|error| format!("Failed to read ROM: {error}"))?;
            classify_gb_payload(&bytes, Some(&format))
                .map_err(|error| format!("Failed to read ROM: {error}"))?
                .to_owned()
        } else {
            format
        };
        return Ok((format, input_path, None));
    }

    let document =
        read_path(&input_path).map_err(|error| format!("Failed to read ROMX: {error}"))?;
    let entry_format = document
        .entries
        .iter()
        .find(|entry| entry.entrypoint)
        .and_then(|entry| format_extension(entry.format_id))
        .ok_or_else(|| "ROMX entry format is not registered".to_owned())?;
    let declared_format = entry_format.to_owned();
    let format = if matches!(declared_format.as_str(), "gb" | "gbc") {
        classify_gb_payload(&document.rom, Some(&declared_format))
            .map_err(|error| format!("Failed to read the ROM inside ROMX: {error}"))?
            .to_owned()
    } else {
        declared_format
    };
    let payload_path = {
        let state = workspace.borrow();
        fs::create_dir_all(&state.temp_dir)
            .map_err(|error| format!("Failed to create the temporary directory: {error}"))?;
        let path = state.temp_dir.join(format!("temp-romx-payload.{format}"));
        fs::write(&path, &document.rom)
            .map_err(|error| format!("Failed to extract the ROM from ROMX: {error}"))?;
        path
    };
    Ok((format, payload_path.clone(), Some(payload_path)))
}

fn run_single_conversion(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    conversion: SingleConversion,
) {
    let SingleConversion {
        lpl_path: temporary_lpl,
        payload_path: temporary_payload,
        cover_target,
        save_path,
        output_path,
        replace,
    } = conversion;
    let temp_output = save_path.clone();
    if let Err(error) = fs::create_dir_all(&temp_output) {
        set_status(
            window,
            format!("Failed to create the temporary output directory: {error}"),
            format!("Temporary output directory failed: {error}"),
        );
        remove_single_temp(&temporary_lpl, temporary_payload.as_deref());
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    workspace.borrow_mut().cancel_flag = Some(cancel.clone());
    let prompt_sender = workspace.borrow().prompt_sender.clone();
    let weak = window.as_weak();
    window.set_conversion_running(true);
    window.set_conversion_current(0);
    window.set_conversion_total(1);
    window.set_conversion_imported(0);
    window.set_conversion_skipped(0);
    thread::spawn(move || {
        let error_weak = weak.clone();
        let error_prompt_sender = prompt_sender.clone();
        let error_cancel = cancel.clone();
        let mut progress_callback =
            |current: usize, total: usize, imported: usize, skipped: usize| {
                let _ = slint::invoke_from_event_loop({
                    let progress_weak = weak.clone();
                    move || {
                        if let Some(window) = progress_weak.upgrade() {
                            window.set_conversion_current(current as i32);
                            window.set_conversion_total(total as i32);
                            window.set_conversion_imported(imported as i32);
                            window.set_conversion_skipped(skipped as i32);
                        }
                    }
                });
            };
        let result = import_lpl_with_error_handling(
            &temporary_lpl,
            &temp_output,
            &romx_core::ImportLplOptions {
                temporary_output: true,
                cover_target,
                ..Default::default()
            },
            &mut progress_callback,
            || cancel.load(Ordering::Relaxed),
            move |item_index, error| {
                request_error_choice(
                    &error_prompt_sender,
                    &error_weak,
                    &error_cancel,
                    item_index,
                    error,
                )
            },
        );
        let result = if cancel.load(Ordering::Relaxed) {
            Err(romx_core::RomxError::Cancelled)
        } else {
            result
        };
        let final_result = result.and_then(|report| {
            let Some(generated_path) = report.output_files.first() else {
                let _ = fs::remove_file(&report.manifest_path);
                return Err(romx_core::RomxError::Invalid(
                    "Core did not generate a ROMX file".into(),
                ));
            };
            let generated_name = final_output_name(generated_path).ok_or_else(|| {
                romx_core::RomxError::Invalid("Core produced an invalid output filename".into())
            })?;
            if Path::new(&generated_name).extension() != output_path.extension() {
                let _ = fs::remove_file(generated_path);
                let _ = fs::remove_file(&report.manifest_path);
                return Err(romx_core::RomxError::Invalid(
                    "Core output format does not match the target file".into(),
                ));
            }
            commit_staged(generated_path, &output_path, replace)
                .map_err(romx_core::RomxError::Invalid)?;
            let _ = fs::remove_file(&report.manifest_path);
            Ok((report, output_path))
        });
        remove_single_temp(&temporary_lpl, temporary_payload.as_deref());
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.set_conversion_running(false);
                window.set_conflict_visible(false);
                window.set_error_visible(false);
                match final_result {
                    Ok((report, output_path)) => {
                        window.set_conversion_current(report.total_items as i32);
                        window.set_conversion_imported(report.imported as i32);
                        window.set_conversion_skipped(report.skipped as i32);
                        set_status(
                            &window,
                            format!("Conversion complete: {}", output_path.display()),
                            format!("Conversion complete: {}", output_path.display()),
                        );
                    }
                    Err(romx_core::RomxError::Cancelled) => {
                        set_status(&window, "conversion_cancelled", "Conversion cancelled");
                    }
                    Err(error) => set_status(
                        &window,
                        format!("ROMX conversion failed: {error}"),
                        format!("ROMX conversion failed: {error}"),
                    ),
                }
            }
        });
    });
}

fn convert_single(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let rom_path = window.get_rom_path();
    let save_path = window.get_save_path();
    if rom_path.trim().is_empty() || save_path.trim().is_empty() {
        set_status(
            window,
            "choose_game_and_save_path_first",
            "Choose a game file and output folder first",
        );
        return;
    }
    let (_format, source_path, temporary_payload) = match prepare_single_input(window, workspace) {
        Ok(value) => value,
        Err(error) => {
            set_status(window, &error, &error);
            return;
        }
    };
    let cover_target = match resolution(window) {
        Ok(target) => target,
        Err(error) => {
            if let Some(payload_path) = temporary_payload.as_deref() {
                let _ = fs::remove_file(payload_path);
            }
            set_status(window, &error, &error);
            return;
        }
    };
    let temporary_lpl = match create_single_lpl(window, workspace, &source_path) {
        Ok(path) => path,
        Err(error) => {
            if let Some(payload_path) = temporary_payload.as_deref() {
                let _ = fs::remove_file(payload_path);
            }
            set_status(window, &error, &error);
            return;
        }
    };
    let save_path = PathBuf::from(save_path.trim());
    let output_path = save_path.join(format!("{}.romx", path_stem(&rom_path)));
    if output_path.exists() {
        workspace.borrow_mut().single_conflict = Some(PendingSingleConflict {
            lpl_path: temporary_lpl,
            payload_path: temporary_payload,
            cover_target,
            save_path,
            output_path: output_path.clone(),
            replace: false,
        });
        window.set_conflict_path(output_path.to_string_lossy().into_owned().into());
        window.set_conflict_visible(true);
        return;
    }
    run_single_conversion(
        window,
        workspace,
        SingleConversion {
            lpl_path: temporary_lpl,
            payload_path: temporary_payload,
            cover_target,
            save_path,
            output_path,
            replace: false,
        },
    );
}

fn handle_conflict_response(
    window: &MainWindow,
    workspace: &Rc<RefCell<LplWorkspace>>,
    response: PromptResponse,
) {
    if let Some(mut pending) = workspace.borrow_mut().single_conflict.take() {
        window.set_conflict_visible(false);
        match response {
            PromptResponse::Rename | PromptResponse::RenameAll => {
                pending.output_path = renamed_target(&pending.output_path);
                pending.replace = false;
                run_single_conversion(window, workspace, pending);
            }
            PromptResponse::Replace | PromptResponse::ReplaceAll => {
                pending.replace = true;
                run_single_conversion(window, workspace, pending);
            }
            PromptResponse::Skip | PromptResponse::SkipAll | PromptResponse::Stop => {
                remove_single_temp(&pending.lpl_path, pending.payload_path.as_deref());
                set_status(window, "output_skipped", "Output skipped");
            }
            _ => {}
        }
        return;
    }
    // Batch conversion waits on the worker thread for this response. Hide the
    // modal immediately; the worker will reopen it only if another item needs
    // a new decision.
    window.set_conflict_visible(false);
    if let Ok(mut slot) = workspace.borrow().prompt_sender.lock() {
        if let Some(sender) = slot.take() {
            let _ = sender.send(response);
        }
    }
}

fn convert_lpl_edit(window: &MainWindow, workspace: &Rc<RefCell<LplWorkspace>>) {
    let save_path = window.get_lpl_save_path();
    if save_path.trim().is_empty() {
        set_status(
            window,
            "choose_lpl_save_path_first",
            "Choose an LPL output folder first",
        );
        return;
    }
    let cover_target = match resolution(window) {
        Ok(target) => target,
        Err(error) => {
            set_status(window, &error, &error);
            return;
        }
    };
    let (_item_index, edit_path, _, cover_path) = match update_lpl_edit_file(window, workspace) {
        Ok(value) => value,
        Err(error) => {
            set_status(window, &error, &error);
            return;
        }
    };
    set_edit_cover(window, workspace, cover_path.as_deref());
    begin_preflight(
        window,
        workspace,
        edit_path,
        PathBuf::from(save_path.trim()),
        cover_target,
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let window = MainWindow::new()?;
    window.set_app_version(application_version().into());
    let lpl_workspace = Rc::new(RefCell::new(LplWorkspace::new()));
    {
        window.on_translate(|key, language_index| {
            locale_catalog().text(key.as_str(), language_index).into()
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_language_changed(move || {
            if let Some(window) = weak.upgrade() {
                window.set_status_text(
                    locale_catalog()
                        .text("ready", window.get_language_index())
                        .into(),
                );
                if window.get_active_page() == 0 && window.get_pack_subpage() == 1 {
                    refresh_lpl_preview(&window, &workspace);
                } else if window.get_active_page() == 1 {
                    refresh_unpack_preview(&window, &workspace);
                }
            }
        });
    }
    // Registering a callback does not automatically invalidate bindings that
    // were evaluated while MainWindow::new() was constructing the component.
    // Toggle once after registration so the initial language is rendered too.
    window.set_language_index(1);
    window.set_language_index(0);
    window.set_lpl_detail_title(
        localized_text(&window, "select_lpl_preview", "Select ROM / ROMX").into(),
    );
    window.set_lpl_detail_info(
        localized_text(
            &window,
            "select_lpl_preview_info",
            "Select an LPL and ROM folder to preview entries and covers",
        )
        .into(),
    );
    window.set_status_text(
        locale_catalog()
            .text("ready", window.get_language_index())
            .into(),
    );
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_open_rom_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_rom(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_open_cover_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_cover(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_metadata_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_metadata(&window);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_save_path_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_directory(&window);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_convert_clicked(move || {
            if let Some(window) = weak.upgrade() {
                if window.get_active_page() == 0 && window.get_pack_subpage() == 1 {
                    convert_lpl(&window, &workspace);
                } else if window.get_active_page() == 0 && window.get_editing_lpl_file() {
                    convert_lpl_edit(&window, &workspace);
                } else {
                    convert_single(&window, &workspace);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_preflight_scan_clicked(move || {
            if let Some(window) = weak.upgrade() {
                scan_pending_conversion(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_preflight_skip_clicked(move || {
            if let Some(window) = weak.upgrade() {
                start_pending_conversion(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_preflight_start_clicked(move || {
            if let Some(window) = weak.upgrade() {
                start_pending_conversion(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_preflight_cancel_clicked(move || {
            if let Some(window) = weak.upgrade() {
                workspace.borrow_mut().pending = None;
                window.set_preflight_visible(false);
                window.set_preflight_scanning(false);
                set_status(&window, "conversion_cancelled", "Conversion cancelled");
            }
        });
    }
    {
        let workspace = lpl_workspace.clone();
        window.on_cancel_conversion_clicked(move || {
            if let Some(flag) = workspace.borrow().cancel_flag.as_ref() {
                flag.store(true, Ordering::Relaxed);
            }
            if let Ok(mut slot) = workspace.borrow().prompt_sender.lock() {
                if let Some(sender) = slot.take() {
                    let _ = sender.send(PromptResponse::Stop);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_conflict_skip_clicked(move || {
            if let Some(window) = weak.upgrade() {
                handle_conflict_response(&window, &workspace, PromptResponse::Skip);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_conflict_skip_all_clicked(move || {
            if let Some(window) = weak.upgrade() {
                handle_conflict_response(&window, &workspace, PromptResponse::SkipAll);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_conflict_rename_clicked(move || {
            if let Some(window) = weak.upgrade() {
                handle_conflict_response(&window, &workspace, PromptResponse::Rename);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_conflict_rename_all_clicked(move || {
            if let Some(window) = weak.upgrade() {
                handle_conflict_response(&window, &workspace, PromptResponse::RenameAll);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_conflict_replace_clicked(move || {
            if let Some(window) = weak.upgrade() {
                handle_conflict_response(&window, &workspace, PromptResponse::Replace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_conflict_replace_all_clicked(move || {
            if let Some(window) = weak.upgrade() {
                handle_conflict_response(&window, &workspace, PromptResponse::ReplaceAll);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_error_continue_clicked(move || {
            if let Some(window) = weak.upgrade() {
                window.set_error_visible(false);
                if let Ok(mut slot) = workspace.borrow().prompt_sender.lock() {
                    if let Some(sender) = slot.take() {
                        let _ = sender.send(PromptResponse::Continue);
                    }
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_error_stop_clicked(move || {
            if let Some(window) = weak.upgrade() {
                window.set_error_visible(false);
                if let Ok(mut slot) = workspace.borrow().prompt_sender.lock() {
                    if let Some(sender) = slot.take() {
                        let _ = sender.send(PromptResponse::Stop);
                    }
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_open_lpl_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_lpl(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_open_lpl_rom_dir_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_lpl_directory(&window, LplDirectoryKind::Rom, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_open_lpl_image_dir_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_lpl_directory(&window, LplDirectoryKind::Image, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_open_lpl_save_path_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_lpl_directory(&window, LplDirectoryKind::Save, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_lpl_rom_file_clicked(move |index| {
            if let Some(window) = weak.upgrade() {
                select_lpl_rom(&window, &workspace, index);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_lpl_output_file_clicked(move |index| {
            if let Some(window) = weak.upgrade() {
                select_lpl_output(&window, &workspace, index);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_edit_lpl_file_clicked(move || {
            if let Some(window) = weak.upgrade() {
                begin_lpl_edit(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_save_lpl_edit_clicked(move || {
            if let Some(window) = weak.upgrade() {
                save_lpl_edit(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_return_lpl_edit_clicked(move || {
            if let Some(window) = weak.upgrade() {
                return_from_lpl_edit(&window, &workspace);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_open_unpack_file_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_unpack_source(&window, &workspace, false);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_open_unpack_folder_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_unpack_source(&window, &workspace, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_unpack_rom_dir_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_unpack_directory(&window, UnpackDirectoryKind::Rom);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_unpack_cover_dir_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_unpack_directory(&window, UnpackDirectoryKind::Cover);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_unpack_lpl_dir_clicked(move || {
            if let Some(window) = weak.upgrade() {
                choose_unpack_directory(&window, UnpackDirectoryKind::Lpl);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_unpack_romx_file_clicked(move |index| {
            if let Some(window) = weak.upgrade() {
                select_unpack_romx(&window, &workspace, index);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_unpack_file_convert_clicked(move || {
            if let Some(window) = weak.upgrade() {
                start_unpack_conversion(&window, &workspace, false);
            }
        });
    }
    {
        let weak = window.as_weak();
        let workspace = lpl_workspace.clone();
        window.on_unpack_batch_convert_clicked(move || {
            if let Some(window) = weak.upgrade() {
                start_unpack_conversion(&window, &workspace, true);
            }
        });
    }

    {
        let weak = window.as_weak();
        window.on_window_minimize_clicked(move || {
            if let Some(window) = weak.upgrade() {
                window.window().set_minimized(true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_window_maximize_clicked(move || {
            if let Some(window) = weak.upgrade() {
                let is_maximized = window.window().is_maximized();
                window.window().set_maximized(!is_maximized);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_window_close_clicked(move || {
            if let Some(window) = weak.upgrade() {
                let _ = window.window().hide();
                let _ = slint::quit_event_loop();
            }
        });
    }

    // Show first so the native window handle exists. On macOS the one-shot
    // callback runs on the first event-loop turn, after AppKit has installed
    // the NSView in its NSWindow.
    window.show()?;
    #[cfg(target_os = "macos")]
    {
        let weak = window.as_weak();
        // Keep applying the native setting for a short period because winit
        // may finish installing its titlebar after the first event-loop turn.
        retry_native_titlebar(weak, 40);
    }
    slint::run_event_loop()?;
    window.hide()?;
    Ok(())
}
