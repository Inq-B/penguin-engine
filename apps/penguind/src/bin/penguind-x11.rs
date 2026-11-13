//! Native X11 desktop backend. Shares the renderer, IPC types, presets and saved state
//! with the Wayland daemon. A separate binary keeps Wayland's event loop unchanged.
//! X11 requires Xlib, Xext (Shape), and xrandr on PATH.
#![allow(clippy::too_many_arguments)]

#[path = "../state.rs"]
mod state;
#[path = "../preset.rs"]
mod preset;
#[path = "../power.rs"]
mod power;
#[path = "../x11_windows.rs"]
mod x11_windows;
#[path = "../x11_rules.rs"]
mod x11_rules;

use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_ulong, c_void};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, XlibDisplayHandle, XlibWindowHandle,
};
use wpe_ipc::{Fill, Request, Response, Settings, Status, OutputStatus, WallpaperInfo};
use wpe_render::{Gpu, wgpu};
use wpe_runtime::{Wallpaper, FillMode, MouseButton};

#[link(name = "X11")]
unsafe extern "C" {
    fn XInitThreads() -> c_int;
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultScreen(display: *mut c_void) -> c_int;
    fn XRootWindow(display: *mut c_void, screen: c_int) -> c_ulong;
    fn XCreateSimpleWindow(display: *mut c_void, parent: c_ulong, x: c_int, y: c_int,
        width: u32, height: u32, border: u32, border_pixel: c_ulong, background: c_ulong) -> c_ulong;
    fn XDestroyWindow(display: *mut c_void, window: c_ulong) -> c_int;
    fn XMapWindow(display: *mut c_void, window: c_ulong) -> c_int;
    fn XMoveResizeWindow(display: *mut c_void, window: c_ulong, x: c_int, y: c_int,
        width: u32, height: u32) -> c_int;
    fn XLowerWindow(display: *mut c_void, window: c_ulong) -> c_int;
    fn XFlush(display: *mut c_void) -> c_int;
    fn XInternAtom(display: *mut c_void, name: *const c_char, only_if_exists: c_int) -> c_ulong;
    fn XChangeProperty(display: *mut c_void, window: c_ulong, property: c_ulong,
        typ: c_ulong, format: c_int, mode: c_int, data: *const u8, nelements: c_int) -> c_int;
    fn XSelectInput(display: *mut c_void, window: c_ulong, mask: c_long) -> c_int;
    fn XPending(display: *mut c_void) -> c_int;
    fn XNextEvent(display: *mut c_void, event: *mut c_void) -> c_int;
    fn XQueryPointer(display: *mut c_void, window: c_ulong, root: *mut c_ulong,
        child: *mut c_ulong, root_x: *mut c_int, root_y: *mut c_int,
        win_x: *mut c_int, win_y: *mut c_int, mask: *mut u32) -> c_int;
}
#[link(name = "Xext")]
unsafe extern "C" {
    fn XShapeCombineRectangles(display: *mut c_void, window: c_ulong,
        kind: c_int, x: c_int, y: c_int, rectangles: *const c_void,
        n_rectangles: c_int, op: c_int, ordering: c_int);
}

