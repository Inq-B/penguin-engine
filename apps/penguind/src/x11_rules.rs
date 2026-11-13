//! Shared rule matching logic adapted from the Wayland daemon's rules.rs.
use wpe_ipc::{Condition, Limits, Rule, Signals, WindowInfo, WindowState};

/// What the enabled `rules` holding make of the wallpaper on `output`.
pub fn limits_for(rules: &[Rule], signals: &Signals, output: &str) -> Limits {
    let mut limits = Limits::default();
    for rule in rules.iter().filter(|r| r.enabled) {
        if let Some(why) = holds(rule, signals, output) {
            limits.add(rule, why);
        }
    }
    limits
}

/// Why `rule` holds for `output`, if it does.
fn holds(rule: &Rule, signals: &Signals, output: &str) -> Option<String> {
    match &rule.when {
        Condition::Window { state, app } => {
            let windows = signals.windows.as_deref()?;
            let here = |w: &WindowInfo| {
                w.output.as_deref() == Some(output) || (rule.all_outputs && w.output.is_some())
            };
            let w = windows.iter().find(|w| {
                let names = [Some(w.app.as_str()), w.process.as_deref()];
                let app_matches = app
                    .as_deref()
                    .is_none_or(|wanted| names.iter().flatten().any(|n| is_app(wanted, n)));
                app_matches
                    && match state {
                        WindowState::Open => true,
                        WindowState::Focused => w.focused && here(w),
                        WindowState::Maximized => (w.maximized || w.fullscreen) && here(w),
                        WindowState::Fullscreen => w.fullscreen && here(w),
                    }
            })?;
            let what = match state {
                WindowState::Open => "is running",
                WindowState::Focused => "is focused",
                WindowState::Maximized if w.fullscreen => "is fullscreen",
                WindowState::Maximized => "is maximized",
                WindowState::Fullscreen => "is fullscreen",
            };
            Some(format!("{} {what}", window_name(w)))
        }
        Condition::Audio { app } => {
            let playing = signals.audio.as_deref()?;
            let name = playing
                .iter()
                .find(|n| app.as_deref().is_none_or(|wanted| is_app(wanted, n)))?;
            Some(match name.is_empty() {
                true => "an application plays sound".to_owned(),
                false => format!("{name} plays sound"),
            })
        }
        Condition::Battery { below } => {
            let battery = signals.battery.filter(|b| b.discharging)?;
            match below {
                None => Some("on battery".to_owned()),
                Some(below) => battery
                    .percent
                    .filter(|p| p < below)
                    .map(|_| format!("battery below {below}%")),
            }
        }
        Condition::Idle { seconds } => (signals.idle? >= *seconds && *seconds > 0)
            .then(|| format!("no input for {}", duration(*seconds))),
    }
}

/// `name` (an app id, process or sound name) is the application `wanted`:
/// the same, case aside, or its last dotted part (`org.mozilla.firefox`
/// is `firefox`).
fn is_app(wanted: &str, name: &str) -> bool {
    let wanted = wanted.trim();
    let name = name.strip_suffix(".desktop").unwrap_or(name);
    !wanted.is_empty()
        && (name.eq_ignore_ascii_case(wanted)
            || name
                .rsplit('.')
                .next()
                .is_some_and(|last| last.eq_ignore_ascii_case(wanted)))
}

fn window_name(w: &WindowInfo) -> &str {
    [
        Some(w.app.as_str()),
        w.process.as_deref(),
        Some(w.title.as_str()),
    ]
    .into_iter()
    .flatten()
    .find(|n| !n.is_empty())
    .unwrap_or("a window")
}

fn duration(seconds: u32) -> String {
    match seconds {
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

/// The idle times the enabled rules wait for.
pub fn idle_times(rules: &[Rule]) -> Vec<u32> {
    let mut times: Vec<u32> = rules
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| match r.when {
            Condition::Idle { seconds } if seconds > 0 => Some(seconds),
            _ => None,
        })
        .collect();
    times.sort_unstable();
    times.dedup();
    times
}

