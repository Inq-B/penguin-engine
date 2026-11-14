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
#[path = "../x11_sound_watch.rs"]
mod x11_sound_watch;

use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_long, c_ulong, c_void};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
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
    fn XShapeCombineMask(display: *mut c_void, window: c_ulong,
        kind: c_int, x: c_int, y: c_int, bitmap: c_ulong, op: c_int);
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
/// Preserve the configured desktop input policy. The backend defaults to
/// click-through only on first installation; a saved GUI preference must win
/// on subsequent launches.
fn x11_passthrough(
    saved: Option<bool>, configured: Option<bool>, interactive: bool, passthrough: bool
) -> bool {
    if passthrough { true }
    else if interactive { false }
    else { saved.or(configured).unwrap_or(true) }
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

/// One spanning surface covers the union of all XRandR monitor rectangles.
/// A single runtime/Servo page means animations and interactive content
/// remain continuous instead of being duplicated on each monitor.
fn desired_monitors(physical: &[Monitor], span: bool) -> Vec<Monitor> {
    if !span || physical.len() < 2 {
        return physical.to_vec();
    }
    let x = physical.iter().map(|m| m.x).min().unwrap_or(0);
    let y = physical.iter().map(|m| m.y).min().unwrap_or(0);
    let right = physical.iter().map(|m| i64::from(m.x) + i64::from(m.width)).max().unwrap_or(0);
    let bottom = physical.iter().map(|m| i64::from(m.y) + i64::from(m.height)).max().unwrap_or(0);
    vec![Monitor {
        name: "SPAN".to_owned(), x, y,
        width: (right - i64::from(x)).max(1) as u32,
        height: (bottom - i64::from(y)).max(1) as u32,
    }]
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
                // Passing None to XShapeCombineMask removes the custom input
                // region and restores the server's normal window-sized shape.
                // The old fixed 65535px rectangle was not the correct reset.
                XShapeCombineMask(self.0, win, 2, 0, 0, 0, 0);
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
    render_time: Duration,
    render_max: Duration,
    pressed: u32,
}
impl Screen {
    /// Keep the Xlib event path and the polled desktop fallback in sync, so
    /// a click is never delivered twice and release events aren't lost.
    fn button(&mut self, button: MouseButton, down: bool) {
        let bit=match button {
            MouseButton::Left => 1 << 8,     // X11 Button1Mask
            MouseButton::Middle => 1 << 9,   // X11 Button2Mask
            MouseButton::Right => 1 << 10,   // X11 Button3Mask
        };
        if (self.pressed & bit != 0)==down { return; }
        if down { self.pressed |= bit; } else { self.pressed &= !bit; }
        if let Some(wallpaper)=&mut self.wallpaper {
            wallpaper.pointer_button(button,down);
        }
    }
    fn buttons(&mut self, mask: u32) {
        for (button,bit) in [
            (MouseButton::Left,1 << 8),
            (MouseButton::Middle,1 << 9),
            (MouseButton::Right,1 << 10),
        ] {
            self.button(button,mask & bit != 0);
        }
    }
    fn render(&mut self, gpu: &Gpu, fps_cap: u32, stats: bool) {
        if self.paused || self.held.playback >= wpe_ipc::Playback::Pause { return; }
        // The X11 loop uses 60 Hz when the general cap is unspecified.
        // Preserve a stricter per-output rule if one is active.
        let requested = if fps_cap == 0 { 60 } else { fps_cap };
        let cap = self.held.fps.map_or(requested, |rule| rule.min(requested)).max(1);
        // The outer loop already schedules frames. Comparing against the
        // *end* of the previous frame (and requiring an exact full period)
        // caused 60 Hz scenes to skip every other frame under ordinary render
        // load. Keep a small scheduling tolerance for per-output rule caps.
        let period=Duration::from_secs_f64(1.0 / cap as f64);
        if self.last_render.elapsed().saturating_add(Duration::from_millis(2)) < period { return; }
        let Some(wallpaper) = self.wallpaper.as_mut() else { return; };
        let cpu_start=Instant::now();
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
        let spent=cpu_start.elapsed();
        self.render_time+=spent;
        self.render_max=self.render_max.max(spent);
        // Frame intervals are measured from their start, not present time.
        self.last_render = cpu_start;
        self.frames += 1;
        let elapsed = self.sampled.elapsed();
        if elapsed >= Duration::from_secs(3) {
            self.fps = self.frames as f32 / elapsed.as_secs_f32();
            if stats {
                tracing::info!("{}: {:.1} fps (cap {}), CPU render+present avg {:.2}ms, max {:.2}ms",
                    self.monitor.name,self.fps,cap,
                    self.render_time.as_secs_f64()*1000.0/(self.frames.max(1) as f64),
                    self.render_max.as_secs_f64()*1000.0);
            }
            self.render_time=Duration::ZERO;
            self.render_max=Duration::ZERO;
            self.frames=0;
            self.sampled=Instant::now();
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
    sound_rx: std::sync::mpsc::Receiver<Option<Vec<String>>>,
    peers: Vec<Peer>,
    subscribers: Vec<UnixStream>,
    quit: bool,
    state_dirty: bool,
    last_state_edit: Instant,
    hit_window: c_ulong,
    hit_is_desktop: bool,
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
    /// XQueryPointer(root) reports the top-level window actually under the
    /// cursor. Only relay buttons observed over desktop surfaces, never over
    /// another application, even when those buttons were grabbed by Nemo.
    fn desktop_hit(&mut self, child: c_ulong) -> bool {
        if child == 0 || self.screens.iter().any(|s| s.win == child) { return true; }
        if self.hit_window != child {
            self.hit_window = child;
            self.hit_is_desktop = x11_windows::is_desktop(self.display.0,child);
        }
        self.hit_is_desktop
    }

    /// Button state is sampled independently of the render loop. A quick
    /// mouse click can start and finish between two 60 Hz frames; checking
    /// at ~5 ms while interactive reduces those missed clicks. Click-through
    /// mode never incurs this extra X server round trip.
    fn poll_desktop_buttons(&mut self) {
        if self.settings.passthrough { return; }
        let root_window=unsafe {XRootWindow(self.display.0,self.display.1)};
        let (mut root,mut child,mut rx,mut ry,mut wx,mut wy,mut mask)=(0,0,0,0,0,0,0);
        if unsafe { XQueryPointer(self.display.0,root_window,
            &mut root,&mut child,&mut rx,&mut ry,&mut wx,&mut wy,&mut mask) } == 0 {
            return;
        }
        let desktop=self.desktop_hit(child);
        for screen in &mut self.screens {
            let inside=rx>=screen.monitor.x && ry>=screen.monitor.y
                && i64::from(rx)<i64::from(screen.monitor.x)+i64::from(screen.monitor.width)
                && i64::from(ry)<i64::from(screen.monitor.y)+i64::from(screen.monitor.height);
            let wanted=if desktop && inside { mask & ((1<<8)|(1<<9)|(1<<10)) } else {0};
            if screen.pressed != wanted {
                if let Some(wallpaper)=screen.wallpaper.as_mut() {
                    wallpaper.set_pointer(glam::Vec2::new(
                        ((rx-screen.monitor.x) as f32 / screen.monitor.width.max(1) as f32).clamp(0.0,1.0),
                        ((ry-screen.monitor.y) as f32 / screen.monitor.height.max(1) as f32).clamp(0.0,1.0),
                    ));
                }
                screen.buttons(wanted);
            }
        }
    }

    fn poll_input(&mut self) {
        unsafe {
            while XPending(self.display.0) > 0 {
                let mut raw = [0 as c_long; 24];
                XNextEvent(self.display.0, raw.as_mut_ptr().cast());
                let event = &*(raw.as_ptr().cast::<XButtonEvent>());
                if event.event_type != 4 && event.event_type != 5 {continue;}
                if self.settings.passthrough {continue;}
                let Some(screen) = self.screens.iter_mut().find(|s| s.win == event.window) else {continue;};
                if let Some(wallpaper)=screen.wallpaper.as_mut() {
                    wallpaper.set_pointer(glam::Vec2::new(
                        (event.x as f32 / screen.monitor.width.max(1) as f32).clamp(0.0,1.0),
                        (event.y as f32 / screen.monitor.height.max(1) as f32).clamp(0.0,1.0),
                    ));
                } else { continue; }
                let down = event.event_type == 4;
                match event.button {
                    1 => screen.button(MouseButton::Left, down),
                    2 => screen.button(MouseButton::Middle, down),
                    3 => screen.button(MouseButton::Right, down),
                    4 if down => if let Some(w)=&mut screen.wallpaper { w.scroll(0.0, -120.0); },
                    5 if down => if let Some(w)=&mut screen.wallpaper { w.scroll(0.0, 120.0); },
                    6 if down => if let Some(w)=&mut screen.wallpaper { w.scroll(-120.0, 0.0); },
                    7 if down => if let Some(w)=&mut screen.wallpaper { w.scroll(120.0, 0.0); },
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
        while let Ok(sound) = self.sound_rx.try_recv() { self.signals.audio = sound; }
        // X11 idle notification has not yet been wired up.
        self.apply_rules();
    }
    fn apply_rules(&mut self) {
        let mut changed = false;
        for s in &mut self.screens {
            let limits = x11_rules::limits_for(&self.settings.rules, &self.signals, &s.monitor.name);
            if s.held == limits { continue; }
            changed = true;
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
        // Don't flood the GUI with unchanged status every half-second.
        if changed { self.publish(); }
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
    /// Apply geometry changes without disturbing unchanged outputs.
    /// Physical assignments remain in state when enabling SPAN.
    fn reconcile_monitors(&mut self, desired: &[Monitor]) -> Result<()> {
        let mut changed=false;
        let fallback = self.screens.iter().find_map(|s| s.path.clone()
            .map(|path| (path, s.monitor.name.clone())));
        let mut index=0;
        while index<self.screens.len() {
            if desired.iter().any(|m| m.name == self.screens[index].monitor.name) {
                index+=1;
            } else {
                changed=true;
                let screen=self.screens.remove(index);
                let win=screen.win;
                drop(screen);
                unsafe { XDestroyWindow(self.display.0,win); }
            }
        }
        for monitor in desired {
            if let Some(screen)=self.screens.iter_mut().find(|s|s.monitor.name==monitor.name) {
                if screen.monitor!=*monitor {
                    changed=true;
                    screen.monitor=monitor.clone();
                    unsafe {XMoveResizeWindow(self.display.0,screen.win,monitor.x,monitor.y,monitor.width,monitor.height);}
                    screen.config.width=monitor.width.max(1);
                    screen.config.height=monitor.height.max(1);
                    screen.surface.configure(&self.gpu.device,&screen.config);
                }
                continue;
            }
            changed=true;
            let (screen,_)=create_screen(&self.display,&self.gpu.instance,Some(&self.gpu),
                monitor.clone(),self.settings.passthrough)?;
            self.screens.push(screen);
            let i=self.screens.len()-1;
            let saved=self.state.for_output(&monitor.name).cloned();
            let chosen=saved.or_else(|| {
                fallback.as_ref().map(|(path,_)| state::SavedOutput::new(path.clone()))
            });
            if let Some(saved)=chosen {
                self.screens[i].fill=saved.fill.unwrap_or(self.settings.fill);
                if let Err(e)=self.load(i,saved.wallpaper.clone(),saved.properties.clone()) {
                    tracing::warn!("{}: {e:#}",monitor.name);
                } else if monitor.name=="SPAN" {
                    self.state.set_wallpaper("SPAN",&saved.wallpaper,saved.properties);
                    self.state_dirty=true;
                }
            }
        }
        if changed {
            unsafe {XFlush(self.display.0);}
            self.publish();
        }
        Ok(())
    }
    fn load(&mut self, i: usize, path: PathBuf, properties: BTreeMap<String,serde_json::Value>) -> Result<()> {
        let item = preset::resolve(&path).map_err(anyhow::Error::msg)?;
        let name = self.screens[i].monitor.name.clone();
        let properties = self.state.properties_for(&name, &path, &properties);
        let values = item.properties(&properties);
        let format = self.screens[i].config.format;
        let mut wallpaper = Wallpaper::load_with_properties(&self.gpu, format, &self.assets, &item.load, &values)?;
        let s = &mut self.screens[i];
        wallpaper.fill = fill_mode(s.fill);
        wallpaper.set_audio(self.settings.audio && i == 0);
        wallpaper.set_volume(self.settings.volume as f32 / 100.0 * self.state.volume(&s.monitor.name, &path) as f32 / 100.0);
        wallpaper.set_particle_density(self.settings.particles as f32 / 100.0);
        wallpaper.apply_general_settings(&serde_json::json!({"fps": if self.settings.fps == 0 {60} else {self.settings.fps}}));
        s.buttons(0);
        s.wallpaper = Some(wallpaper);
        s.path = Some(path);
        s.item = Some(item);
        s.paused = false;
        s.held = wpe_ipc::Limits::default();
        Ok(())
    }
    fn settings_update(&mut self, settings: Settings) {
        let old_span=self.settings.span;
        if settings.passthrough {
            // Release any held buttons before removing the input region.
            for screen in &mut self.screens { screen.buttons(0); }
        }
        self.settings = settings;
        if old_span != self.settings.span {
            match monitors() {
                Ok(physical) => {
                    let desired=desired_monitors(&physical,self.settings.span);
                    if let Err(e)=self.reconcile_monitors(&desired) {
                        tracing::error!("changing span layout: {e:#}");
                    }
                }
                Err(e)=>tracing::warn!("discovering displays: {e:#}"),
            }
        }
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
                    self.state_dirty=true;
                    self.last_state_edit=Instant::now();
                    Ok(Response::ok())
                },
                Request::SetProperties{output,values} => {
                    for i in self.target(output.as_deref())? {
                        let s=&mut self.screens[i];
                        if let Some(w)=&mut s.wallpaper {
                            let changes:Vec<_>=values.iter()
                                .filter(|(name, value)| w.project().general.properties.contains_key(*name)
                                    && w.user_property(name) != Some(*value))
                                .map(|(name,value)|(name.clone(),value.clone())).collect();
                            if changes.is_empty() { continue; }
                            w.set_user_properties(&self.gpu,&changes);
                            for (name,value) in changes {
                                if let Some(p)=&s.path {
                                    self.state.set_property(&s.monitor.name,p,&name,value);
                                }
                            }
                            self.state_dirty=true;
                            self.last_state_edit=Instant::now();
                        }
                    }
                    Ok(Response::ok())
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
                let notify=matches!(&request,
                    Ok(Request::Set{..}|Request::Fill{..}|Request::Settings{..}
                    |Request::Pause{..}|Request::Resume{..}|Request::Audio{..}
                    |Request::Volume{..}|Request::Fps{..}|Request::WallpaperVolume{..}));
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
                if notify { self.publish(); }
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
    // FIFO prevents mailbox from rendering a fresh full-screen GPU frame
    // while one or more frames are still waiting in the compositor.
    // Interactive pages benefit from predictable frame pacing over
    // throughput of frames that will never be presented.
    config.present_mode=wgpu::PresentMode::Fifo;
    config.desired_maximum_frame_latency=2;
    surface.configure(&gpu.device,&config);
    Ok((Screen {
        monitor:m,win,surface,config,wallpaper:None,path:None,item:None,
        paused:false,held:wpe_ipc::Limits::default(),last_render:Instant::now()-Duration::from_secs(1),
        fill:Fill::Auto,frames:0,fps:0.0,sampled:Instant::now(),
        render_time:Duration::ZERO,render_max:Duration::ZERO,pressed:0,
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
    let configured=state::config_path().and_then(|p|state::load_config(&p).ok());
    let effective_passthrough=x11_passthrough(
        saved.settings.as_ref().map(|s|s.passthrough),
        configured.as_ref().map(|s|s.passthrough),
        args.interactive,args.passthrough);
    let mut settings=saved.settings.clone().or(configured).unwrap_or_default();
    settings.passthrough=effective_passthrough;
    if let Some(v)=args.fps {settings.fps=v;}
    if let Some(v)=args.volume {settings.volume=v;}
    if args.audio {settings.audio=true;}
    if let Some(v)=args.fill {settings.fill=v;}
    if let Some(wallpaper)=&args.wallpaper {
        let resolved=wpe_runtime::steam::resolve_wallpaper(wallpaper).map_err(anyhow::Error::msg)?;
        let mut entry=state::SavedOutput::new(resolved);
        entry.properties=args.property.iter().cloned().collect();
        saved.set_everywhere(entry);
        if let Some(p)=&path {saved.save(p)?;}
    }
    let display=Display::open()?;
    let outputs=desired_monitors(&monitors()?,settings.span);
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
    let (sound_tx,sound_rx)=std::sync::mpsc::channel();
    x11_sound_watch::watch(sound_tx);
    let mut app=App{display,gpu:gpu.context("no selected X11 output")?,screens,assets,
        state:saved,state_path:path,settings,signals:wpe_ipc::Signals::default(),sound_rx,
        peers:vec![],subscribers:vec![],quit:false,
        state_dirty:false,last_state_edit:Instant::now(),
        hit_window:0,hit_is_desktop:false};
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
    let mut last_buttons=Instant::now();
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
        // Coalesce rapid property changes into a single atomic state write.
        if app.state_dirty && app.last_state_edit.elapsed()>=Duration::from_millis(700) {
            app.save();
            app.state_dirty=false;
        }
        app.poll_input();
        if !app.settings.passthrough && last_buttons.elapsed()>=Duration::from_millis(5) {
            app.poll_desktop_buttons();
            last_buttons=Instant::now();
        }
        if last_signals.elapsed()>=Duration::from_millis(500) {
            app.poll_signals();
            last_signals=Instant::now();
        }
        if last_scan.elapsed()>=Duration::from_secs(2) {
            if let Ok(physical)=monitors() {
                let desired=desired_monitors(&physical,app.settings.span);
                if let Err(e)=app.reconcile_monitors(&desired) {
                    tracing::warn!("X11 monitor update: {e:#}");
                }
            }
            last_scan=Instant::now();
        }
        let now=Instant::now();
        if now>=next_frame {
            // Query the pointer ONCE from the root instead of once per
            // display, avoiding multiple synchronous X11 round trips for
            // multi-monitor systems.
            let (mut root,mut child,mut rx,mut ry,mut wx,mut wy,mut mask)=(0,0,0,0,0,0,0);
            let root_window=unsafe {XRootWindow(app.display.0,app.display.1)};
            let pointer_ok=unsafe {XQueryPointer(app.display.0,root_window,
                &mut root,&mut child,&mut rx,&mut ry,&mut wx,&mut wy,&mut mask)} != 0;
            // Cinnamon/Nemo often owns the top-level desktop window, even
            // while the wallpaper is visible and gets hover polling. X11
            // ButtonPress events then go to Nemo instead of our window.
            // When interactive mode is explicitly on, observe button-mask
            // transitions as a fallback, but ONLY when the topmost window is
            // a desktop surface (never over an application).
            let desktop=pointer_ok && app.desktop_hit(child);
            for s in &mut app.screens {
                let inside=pointer_ok
                    && rx>=s.monitor.x && ry>=s.monitor.y
                    && i64::from(rx)<i64::from(s.monitor.x)+i64::from(s.monitor.width)
                    && i64::from(ry)<i64::from(s.monitor.y)+i64::from(s.monitor.height);
                if inside {
                    if let Some(w)=&mut s.wallpaper {
                        let x=((rx-s.monitor.x) as f32/s.monitor.width.max(1) as f32).clamp(0.0,1.0);
                        let y=((ry-s.monitor.y) as f32/s.monitor.height.max(1) as f32).clamp(0.0,1.0);
                        w.set_pointer(glam::Vec2::new(x,y));
                    }
                }
                let buttons=if !app.settings.passthrough && desktop && inside { mask } else { 0 };
                s.buttons(buttons);
                s.render(&app.gpu,app.settings.fps,args.stats);
            }
            let cap=if app.settings.fps==0 {60}else{app.settings.fps.max(1)};
            let period=Duration::from_secs_f64(1.0/cap as f64);
            // Anchor to the scheduled deadline; don't gradually lose time
            // to the work needed to render a frame. Catch up without spinning
            // after a large frame-time spike.
            next_frame+=period;
            if next_frame<=Instant::now() { next_frame=Instant::now()+period; }
        }
        // An adaptive wait reduces idle CPU wakeups (~333/s before), while
        // keeping input and control commands responsive at up to 200 Hz.
        let until=next_frame.saturating_duration_since(Instant::now());
        std::thread::sleep(until.min(Duration::from_millis(5)));
    }
    if app.state_dirty { app.save(); }
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
    #[test] fn keeps_interactive_mode_across_restarts() {
        assert!(x11_passthrough(None,None,false,false)); // safe default
        assert!(!x11_passthrough(Some(false),None,false,false));
        assert!(!x11_passthrough(None,Some(false),false,false));
        assert!(!x11_passthrough(Some(true),None,true,false));
        assert!(x11_passthrough(Some(false),None,false,true));
        assert!(x11_passthrough(Some(false),None,true,true)); // passthrough wins
    }
    #[test] fn spanning_geometry_handles_offset_monitors() {
        let screens=vec![
            Monitor{name:"A".into(),x:-1920,y:0,width:1920,height:1080},
            Monitor{name:"B".into(),x:0,y:180,width:2560,height:1440},
        ];
        let span=desired_monitors(&screens,true);
        assert_eq!(span.len(),1);
        assert_eq!(span[0].name,"SPAN");
        assert_eq!((span[0].x,span[0].y,span[0].width,span[0].height),
            (-1920,0,4480,1620));
        assert_eq!(desired_monitors(&screens,false),screens);
    }
    #[test] fn parse_xrandr_geometry() {
        assert_eq!(geometry("1920x1080+0+0"),Some((1920,1080,0,0)));
        assert_eq!(geometry("2560x1440-1920+0"),Some((2560,1440,-1920,0)));
        assert_eq!(geometry("primary"),None);
    }
}