#[derive(Parser)]
#[command(about = "Play Wallpaper Engine wallpapers on an X11 desktop")]
struct Args {
    wallpaper: Option<String>,
    #[arg(long)] assets: Option<PathBuf>,
    #[arg(long)] output: Option<String>,
    #[arg(long)] fps: Option<u32>,
    #[arg(long)] volume: Option<u32>,
    #[arg(long)] audio: bool,
    #[arg(long)] fill: Option<Fill>,
    #[arg(long)] passthrough: bool,
    /// Allow wallpaper input (X11 defaults to click-through for desktop icons).
    #[arg(long)] interactive: bool,
    /// Accepted for compatibility; X11 uses EWMH desktop windows.
    #[arg(long)] layer: Option<wpe_ipc::Layer>,
    #[arg(long)] socket: Option<PathBuf>,
    #[arg(long)] stats: bool,
    #[arg(long = "property", value_parser = parse_property)]
    property: Vec<(String, serde_json::Value)>,
}
fn parse_property(s: &str) -> std::result::Result<(String, serde_json::Value), String> {
    let (k, v) = s.split_once('=').ok_or("expected NAME=VALUE")?;
    Ok((k.to_owned(), serde_json::from_str(v).unwrap_or_else(|_| v.into())))
}
fn fill_mode(fill: Fill) -> FillMode {
    match fill {
        Fill::Auto => FillMode::Auto, Fill::Crop => FillMode::Crop,
        Fill::Fit => FillMode::Fit, Fill::Stretch => FillMode::Stretch,
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Monitor { name: String, x: i32, y: i32, width: u32, height: u32 }

fn geometry(s: &str) -> Option<(u32,u32,i32,i32)> {
    // xrandr --query: 1920x1080+0+0, including negative origins.
    let (w, rest) = s.split_once('x')?;
    let w: u32 = w.parse().ok()?;
    let n = rest.find(['+', '-'])?;
    let h: u32 = rest[..n].parse().ok()?;
    let rest = &rest[n..];
    let m = rest[1..].find(['+', '-'])? + 1;
    let x: i32 = rest[..m].parse().ok()?;
    let y: i32 = rest[m..].parse().ok()?;
    Some((w, h, x, y))
}
fn monitors() -> Result<Vec<Monitor>> {
    let result = std::process::Command::new("xrandr").arg("--query").output()
        .context("xrandr is required for X11 output discovery")?;
    if !result.status.success() { bail!("xrandr failed: {}", String::from_utf8_lossy(&result.stderr)); }
    let mut result_monitors = Vec::new();
    for line in String::from_utf8_lossy(&result.stdout).lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        if words.len() < 3 || words[1] != "connected" { continue; }
        if let Some((width,height,x,y)) = words.iter().find_map(|word| geometry(word)) {
            result_monitors.push(Monitor { name: words[0].to_owned(), x,y,width,height });
        }
    }
    Ok(result_monitors)
}
// Xlib's common button event prefix. The XEvent union is at least 24 longs.
#[repr(C)]
struct XButtonEvent {
    event_type: c_int, serial: c_ulong, send_event: c_int,
    display: *mut c_void, window: c_ulong, root: c_ulong, subwindow: c_ulong,
    time: c_ulong, x: c_int, y: c_int, x_root: c_int, y_root: c_int,
    state: u32, button: u32, same_screen: c_int,
}
struct Display(*mut c_void, c_int);
impl Display {
    fn open() -> Result<Self> {
        // SAFETY: all calls are on the daemon's main thread; XInitThreads precedes wgpu.
        unsafe {
            XInitThreads();
            let p = XOpenDisplay(std::ptr::null());
            if p.is_null() { bail!("cannot connect to X11 ($DISPLAY)"); }
            Ok(Self(p, XDefaultScreen(p)))
        }
    }
    fn atom(&self, s: &'static [u8]) -> c_ulong {
        unsafe { XInternAtom(self.0, s.as_ptr().cast(), 0) }
    }
    fn window(&self, m: &Monitor, passthrough: bool) -> Result<c_ulong> {
        unsafe {
            let root = XRootWindow(self.0, self.1);
            let win = XCreateSimpleWindow(self.0, root, m.x, m.y, m.width, m.height, 0, 0, 0);
            if win == 0 { bail!("XCreateSimpleWindow failed"); }
            // EWMH type DESKTOP ensures the WM keeps this outside the task switcher,
            // dock and normal stacking; BELOW is a second hint for WMs which support it.
            let atom = self.atom(b"ATOM\0");
            let kind = self.atom(b"_NET_WM_WINDOW_TYPE\0");
            let desktop = self.atom(b"_NET_WM_WINDOW_TYPE_DESKTOP\0");
            XChangeProperty(self.0, win, kind, atom, 32, 0, (&desktop as *const c_ulong).cast(), 1);
            let state = self.atom(b"_NET_WM_STATE\0");
            let below = self.atom(b"_NET_WM_STATE_BELOW\0");
            let skip = self.atom(b"_NET_WM_STATE_SKIP_TASKBAR\0");
            let values = [below, skip];
            XChangeProperty(self.0, win, state, atom, 32, 0, values.as_ptr().cast(), 2);
            XSelectInput(self.0, win, (1 << 2) | (1 << 3) | (1 << 6));
            self.passthrough(win, passthrough);
            XMapWindow(self.0, win);
            XLowerWindow(self.0, win);
            XFlush(self.0);
            Ok(win)
        }
    }
    fn passthrough(&self, win: c_ulong, yes: bool) {
        unsafe {
            // ShapeInput = 2; ShapeSet = 0. None means default window input shape,
            // empty rectangles mean no input. Preserve desktop icon click handling.
            if yes {
                XShapeCombineRectangles(self.0, win, 2, 0, 0, std::ptr::null(), 0, 0, 0);
            } else {
                // Reset to default input region by copying the bounding shape.
                // ShapeBounding = 0; ShapeInput = 2; ShapeSet = 0.
                // For portability, interactive mode uses an explicit rectangle.
                let rect: [u16; 4] = [0,0, u16::MAX,u16::MAX];
                XShapeCombineRectangles(self.0, win, 2, 0, 0, rect.as_ptr().cast(), 1, 0, 0);
            }
            XFlush(self.0);
        }
    }
}
impl Drop for Display {
    fn drop(&mut self) { unsafe { XCloseDisplay(self.0); } }
}

struct Screen {
    monitor: Monitor,
    win: c_ulong,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    wallpaper: Option<Wallpaper>,
    path: Option<PathBuf>,
    item: Option<preset::Item>,
    paused: bool,
    held: wpe_ipc::Limits,
    last_render: Instant,
    fill: Fill,
    frames: u32,
    fps: f32,
    sampled: Instant,
}
impl Screen {
    fn render(&mut self, gpu: &Gpu, fps_cap: u32, stats: bool) {
        if self.paused || self.held.playback >= wpe_ipc::Playback::Pause { return; }
        let cap = self.held.fps.map_or(fps_cap, |rule| if fps_cap == 0 { rule } else { rule.min(fps_cap) }).max(1);
        if self.last_render.elapsed() < Duration::from_secs_f64(1.0 / cap as f64) { return; }
        let Some(wallpaper) = self.wallpaper.as_mut() else { return; };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) |
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&gpu.device, &self.config); return;
            },
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        wallpaper.set_scale(1.0);
        wallpaper.render(gpu, &mut encoder, &view, (self.config.width,self.config.height));
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
        self.last_render = Instant::now();
        self.frames += 1;
        let elapsed = self.sampled.elapsed();
        if elapsed >= Duration::from_secs(3) {
            self.fps = self.frames as f32 / elapsed.as_secs_f32();
            self.frames = 0;
            self.sampled = Instant::now();
            if stats { tracing::info!("{}: {:.1} fps (cap {})", self.monitor.name, self.fps, fps_cap); }
        }
    }
}
struct Peer { socket: UnixStream, buffer: Vec<u8> }
struct App {
    display: Display,
    gpu: Gpu,
    screens: Vec<Screen>,
    assets: PathBuf,
    state: state::State,
    state_path: Option<PathBuf>,
    settings: Settings,
    signals: wpe_ipc::Signals,
    peers: Vec<Peer>,
    subscribers: Vec<UnixStream>,
    quit: bool,
}
impl App {
    fn save(&self) {
        if let Some(p) = &self.state_path {
            if let Err(e) = self.state.save(p) { tracing::warn!("saving state: {e:#}"); }
        }
    }
    fn wallpaper_info(&self, s: &Screen) -> Option<WallpaperInfo> {
        let w = s.wallpaper.as_ref()?;
        let path = s.path.as_ref()?;
        let preset = s.item.as_ref().and_then(|i| i.preset.as_ref());
        Some(WallpaperInfo {
            path: path.display().to_string(),
            workshop_id: preset.and_then(|p| p.workshop_id.clone())
                .or_else(|| w.project().workshopid.clone()),
            title: preset.and_then(|p| p.title.clone()).unwrap_or_else(|| w.title.clone()),
            kind: w.project().kind.clone().unwrap_or_else(|| if w.is_video() {"video"} else {"scene"}.into()),
            dependency: preset.map(|p| p.dependency.display().to_string()),
            sound: w.has_sound(), volume: self.state.volume(&s.monitor.name, path),
        })
    }
    fn status(&self) -> Status {
        Status { settings: self.settings.clone(), outputs: self.screens.iter().map(|s| OutputStatus {
            name: s.monitor.name.clone(), width: s.monitor.width, height: s.monitor.height,
            scale: 1.0, wallpaper: self.wallpaper_info(s), paused: s.paused, fps: s.fps,
            fill: s.fill, limits: s.held.clone(), ..Default::default()
        }).collect() }
    }
    fn poll_input(&mut self) {
        unsafe {
            while XPending(self.display.0) > 0 {
                let mut raw = [0 as c_long; 24];
                XNextEvent(self.display.0, raw.as_mut_ptr().cast());
                let event = &*(raw.as_ptr().cast::<XButtonEvent>());
                if event.event_type != 4 && event.event_type != 5 {continue;}
                let Some(screen) = self.screens.iter_mut().find(|s| s.win == event.window) else {continue;};
                let Some(wallpaper) = screen.wallpaper.as_mut() else {continue;};
                let down = event.event_type == 4;
                match event.button {
                    1 => wallpaper.pointer_button(MouseButton::Left, down),
                    2 => wallpaper.pointer_button(MouseButton::Middle, down),
                    3 => wallpaper.pointer_button(MouseButton::Right, down),
                    4 if down => wallpaper.scroll(0.0, -120.0),
                    5 if down => wallpaper.scroll(0.0, 120.0),
                    6 if down => wallpaper.scroll(-120.0, 0.0),
                    7 if down => wallpaper.scroll(120.0, 0.0),
                    _ => {}
                }
            }
        }
    }
    fn poll_signals(&mut self) {
        let root = unsafe { XRootWindow(self.display.0, self.display.1) };
        let rects = self.screens.iter().map(|s| (
            s.monitor.name.clone(),
            (s.monitor.x, s.monitor.y, s.monitor.width, s.monitor.height)
        )).collect::<Vec<_>>();
        self.signals.windows = Some(x11_windows::list(self.display.0, root, &rects));
        self.signals.battery = power::battery();
        // Idle/audio sources have not yet been wired up on this backend.
        self.apply_rules();
    }
    fn apply_rules(&mut self) {
        for s in &mut self.screens {
            let limits = x11_rules::limits_for(&self.settings.rules, &self.signals, &s.monitor.name);
            if s.held == limits { continue; }
            tracing::info!("{}: X11 rules {:?} ({})", s.monitor.name, limits.playback, limits.because.join(", "));
            s.held = limits;
            if let Some(w) = &mut s.wallpaper {
                w.set_paused(s.paused || s.held.playback >= wpe_ipc::Playback::Pause);
                let volume = s.path.as_ref().map_or(100, |p| self.state.volume(&s.monitor.name, p));
                let muted = s.held.playback >= wpe_ipc::Playback::Mute;
                w.set_volume(if muted {0.0} else {self.settings.volume as f32 / 100.0 * volume as f32 / 100.0});
                let particles = s.held.particles.unwrap_or(100).min(100);
                w.set_particle_density((self.settings.particles.min(100) as f32 / 100.0) * particles as f32 / 100.0);
            }
        }
        self.publish();
    }
    fn publish(&mut self) {
        let json = serde_json::to_vec(&wpe_ipc::Event::Status { status: self.status() }).unwrap_or_default();
        self.subscribers.retain_mut(|s| s.write_all(&json).and_then(|_| s.write_all(b"\n")).is_ok());
    }
    fn target(&self, wanted: Option<&str>) -> std::result::Result<Vec<usize>,String> {
        match wanted {
            Some(name) => self.screens.iter().position(|s| s.monitor.name == name)
                .map(|i|vec![i]).ok_or_else(|| format!("unknown output {name}")),
            None => Ok((0..self.screens.len()).collect()),
        }
    }
    fn load(&mut self, i: usize, path: PathBuf, properties: BTreeMap<String,serde_json::Value>) -> Result<()> {
        let item = preset::resolve(&path).map_err(anyhow::Error::msg)?;
        let values = item.properties(&properties);
        let format = self.screens[i].config.format;
        let mut wallpaper = Wallpaper::load_with_properties(&self.gpu, format, &self.assets, &item.load, &values)?;
        let s = &mut self.screens[i];
        wallpaper.fill = fill_mode(s.fill);
        wallpaper.set_audio(self.settings.audio && i == 0);
        wallpaper.set_volume(self.settings.volume as f32 / 100.0 * self.state.volume(&s.monitor.name, &path) as f32 / 100.0);
        wallpaper.set_particle_density(self.settings.particles as f32 / 100.0);
        wallpaper.apply_general_settings(&serde_json::json!({"fps": if self.settings.fps == 0 {60} else {self.settings.fps}}));
        s.wallpaper = Some(wallpaper);
        s.path = Some(path);
        s.item = Some(item);
        s.paused = false;
        s.held = wpe_ipc::Limits::default();
        Ok(())
    }
    fn settings_update(&mut self, settings: Settings) {
        self.settings = settings;
        for (i,s) in self.screens.iter_mut().enumerate() {
            self.display.passthrough(s.win, self.settings.passthrough);
            if let Some(w) = s.wallpaper.as_mut() {
                w.fill = fill_mode(s.fill);
                w.set_audio(self.settings.audio && i == 0);
                w.set_volume(self.settings.volume as f32 / 100.0
                    * s.path.as_ref().map_or(100, |p|self.state.volume(&s.monitor.name,p)) as f32 / 100.0);
                w.set_particle_density(self.settings.particles as f32 / 100.0);
                w.apply_general_settings(&serde_json::json!({"fps": if self.settings.fps == 0 {60} else {self.settings.fps}}));
            }
        }
        self.state.settings = Some(self.settings.clone());
        self.save();
        self.apply_rules();
    }
    fn handle(&mut self, request: Request) -> Response {
        let answer = (|| -> std::result::Result<Response,String> {
            match request {
                Request::Status => Ok(Response {status: Some(self.status()),..Response::ok()}),
                Request::Version => Ok(Response {version: Some(wpe_ipc::VersionInfo {
                    protocol: wpe_ipc::PROTOCOL, daemon: env!("CARGO_PKG_VERSION").into(),
                    features: vec!["x11".into()],
                }),..Response::ok()}),
                Request::Set {output,wallpaper,properties} => {
                    let indices = self.target(output.as_deref())?;
                    let path = wpe_runtime::steam::resolve_wallpaper(&wallpaper).map_err(|e|e.to_string())?;
                    for &i in &indices { self.load(i,path.clone(),properties.iter().map(|(k,v)|(k.clone(),v.clone())).collect()).map_err(|e|e.to_string())?; }
                    for &i in &indices { self.state.set_wallpaper(&self.screens[i].monitor.name,&path,properties.iter().map(|(k,v)|(k.clone(),v.clone())).collect()); }
                    self.save(); Ok(Response::ok())
                },
                Request::Pause{output} | Request::Resume{output} => {
                    // Explicit state is handled in the outer match below.
                    let _ = output;
                    unreachable!("pause/resume dispatched separately")
                },
                Request::Quit => { self.quit = true; Ok(Response::ok()) },
                Request::Fps{fps} => {
                    if fps > 1000 { return Err("fps must be <= 1000".into()); }
                    let mut s = self.settings.clone(); s.fps=fps; self.settings_update(s); Ok(Response::ok())
                },
                Request::Audio{enabled} => {let mut s=self.settings.clone();s.audio=enabled;self.settings_update(s);Ok(Response::ok())},
                Request::Volume{volume} => {
                    if volume>100 {return Err("volume must be <= 100".into())}
                    let mut s=self.settings.clone();s.volume=volume;self.settings_update(s);Ok(Response::ok())
                },
                Request::Fill{output,fill} => {
                    for i in self.target(output.as_deref())? {
                        let s=&mut self.screens[i];s.fill=fill;
                        if let Some(w)=&mut s.wallpaper {w.fill=fill_mode(fill);}
                        if let Some(path)=&s.path {self.state.set_fill(&s.monitor.name,path,Some(fill));}
                    }
                    self.save(); Ok(Response::ok())
                },
                Request::Settings{patch} => {
                    let s=self.settings.patched(&patch);
                    if s.volume>100 || s.particles>100 || s.fps>1000 {return Err("invalid settings range".into())}
                    self.settings_update(s);Ok(Response::ok())
                },
                Request::ReloadSettings => {
                    let s=state::config_path().and_then(|p|state::load_config(&p).ok()).unwrap_or_default();
                    self.settings_update(s);Ok(Response::ok())
                },
                Request::WallpaperVolume{output,volume} => {
                    if volume>100 {return Err("volume must be <= 100".into())}
                    for i in self.target(output.as_deref())? {
                        let s=&self.screens[i];
                        if let Some(p)=&s.path {self.state.set_volume(&s.monitor.name,p,volume);}
                    }
                    self.save(); self.settings_update(self.settings.clone());Ok(Response::ok())
                },
                Request::Property{output,name,value} => {
                    for i in self.target(output.as_deref())? {
                        let s=&mut self.screens[i];
                        if let Some(w)=&mut s.wallpaper {
                            if w.project().general.properties.contains_key(&name) {
                                w.set_user_property(&self.gpu,&name,value.clone());
                                if let Some(p)=&s.path {self.state.set_property(&s.monitor.name,p,&name,value.clone());}
                            }
                        }
                    }
                    self.save();Ok(Response::ok())
                },
                Request::SetProperties{output,values} => {
                    for i in self.target(output.as_deref())? {
                        let s=&mut self.screens[i];
                        if let Some(w)=&mut s.wallpaper {
                            for (name,value) in &values {
                                if w.project().general.properties.contains_key(name) {
                                    w.set_user_property(&self.gpu,name,value.clone());
                                    if let Some(p)=&s.path {self.state.set_property(&s.monitor.name,p,name,value.clone());}
                                }
                            }
                        }
                    }
                    self.save();Ok(Response::ok())
                },
                Request::ResetProperties{output} => {
                    for i in self.target(output.as_deref())? {
                        if let Some(path)=self.screens[i].path.clone() {
                            let name=self.screens[i].monitor.name.clone();
                            self.state.clear_properties(&name,&path);
                            self.load(i,path,BTreeMap::new()).map_err(|e|e.to_string())?;
                        }
                    }
                    self.save();Ok(Response::ok())
                },
                Request::Properties{output} => {
                    let mut properties=vec![];
                    for i in self.target(output.as_deref())? {
                        let s=&self.screens[i];
                        let (Some(w),Some(path))=(&s.wallpaper,&s.path) else {continue;};
                        let item=s.item.as_ref();
                        properties.push(wpe_ipc::OutputProperties {
                            output:s.monitor.name.clone(), wallpaper:path.display().to_string(),
                            properties: w.project().general.properties.iter().map(|(name,p)|wpe_ipc::PropertyInfo{
                                name:name.clone(),kind:p.kind.clone().unwrap_or_default(),text:p.text.clone(),
                                value:w.user_property(name).unwrap_or(&p.value).clone(),
                                default:item.and_then(|i|i.defaults.get(name)).unwrap_or(&p.value).clone(),
                                options:p.options.iter().map(|o|wpe_ipc::PropertyChoice{label:o.label.clone(),value:o.value.clone()}).collect(),
                                min:p.min,max:p.max,step:p.step,order:p.order,condition:p.condition.clone(),
                            }).collect(),
                        });
                    }
                    Ok(Response{properties:Some(properties),..Response::ok()})
                },
                Request::Signals => Ok(Response {signals:Some(self.signals.clone()),..Response::ok()}),
                Request::Subscribe => Err("subscribe dispatched by socket layer".into()),
            }
        })();
        Response::from_result(answer)
    }
    fn dispatch(&mut self, request: Request) -> Response {
        match request {
            Request::Pause{output} => {
                let Ok(indices)=self.target(output.as_deref()) else {return Response::error("unknown output")};
                for i in indices {let s=&mut self.screens[i];s.paused=true;if let Some(w)=&mut s.wallpaper {w.set_paused(true);}}
                Response::ok()
            },
            Request::Resume{output} => {
                let Ok(indices)=self.target(output.as_deref()) else {return Response::error("unknown output")};
                for i in indices {let s=&mut self.screens[i];s.paused=false;if let Some(w)=&mut s.wallpaper {w.set_paused(false);}}
                Response::ok()
            },
            other => self.handle(other),
        }
    }
    fn poll_peers(&mut self) {
        let mut remaining=Vec::new();
        for mut peer in std::mem::take(&mut self.peers) {
            let mut tmp=[0u8;8192];
            let mut alive=true;
            loop {
                match peer.socket.read(&mut tmp) {
                    Ok(0)=>{alive=false;break;},
                    Ok(n)=>peer.buffer.extend_from_slice(&tmp[..n]),
                    Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>break,
                    Err(_)=>{alive=false;break;}
                }
            }
            while let Some(end)=peer.buffer.iter().position(|b|*b==b'\n') {
                let line:Vec<_>=peer.buffer.drain(..=end).collect();
                let request=serde_json::from_slice::<Request>(&line);
                let result=match request {
                    Ok(Request::Subscribe)=>{
                        let ok=serde_json::to_vec(&Response::ok()).unwrap();
                        let _=peer.socket.write_all(&[ok.as_slice(),b"\n"].concat());
                        let first=serde_json::to_vec(&wpe_ipc::Event::Status{status:self.status()}).unwrap();
                        let _=peer.socket.write_all(&[first.as_slice(),b"\n"].concat());
                        if let Ok(sub)=peer.socket.try_clone() {self.subscribers.push(sub);}
                        alive=false;break;
                    },
                    Ok(request)=>self.dispatch(request),
                    Err(e)=>Response::error(format!("invalid request: {e}")),
                };
                let json=serde_json::to_vec(&result).unwrap();
                if peer.socket.write_all(&[json.as_slice(),b"\n"].concat()).is_err(){alive=false;break;}
                self.publish();
            }
            if peer.buffer.len()>1024*1024 {alive=false;}
            if alive {remaining.push(peer);}
        }
        self.peers=remaining;
    }
}
fn create_screen(display:&Display,instance:&wgpu::Instance,gpu:Option<&Gpu>,m:Monitor,passthrough:bool) -> Result<(Screen,Option<Gpu>)> {
    let win=display.window(&m,passthrough)?;
    let raw_window_handle=RawWindowHandle::Xlib(XlibWindowHandle::new(win));
    let raw_display_handle=RawDisplayHandle::Xlib(XlibDisplayHandle::new(
        NonNull::new(display.0),display.1));
    // SAFETY: the display and window outlive the wgpu surface; the screen is
    // dropped before XDestroyWindow, and Display is dropped last.
    let surface=unsafe {instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle:Some(raw_display_handle),raw_window_handle,
    })?};
    let new_gpu=if gpu.is_none(){Some(pollster::block_on(Gpu::new(instance.clone(),Some(&surface)))?)}else{None};
    let gpu=gpu.or(new_gpu.as_ref()).context("gpu not initialized")?;
    let caps=surface.get_capabilities(&gpu.adapter);
    let mut config=surface.get_default_config(&gpu.adapter,m.width.max(1),m.height.max(1))
        .context("X11 surface incompatible with the GPU")?;
    if let Some(format)=caps.formats.iter().find(|f|!f.is_srgb()) {config.format=*format;}
    if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque){config.alpha_mode=wgpu::CompositeAlphaMode::Opaque;}
    config.present_mode=if caps.present_modes.contains(&wgpu::PresentMode::Mailbox){
        wgpu::PresentMode::Mailbox
    }else{wgpu::PresentMode::Fifo};
    surface.configure(&gpu.device,&config);
    Ok((Screen {
        monitor:m,win,surface,config,wallpaper:None,path:None,item:None,
        paused:false,held:wpe_ipc::Limits::default(),last_render:Instant::now()-Duration::from_secs(1),
        fill:Fill::Auto,frames:0,fps:0.0,sampled:Instant::now(),
    },new_gpu))
}
fn main()->Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_|"info,wgpu_core=warn,wgpu_hal=warn".into())
    ).init();
    let args=Args::parse();
    let assets=args.assets.clone().or_else(wpe_runtime::steam::default_assets_dir)
        .context("Wallpaper Engine assets not found; pass --assets")?;
    let path=state::state_path();
    let mut saved=path.as_deref().map(state::State::load).unwrap_or_default();
    let config=state::config_path().and_then(|p|state::load_config(&p).ok()).unwrap_or_default();
    let mut settings=saved.settings.clone().unwrap_or(config);
    if let Some(v)=args.fps {settings.fps=v;}
    if let Some(v)=args.volume {settings.volume=v;}
    if args.audio {settings.audio=true;}
    if let Some(v)=args.fill {settings.fill=v;}
    // X11 desktop backgrounds must leave desktop icons and context menus clickable.
    // --interactive deliberately opts out (e.g. for mouse-reactive wallpapers).
    settings.passthrough = !args.interactive;
    if args.passthrough { settings.passthrough=true; }
    if let Some(wallpaper)=&args.wallpaper {
        let resolved=wpe_runtime::steam::resolve_wallpaper(wallpaper).map_err(anyhow::Error::msg)?;
        let mut entry=state::SavedOutput::new(resolved);
        entry.properties=args.property.iter().cloned().collect();
        saved.set_everywhere(entry);
        if let Some(p)=&path {saved.save(p)?;}
    }
    let display=Display::open()?;
    let outputs=monitors()?;
    if outputs.is_empty(){bail!("no active X11 outputs reported by xrandr");}
    let instance=wgpu::Instance::default();
    let mut gpu:Option<Gpu>=None;
    let mut screens=Vec::new();
    for m in outputs {
        if args.output.as_ref().is_some_and(|name|name!=&m.name){continue;}
        let (screen,new_gpu)=create_screen(&display,&instance,gpu.as_ref(),m,settings.passthrough)?;
        if let Some(g)=new_gpu{gpu=Some(g);}
        screens.push(screen);
    }
    let mut app=App{display,gpu:gpu.context("no selected X11 output")?,screens,assets,
        state:saved,state_path:path,settings,signals:wpe_ipc::Signals::default(),
        peers:vec![],subscribers:vec![],quit:false};
    for i in 0..app.screens.len() {
        let name=app.screens[i].monitor.name.clone();
        if let Some(saved)=app.state.for_output(&name).cloned() {
            app.screens[i].fill=saved.fill.unwrap_or(app.settings.fill);
            if let Err(e)=app.load(i,saved.wallpaper,saved.properties){tracing::error!("{name}: {e:#}");}
        }
    }
    app.poll_signals();
    let socket_path=args.socket.or_else(wpe_ipc::socket_path)
        .context("no socket path; set XDG_RUNTIME_DIR or --socket")?;
    if socket_path.exists() {
        if UnixStream::connect(&socket_path).is_ok(){bail!("another penguin-engine daemon already owns {}",socket_path.display());}
        std::fs::remove_file(&socket_path)?;
    }
    let listener=UnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;
    let mut last_scan=Instant::now();
    let mut last_signals=Instant::now();
    let mut next_frame=Instant::now();
    tracing::info!("X11 backend active on {} outputs; control socket {}",app.screens.len(),socket_path.display());
    while !app.quit {
        loop {
            match listener.accept() {
                Ok((socket,_)) => {
                    socket.set_nonblocking(true)?;
                    app.peers.push(Peer{socket,buffer:vec![]});
                },
                Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>break,
                Err(e)=>return Err(e.into()),
            }
        }
        app.poll_peers();
        app.poll_input();
        if last_signals.elapsed()>=Duration::from_millis(500) {
            app.poll_signals();
            last_signals=Instant::now();
        }
        if last_scan.elapsed()>=Duration::from_secs(2) {
            if let Ok(found)=monitors() {
                for m in &found {
                    if let Some(screen)=app.screens.iter_mut().find(|s|s.monitor.name==m.name) {
                        if &screen.monitor!=m {
                            screen.monitor=m.clone();
                            unsafe {XMoveResizeWindow(app.display.0,screen.win,m.x,m.y,m.width,m.height);}
                            screen.config.width=m.width.max(1);
                            screen.config.height=m.height.max(1);
                            screen.surface.configure(&app.gpu.device,&screen.config);
                        }
                    } else if args.output.as_ref().is_none_or(|name|name==&m.name) {
                        let (screen,_)=create_screen(&app.display,&app.gpu.instance,Some(&app.gpu),m.clone(),app.settings.passthrough)?;
                        app.screens.push(screen);
                        let i=app.screens.len()-1;
                        if let Some(saved)=app.state.for_output(&m.name).cloned() {
                            app.screens[i].fill=saved.fill.unwrap_or(app.settings.fill);
                            if let Err(e)=app.load(i,saved.wallpaper,saved.properties){tracing::warn!("{e:#}");}
                        }
                        app.publish();
                    }
                }
                let mut i=0;
                while i<app.screens.len() {
                    if !found.iter().any(|m|m.name==app.screens[i].monitor.name) {
                        let screen=app.screens.remove(i);
                        let win=screen.win;drop(screen);
                        unsafe {XDestroyWindow(app.display.0,win);}
                        app.publish();
                    } else {i+=1;}
                }
                unsafe {XFlush(app.display.0);}
            }
            last_scan=Instant::now();
        }
        let now=Instant::now();
        if now>=next_frame {
            // Input is sampled through XQueryPointer so wallpaper cursor parallax
            // works even on a click-through desktop.
            for s in &mut app.screens {
                let (mut root,mut child,mut rx,mut ry,mut wx,mut wy,mut mask)=(0,0,0,0,0,0,0);
                unsafe {XQueryPointer(app.display.0,s.win,&mut root,&mut child,&mut rx,&mut ry,&mut wx,&mut wy,&mut mask);}
                if let Some(w)=&mut s.wallpaper {
                    w.set_pointer(glam::Vec2::new(wx as f32/s.monitor.width.max(1) as f32,
                        wy as f32/s.monitor.height.max(1) as f32));
                }
                s.render(&app.gpu,app.settings.fps,args.stats);
            }
            let cap=if app.settings.fps==0 {60}else{app.settings.fps.max(1)};
            next_frame=now+Duration::from_secs_f64(1.0/cap as f64);
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    app.subscribers.clear();
    app.peers.clear();
    app.screens.clear();
    drop(listener);
    let _=std::fs::remove_file(&socket_path);
    wpe_runtime::shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn parse_xrandr_geometry() {
        assert_eq!(geometry("1920x1080+0+0"),Some((1920,1080,0,0)));
        assert_eq!(geometry("2560x1440-1920+0"),Some((2560,1440,-1920,0)));
        assert_eq!(geometry("primary"),None);
    }
}
