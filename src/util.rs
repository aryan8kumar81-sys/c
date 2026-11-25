//! Small helpers: time formatting, colours, persistence.
use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

pub static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static TZ: AtomicI32 = AtomicI32::new(0);

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ───────── timezone ─────────
pub fn init_tz() {
    TZ.store(detect_tz(), Ordering::Relaxed);
}

#[cfg(not(target_os = "android"))]
fn detect_tz() -> i32 {
    chrono::Local::now().offset().local_minus_utc()
}

#[cfg(target_os = "android")]
fn detect_tz() -> i32 {
    android_offset().unwrap_or(0)
}

#[cfg(any(target_os = "android", rootchat_jni_check))]
#[allow(dead_code)]
fn android_offset() -> Option<i32> {
    use jni::objects::JValue;
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let tz = env.call_static_method("java/util/TimeZone", "getDefault", "()Ljava/util/TimeZone;", &[]).ok()?.l().ok()?;
    let ms = env.call_method(&tz, "getOffset", "(J)I", &[JValue::Long(now_ms())]).ok()?.i().ok()?;
    Some(ms / 1000)
}

fn local(ts_ms: i64) -> DateTime<FixedOffset> {
    let off = FixedOffset::east_opt(TZ.load(Ordering::Relaxed)).unwrap_or(FixedOffset::east_opt(0).unwrap());
    DateTime::from_timestamp_millis(ts_ms).unwrap_or_default().with_timezone(&off)
}

pub fn fmt_time(ts: i64) -> String {
    let d = local(ts);
    format!("{:02}:{:02}", d.hour(), d.minute())
}

pub fn day_key(ts: i64) -> String {
    let d = local(ts);
    format!("{}-{}-{}", d.year(), d.month(), d.day())
}

fn days_ago(ts: i64) -> i64 {
    (local(now_ms()).date_naive() - local(ts).date_naive()).num_days()
}

pub fn day_label(ts: i64) -> String {
    match days_ago(ts) {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => {
            let d = local(ts);
            if d.year() == local(now_ms()).year() {
                d.format("%-d %B").to_string()
            } else {
                d.format("%-d %B %Y").to_string()
            }
        }
    }
}

pub fn list_time(ts: i64) -> String {
    match days_ago(ts) {
        0 => fmt_time(ts),
        1..=6 => local(ts).format("%a").to_string(),
        _ => local(ts).format("%-d %b").to_string(),
    }
}

pub fn last_seen(ts: i64) -> String {
    match days_ago(ts) {
        0 => format!("today at {}", fmt_time(ts)),
        1 => format!("yesterday at {}", fmt_time(ts)),
        _ => local(ts).format("%-d %b").to_string(),
    }
}

// ───────── text / colour ─────────
pub fn initials(name: &str) -> String {
    let mut it = name.split_whitespace().filter_map(|w| w.chars().next());
    let a = it.next();
    let b = it.next();
    let s: String = a.into_iter().chain(b).collect();
    if s.is_empty() { "?".into() } else { s.to_uppercase() }
}

pub fn sanitize_username(raw: &str) -> String {
    raw.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_').take(20).collect()
}

const COLORS: [&str; 8] = ["#3ee8a8", "#8b7cf6", "#f0b429", "#ff6b5e", "#5eb1ff", "#ff9f5e", "#c084fc", "#4ade80"];

pub fn color_for(s: &str) -> String {
    let mut h: i32 = 0;
    for c in s.chars() {
        h = (c as i32).wrapping_add(h.wrapping_shl(5).wrapping_sub(h));
    }
    COLORS[(h.unsigned_abs() as usize) % COLORS.len()].to_string()
}

pub fn parse_color(s: &str) -> slint::Color {
    let h = s.trim_start_matches('#');
    if h.len() == 6 {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8);
        }
    }
    slint::Color::from_rgb_u8(0x33, 0x90, 0xec)
}

// ───────── persistence ─────────
fn path(name: &str) -> PathBuf {
    let base = DATA_DIR.get().cloned().unwrap_or_else(|| {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
        home.join(".rootchat")
    });
    let _ = std::fs::create_dir_all(&base);
    base.join(name)
}

pub fn load_json(name: &str) -> Value {
    std::fs::read_to_string(path(name)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)
}

pub fn save_json(name: &str, v: &Value) {
    let _ = std::fs::write(path(name), v.to_string());
}

pub fn clear_file(name: &str) {
    let _ = std::fs::remove_file(path(name));
}
