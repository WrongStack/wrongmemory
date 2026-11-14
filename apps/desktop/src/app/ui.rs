//! Shared presentation primitives: semantic tones, chips, empty states, field
//! rows, banners and platform-correct shortcut labels. Views compose these so
//! spacing, colour semantics and typography stay consistent on every OS.
use gpui_kit::assets::IconName;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::*;
use gpui_kit::*;
use serde_json::Value;

// Icons outside GPUI Kit's default embedded set. Without this source the
// bundled assets return nothing and the icon renders as an empty gap.
gpui_kit::assets::icon_assets!(
    pub ExtraIcons,
    [
        Activity,
        Anchor,
        CircleDot,
        Command,
        Database,
        FileClock,
        GitBranch,
        Key,
        KeyRound,
        Monitor,
        Pencil,
        PlugZap,
        Scale,
        ShieldCheck,
        Tags,
        Wrench
    ]
);

/// Application asset source: extra icons first, then GPUI Kit's own assets.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// Platform modifier for application shortcuts: Command on macOS, Control
/// elsewhere. Bindings and their visible labels use the same source.
pub const MAC: bool = cfg!(target_os = "macos");

pub fn binding(key: &'static str) -> &'static str {
    match (MAC, key) {
        (true, "r") => "cmd-r",
        (true, "f") => "cmd-f",
        (true, "k") => "cmd-k",
        (true, "p") => "cmd-p",
        (false, "r") => "ctrl-r",
        (false, "f") => "ctrl-f",
        (false, "k") => "ctrl-k",
        (false, "p") => "ctrl-p",
        _ => key,
    }
}

pub fn kbd(key: &'static str) -> Kbd {
    Kbd::new(
        Keystroke::parse(binding(key))
            .unwrap_or_else(|_| Keystroke::parse("escape").expect("static keystroke parses")),
    )
}

/// Linux desktops commonly resolve `.SystemUIFont` to a variable font (for
/// example Ubuntu's), from which GPUI cannot select a bold instance, so every
/// heading renders at regular weight. Prefer families that ship static bold
/// faces; Windows and macOS keep their system UI font.
const LINUX_UI_FONTS: &[&str] = &["Noto Sans", "Liberation Sans", "DejaVu Sans"];

pub fn preferred_ui_font(installed: &[String]) -> Option<&'static str> {
    if !cfg!(any(target_os = "linux", target_os = "freebsd")) {
        return None;
    }
    LINUX_UI_FONTS
        .iter()
        .copied()
        .find(|family| installed.iter().any(|name| name == family))
}

/// Re-apply the platform UI font. Call after every theme mode change, because
/// loading a theme configuration resets the family.
pub fn apply_platform_font(cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    if let Some(family) = preferred_ui_font(&installed)
        && cx.theme().font_family.as_ref() != family
    {
        Theme::update(cx, |theme| theme.font_family = family.into());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Info,
    Success,
    Warning,
    Danger,
}

impl Tone {
    pub fn for_status(status: &str) -> Self {
        match status {
            "active" => Self::Success,
            "stale" => Self::Warning,
            "deleted" | "contradicted" => Self::Danger,
            "superseded" => Self::Info,
            _ => Self::Neutral,
        }
    }

    pub fn for_severity(severity: &str) -> Self {
        match severity {
            "ok" | "pass" => Self::Success,
            "warn" | "warning" => Self::Warning,
            "error" | "fail" | "critical" => Self::Danger,
            "info" => Self::Info,
            _ => Self::Neutral,
        }
    }

    pub fn for_job(state: &str) -> Self {
        match state {
            "succeeded" | "completed" => Self::Success,
            "running" | "queued" | "pending" => Self::Info,
            "failed" | "cancelled" => Self::Danger,
            _ => Self::Neutral,
        }
    }

    pub fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Self::Neutral => theme.muted_foreground,
            Self::Info => theme.info,
            Self::Success => theme.success,
            Self::Warning => theme.warning,
            Self::Danger => theme.danger,
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Self::Neutral | Self::Info => IconName::Info,
            Self::Success => IconName::CircleCheck,
            Self::Warning => IconName::TriangleAlert,
            Self::Danger => IconName::CircleX,
        }
    }

    fn tag(self) -> Tag {
        match self {
            Self::Neutral => Tag::secondary(),
            Self::Info => Tag::info(),
            Self::Success => Tag::success(),
            Self::Warning => Tag::warning(),
            Self::Danger => Tag::danger(),
        }
    }
}

/// Small semantic chip. Text always carries the meaning; colour reinforces it.
pub fn chip(label: impl Into<SharedString>, tone: Tone) -> Tag {
    tone.tag().outline().small().child(label.into())
}

pub fn section(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(label.into())
}

pub fn caption(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(label.into())
}

/// Centered empty/placeholder state whose text always wraps inside its panel.
pub fn empty_state(
    icon: IconName,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    cx: &App,
) -> AnyElement {
    div()
        .size_full()
        .flex_1()
        .min_w_0()
        .p_8()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(px(360.))
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .size_12()
                        .rounded_full()
                        .bg(cx.theme().muted)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(icon)
                                .size_5()
                                .text_color(cx.theme().muted_foreground),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .text_center()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.into()),
                )
                .child(
                    div()
                        .w_full()
                        .text_center()
                        .text_sm()
                        .line_height(relative(1.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(description.into()),
                ),
        )
        .into_any_element()
}

/// Label/value row for structured evidence. Long values wrap.
pub fn field(label: impl Into<SharedString>, value: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .gap_3()
        .text_sm()
        .child(
            div()
                .w(px(112.))
                .flex_none()
                .text_color(cx.theme().muted_foreground)
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(value.into()))
}

/// Tinted inline banner for errors, disconnection and conflicts.
pub fn banner(
    tone: Tone,
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let color = tone.color(cx);
    div()
        .mx_5()
        .mt_4()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(color.opacity(0.45))
        .bg(color.opacity(0.08))
        .flex()
        .gap_3()
        .items_start()
        .child(
            div()
                .pt(px(2.))
                .child(Icon::new(tone.icon()).size_4().text_color(color)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.into()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(body.into()),
                ),
        )
}

/// Human labels for JSON keys shown in evidence fields.
pub fn humanize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    for (ix, ch) in key.chars().enumerate() {
        if ch == '_' {
            out.push(' ');
        } else if ch.is_uppercase() && ix > 0 {
            out.push(' ');
            out.extend(ch.to_lowercase());
        } else if ix == 0 {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Flatten a JSON object into readable rows. Nested values are rendered
/// compactly; nothing is dropped.
pub fn rows(value: &Value) -> Vec<(String, String)> {
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (humanize_key(k), scalar(v)))
            .collect(),
        Value::Null => Vec::new(),
        other => vec![("Value".into(), scalar(other))],
    }
}

pub fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "Yes".into(),
        Value::Bool(false) => "No".into(),
        Value::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => {
            items.iter().map(scalar).collect::<Vec<_>>().join(", ")
        }
        Value::Null => "—".into(),
        other => other.to_string(),
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

pub fn duration(ms: i64) -> String {
    let s = ms.unsigned_abs() / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m", s / 60),
        3_600..=86_399 => {
            let (h, m) = (s / 3_600, (s % 3_600) / 60);
            if m == 0 {
                format!("{h}h")
            } else {
                format!("{h}h {m}m")
            }
        }
        _ => format!("{}d", s / 86_400),
    }
}

pub fn relative_time(at_ms: i64, now: i64) -> String {
    if at_ms <= 0 {
        return "—".into();
    }
    let delta = at_ms - now;
    if delta.abs() < 1_000 {
        "now".into()
    } else if delta > 0 {
        format!("in {}", duration(delta))
    } else {
        format!("{} ago", duration(delta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The GPUI glob import carries its own `test` attribute; use the standard one.
    use core::prelude::v1::test;
    use serde_json::json;

    #[test]
    fn readable_rows_keep_every_non_null_field() {
        let rows = rows(&json!({"type":"user","sessionId":"s1","agent":null,"files":["a","b"]}));
        assert_eq!(
            rows,
            vec![
                ("Type".to_string(), "user".to_string()),
                ("Session id".to_string(), "s1".to_string()),
                ("Files".to_string(), "a, b".to_string()),
            ]
        );
    }

    #[test]
    fn relative_times_are_compact() {
        assert_eq!(relative_time(10_000 + 3_600_000 * 6, 10_000), "in 6h");
        assert_eq!(relative_time(10_000, 10_000 + 90_000), "1m ago");
        assert_eq!(relative_time(0, 5), "—");
        assert_eq!(duration(21_600_000), "6h");
        assert_eq!(duration(5_400_000), "1h 30m");
    }

    /// Every icon referenced by the views must resolve through the app asset
    /// source; a missing SVG renders as an empty gap, not an error.
    #[test]
    fn every_referenced_icon_is_embedded() {
        let sources = [
            include_str!("../app.rs"),
            include_str!("graph_view.rs"),
            include_str!("ui.rs"),
        ];
        let mut names = std::collections::BTreeSet::new();
        for source in sources {
            for part in source.split("IconName::").skip(1) {
                let name: String = part
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && name != "ALL" {
                    names.insert(name);
                }
            }
        }
        assert!(names.len() > 20, "icon scan found too little: {names:?}");
        let mut missing = Vec::new();
        for name in &names {
            let icon = IconName::ALL
                .iter()
                .copied()
                .find(|icon| format!("{icon:?}") == *name)
                .unwrap_or_else(|| panic!("unknown icon {name}"));
            if AppAssets.load(&icon.path()).ok().flatten().is_none() {
                missing.push(name.clone());
            }
        }
        assert!(
            missing.is_empty(),
            "icons without embedded SVG: {missing:?}"
        );
    }

    #[test]
    fn linux_prefers_a_static_bold_family() {
        let installed = vec!["Ubuntu".to_string(), "DejaVu Sans".to_string()];
        let expected =
            cfg!(any(target_os = "linux", target_os = "freebsd")).then_some("DejaVu Sans");
        assert_eq!(preferred_ui_font(&installed), expected);
        assert_eq!(preferred_ui_font(&["Ubuntu".to_string()]), None);
    }

    #[test]
    fn shortcuts_follow_the_platform_modifier() {
        let expected = if MAC { "cmd-k" } else { "ctrl-k" };
        assert_eq!(binding("k"), expected);
        assert!(Keystroke::parse(binding("r")).is_ok());
    }
}
