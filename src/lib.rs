//! RootChat 2 - native Rust messenger. UI: Slint. Backend: Firebase REST.
slint::include_modules!();

mod fb;
mod util;

use fb::{Client, Session};
use serde_json::{json, Value};
use slint::{Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use util::*;

// ───────────────────────── state ─────────────────────────
#[derive(Clone, Debug)]
struct Msg {
    id: String,
    sender: String,
    text: String,
    created: i64,
    seen: bool,
    deleted: bool,
    edited: bool,
    pending: bool,
}

#[derive(Default)]
struct State {
    gen: u64,
    sess: Option<Session>,
    profile: Value,
    users: HashMap<String, Value>,
    presence: HashMap<String, Value>,
    metas: HashMap<String, Value>,
    idx: HashMap<String, String>,
    active: Option<(String, String)>,
    msgs: Vec<Msg>,
    pending: Vec<Msg>,
    msg_etag: Option<(String, String)>,
    usernames: Option<(Instant, Value)>,
    search_gen: u64,
    last_typing: Option<Instant>,
}

#[derive(Clone)]
struct Ctx {
    ui: slint::Weak<AppWindow>,
    st: Arc<Mutex<State>>,
    cl: Client,
}

thread_local! {
    static CHATS: Rc<VecModel<ChatItem>> = Rc::new(VecModel::default());
    static MSGS: Rc<VecModel<MsgItem>> = Rc::new(VecModel::default());
    static RESULTS: Rc<VecModel<UserItem>> = Rc::new(VecModel::default());
    static KEEP: RefCell<Vec<slint::Timer>> = const { RefCell::new(Vec::new()) };
}

fn sync<T: Clone + PartialEq + 'static>(m: &VecModel<T>, new: Vec<T>) {
    let n = m.row_count();
    for (i, item) in new.iter().enumerate().take(n) {
        if m.row_data(i).as_ref() != Some(item) {
            m.set_row_data(i, item.clone());
        }
    }
    while m.row_count() > new.len() {
        m.remove(m.row_count() - 1);
    }
    for item in new.iter().skip(n) {
        m.push(item.clone());
    }
}

impl Ctx {
    fn ui_do(&self, f: impl FnOnce(AppWindow) + Send + 'static) {
        let _ = self.ui.upgrade_in_event_loop(f);
    }

    fn auth(&self) -> Option<(String, String)> {
        let s = self.st.lock().unwrap();
        s.sess.as_ref().map(|x| (x.uid.clone(), x.id_token.clone()))
    }

    fn gen(&self) -> u64 {
        self.st.lock().unwrap().gen
    }

    fn toast(&self, msg: &str) {
        let m = msg.to_string();
        let m2 = m.clone();
        self.ui_do(move |ui| ui.set_toast(m.into()));
        let ctx = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2600));
            ctx.ui_do(move |ui| {
                if ui.get_toast() == m2.as_str() {
                    ui.set_toast("".into());
                }
            });
        });
    }

    fn user(&self, uid: &str) -> Value {
        self.st.lock().unwrap().users.get(uid).cloned().unwrap_or(Value::Null)
    }

    fn fetch_user(&self, uid: &str) -> Value {
        if let Some(u) = self.st.lock().unwrap().users.get(uid) {
            return u.clone();
        }
        let Some((_, tok)) = self.auth() else { return Value::Null };
        let v = self.cl.get_val(&tok, &format!("users/{uid}")).unwrap_or(Value::Null);
        if !v.is_null() {
            self.st.lock().unwrap().users.insert(uid.to_string(), v.clone());
        }
        v
    }
}

// ───────────────────────── session / boot ─────────────────────────
fn boot(ctx: Ctx) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let prefs = load_json("prefs.json");
        let dark = prefs["dark"].as_bool().unwrap_or(true);
        ctx.ui_do(move |ui| ui.set_dark(dark));

        let saved = load_json("session.json");
        let mut sess = None;
        if let (Some(rt), Some(em)) = (saved["refresh"].as_str(), saved["email"].as_str()) {
            sess = ctx.cl.refresh(rt, em).ok();
        }
        let left = Duration::from_millis(1500).saturating_sub(started.elapsed());
        std::thread::sleep(left);
        match sess {
            Some(s) => establish(&ctx, s),
            None => ctx.ui_do(|ui| ui.set_screen(1)),
        }
    });
}

fn establish(ctx: &Ctx, sess: Session) {
    save_json("session.json", &json!({"refresh": sess.refresh_token, "email": sess.email}));
    let uid = sess.uid.clone();
    let tok = sess.id_token.clone();
    let gen = {
        let mut s = ctx.st.lock().unwrap();
        s.gen += 1;
        s.sess = Some(sess.clone());
        s.gen
    };
    let mut profile = ctx.cl.get_val(&tok, &format!("users/{uid}")).unwrap_or(Value::Null);
    if profile.is_null() {
        let base = sanitize_username(sess.email.split('@').next().unwrap_or("user"));
        let base = if base.is_empty() { format!("user{}", fastrand::u32(1000..9999)) } else { base };
        profile = json!({"uid": uid, "username": base, "displayName": base, "bio": "", "color": color_for(&uid), "createdAt": now_ms(), "email": sess.email});
        let _ = ctx.cl.put(&tok, &format!("users/{uid}"), &profile);
        let _ = ctx.cl.put(&tok, &format!("usernames/{base}"), &json!(uid));
    }
    {
        let mut s = ctx.st.lock().unwrap();
        s.profile = profile.clone();
        s.users.insert(uid.clone(), profile.clone());
    }
    apply_profile(ctx, &profile);
    ctx.ui_do(|ui| {
        ui.set_busy(false);
        ui.set_auth_error("".into());
        ui.set_loading_chats(true);
        ui.set_screen(2);
    });
    heartbeat(ctx, true);
    let c = ctx.clone();
    std::thread::spawn(move || poller(c, gen));
}

fn apply_profile(ctx: &Ctx, p: &Value) {
    let name = p["displayName"].as_str().unwrap_or("").to_string();
    let uname = p["username"].as_str().unwrap_or("").to_string();
    let bio = p["bio"].as_str().unwrap_or("").to_string();
    let tint = parse_color(p["color"].as_str().unwrap_or(""));
    ctx.ui_do(move |ui| {
        ui.set_my_initials(initials(&name).into());
        ui.set_my_name(name.into());
        ui.set_my_username(uname.into());
        ui.set_my_bio(bio.into());
        ui.set_my_tint(tint);
    });
}

fn heartbeat(ctx: &Ctx, online: bool) {
    if let Some((uid, tok)) = ctx.auth() {
        let _ = ctx.cl.put(&tok, &format!("presence/{uid}"), &json!({"online": online, "lastSeen": {".sv": "timestamp"}}));
    }
}

fn ensure_token(ctx: &Ctx) {
    let sess = ctx.st.lock().unwrap().sess.clone();
    if let Some(s) = sess {
        if s.near_expiry() {
            if let Ok(n) = ctx.cl.refresh(&s.refresh_token, &s.email) {
                save_json("session.json", &json!({"refresh": n.refresh_token, "email": n.email}));
                ctx.st.lock().unwrap().sess = Some(n);
            }
        }
    }
}

fn poller(ctx: Ctx, gen: u64) {
    let mut tick: u64 = 0;
    loop {
        std::thread::sleep(Duration::from_millis(650));
        if ctx.gen() != gen {
            return;
        }
        tick += 1;
        ensure_token(&ctx);
        let active = ctx.st.lock().unwrap().active.is_some();
        if active {
            poll_messages(&ctx, false);
            if tick % 3 == 0 {
                poll_meta(&ctx);
            }
        }
        if tick % 5 == 1 || (tick < 3) {
            poll_chats(&ctx);
        }
        if tick % 38 == 0 {
            heartbeat(&ctx, true);
        }
    }
}

// ───────────────────────── chat list ─────────────────────────
fn poll_chats(ctx: &Ctx) {
    let Some((uid, tok)) = ctx.auth() else { return };
    let idx = match ctx.cl.get_val(&tok, &format!("userChats/{uid}")) {
        Ok(v) => {
            ctx.ui_do(|ui| ui.set_online(true));
            v
        }
        Err(_) => {
            ctx.ui_do(|ui| ui.set_online(false));
            return;
        }
    };
    let pairs: Vec<(String, String)> = idx
        .as_object()
        .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default();

    let fetched: Vec<(String, String, Value, Value, Value)> = std::thread::scope(|s| {
        let hs: Vec<_> = pairs
            .iter()
            .map(|(cid, other)| {
                let tok = tok.clone();
                let ctx = ctx.clone();
                s.spawn(move || {
                    let meta = ctx.cl.get_val(&tok, &format!("chats/{cid}/meta")).unwrap_or(Value::Null);
                    let user = ctx.fetch_user(other);
                    let pres = ctx.cl.get_val(&tok, &format!("presence/{other}")).unwrap_or(Value::Null);
                    (cid.clone(), other.clone(), meta, user, pres)
                })
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let now = now_ms();
    let mut rows: Vec<(i64, ChatItem)> = Vec::new();
    {
        let mut st = ctx.st.lock().unwrap();
        st.idx = pairs.iter().cloned().collect();
        for (cid, other, meta, user, pres) in fetched {
            if meta.is_null() {
                continue;
            }
            st.metas.insert(cid.clone(), meta.clone());
            st.presence.insert(other.clone(), pres.clone());
            let name = user["displayName"].as_str().unwrap_or("User").to_string();
            let last_at = meta["lastMessageAt"].as_i64().unwrap_or(meta["createdAt"].as_i64().unwrap_or(0));
            let mine_last = meta["lastMessageSender"].as_str() == Some(uid.as_str());
            let unread = meta["unread"][uid.as_str()].as_i64().unwrap_or(0) as i32;
            let other_unread = meta["unread"][other.as_str()].as_i64().unwrap_or(0);
            let typing = meta["typing"][other.as_str()].as_i64().map(|t| now - t < 4000).unwrap_or(false);
            rows.push((
                last_at,
                ChatItem {
                    id: cid.into(),
                    uid: other.clone().into(),
                    initials: initials(&name).into(),
                    name: name.into(),
                    tint: parse_color(user["color"].as_str().unwrap_or("")),
                    last: meta["lastMessageText"].as_str().unwrap_or("").replace('\n', " ").into(),
                    time: if last_at > 0 { list_time(last_at) } else { String::new() }.into(),
                    unread,
                    online: pres["online"].as_bool().unwrap_or(false),
                    mine_last,
                    seen_last: mine_last && other_unread == 0,
                    typing,
                },
            ));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let items: Vec<ChatItem> = rows.into_iter().map(|r| r.1).collect();
    ctx.ui_do(move |ui| {
        CHATS.with(|m| sync(m, items));
        ui.set_loading_chats(false);
    });
}

// ───────────────────────── active chat ─────────────────────────
fn open_chat(ctx: &Ctx, cid: String) {
    let other = {
        let st = ctx.st.lock().unwrap();
        st.idx.get(&cid).cloned()
    };
    let Some(other) = other else { return };
    let user = ctx.user(&other);
    let name = user["displayName"].as_str().unwrap_or("User").to_string();
    let tint = parse_color(user["color"].as_str().unwrap_or(""));
    let uname = user["username"].as_str().unwrap_or("").to_string();
    {
        let mut st = ctx.st.lock().unwrap();
        st.active = Some((cid.clone(), other.clone()));
        st.msgs.clear();
        st.pending.clear();
        st.msg_etag = None;
    }
    ctx.ui_do(move |ui| {
        MSGS.with(|m| m.set_vec(Vec::new()));
        ui.set_editing_id("".into());
        ui.set_chat_initials(initials(&name).into());
        ui.set_chat_title(name.into());
        ui.set_chat_tint(tint);
        ui.set_chat_status(format!("@{uname}").into());
        ui.set_chat_online(false);
        ui.set_peer_typing(false);
        ui.set_search_open(false);
        ui.set_chat_open(true);
    });
    let c = ctx.clone();
    std::thread::spawn(move || {
        poll_meta(&c);
        poll_messages(&c, true);
    });
}

fn close_chat(ctx: &Ctx) {
    let prev = {
        let mut st = ctx.st.lock().unwrap();
        st.active.take()
    };
    if let (Some((cid, _)), Some((uid, tok))) = (prev, ctx.auth()) {
        let cl = ctx.cl.clone();
        std::thread::spawn(move || {
            let _ = cl.delete(&tok, &format!("chats/{cid}/meta/typing/{uid}"));
        });
    }
    ctx.ui_do(|ui| {
        ui.set_chat_open(false);
        ui.set_sheet_open(false);
        slint::Timer::single_shot(Duration::from_millis(380), || {
            MSGS.with(|m| m.set_vec(Vec::new()));
        });
    });
    let c = ctx.clone();
    std::thread::spawn(move || poll_chats(&c));
}

fn poll_meta(ctx: &Ctx) {
    let Some((uid, tok)) = ctx.auth() else { return };
    let Some((cid, other)) = ctx.st.lock().unwrap().active.clone() else { return };
    let meta = ctx.cl.get_val(&tok, &format!("chats/{cid}/meta")).unwrap_or(Value::Null);
    let pres = ctx.cl.get_val(&tok, &format!("presence/{other}")).unwrap_or(Value::Null);
    let now = now_ms();
    let typing = meta["typing"][other.as_str()].as_i64().map(|t| now - t < 4000).unwrap_or(false);
    let online = pres["online"].as_bool().unwrap_or(false);
    let uname = ctx.user(&other)["username"].as_str().unwrap_or("").to_string();
    let status = if online {
        "online".to_string()
    } else if let Some(ts) = pres["lastSeen"].as_i64() {
        format!("last seen {}", last_seen(ts))
    } else {
        format!("@{uname}")
    };
    {
        let mut st = ctx.st.lock().unwrap();
        if st.active.as_ref().map(|a| a.0.as_str()) != Some(cid.as_str()) {
            return;
        }
        if !meta.is_null() {
            st.metas.insert(cid.clone(), meta);
        }
        st.presence.insert(other.clone(), pres);
    }
    let _ = uid;
    ctx.ui_do(move |ui| {
        ui.set_peer_typing(typing);
        ui.set_chat_online(online);
        ui.set_chat_status(status.into());
    });
}

fn parse_msgs(v: &Value) -> Vec<Msg> {
    let mut out: Vec<Msg> = v
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(id, m)| Msg {
                    id: id.clone(),
                    sender: m["senderId"].as_str().unwrap_or("").into(),
                    text: m["text"].as_str().unwrap_or("").into(),
                    created: m["createdAt"].as_i64().unwrap_or(0),
                    seen: m["seen"].as_bool().unwrap_or(false),
                    deleted: m["deleted"].as_bool().unwrap_or(false),
                    edited: m["edited"].as_bool().unwrap_or(false),
                    pending: false,
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|m| m.created);
    out
}

fn poll_messages(ctx: &Ctx, first: bool) {
    let Some((uid, tok)) = ctx.auth() else { return };
    let Some((cid, _)) = ctx.st.lock().unwrap().active.clone() else { return };
    let etag = {
        let st = ctx.st.lock().unwrap();
        st.msg_etag.as_ref().filter(|e| e.0 == cid).map(|e| e.1.clone())
    };
    let q = "orderBy=%22%24key%22&limitToLast=200";
    let res = ctx.cl.get(&tok, &format!("chats/{cid}/messages"), q, etag.as_deref());
    let (val, new_etag) = match res {
        Ok(Some(x)) => x,
        Ok(None) => return,
        Err(_) => return,
    };
    let msgs = parse_msgs(&val);
    let unseen: Vec<String> = msgs.iter().filter(|m| m.sender != uid && !m.seen).map(|m| m.id.clone()).collect();
    {
        let mut st = ctx.st.lock().unwrap();
        if st.active.as_ref().map(|a| a.0.as_str()) != Some(cid.as_str()) {
            return;
        }
        let ids: std::collections::HashSet<&String> = msgs.iter().map(|m| &m.id).collect();
        st.pending.retain(|p| !ids.contains(&p.id));
        st.msgs = msgs;
        if let Some(e) = new_etag {
            st.msg_etag = Some((cid.clone(), e));
        }
    }
    render_messages(ctx, first);
    if !unseen.is_empty() {
        let mut upd = serde_json::Map::new();
        for id in &unseen {
            upd.insert(format!("chats/{cid}/messages/{id}/seen"), json!(true));
        }
        upd.insert(format!("chats/{cid}/meta/unread/{uid}"), json!(0));
        let _ = ctx.cl.patch(&tok, "", &Value::Object(upd));
    }
}

fn render_messages(ctx: &Ctx, force_scroll: bool) {
    let (items, my_last) = {
        let st = ctx.st.lock().unwrap();
        let uid = st.sess.as_ref().map(|s| s.uid.clone()).unwrap_or_default();
        let mut all: Vec<Msg> = st.msgs.clone();
        all.extend(st.pending.iter().cloned());
        all.sort_by_key(|m| m.created);
        let mut items = Vec::with_capacity(all.len());
        let mut last_day = String::new();
        for (i, m) in all.iter().enumerate() {
            let day = day_key(m.created);
            let day_label = if day != last_day { day_label(m.created) } else { String::new() };
            last_day = day;
            let next = all.get(i + 1);
            let last_in_group = next.map(|n| n.sender != m.sender || day_key(n.created) != day_key(m.created) || n.created - m.created > 120_000).unwrap_or(true);
            items.push(MsgItem {
                id: m.id.clone().into(),
                text: m.text.clone().into(),
                time: fmt_time(m.created).into(),
                day: day_label.into(),
                mine: m.sender == uid,
                seen: m.seen,
                pending: m.pending,
                edited: m.edited,
                deleted: m.deleted,
                last_in_group,
            });
        }
        let my_last = all.last().map(|m| m.sender == uid).unwrap_or(false);
        (items, my_last)
    };
    ctx.ui_do(move |ui| {
        let was_bottom = ui.get_at_bottom();
        let prev = MSGS.with(|m| m.row_count());
        let grew = items.len() > prev;
        MSGS.with(|m| sync(m, items));
        if force_scroll || (grew && (was_bottom || my_last)) {
            let w = ui.as_weak();
            for ms in [30u64, 140] {
                let w = w.clone();
                slint::Timer::single_shot(Duration::from_millis(ms), move || {
                    if let Some(u) = w.upgrade() {
                        u.invoke_scroll_bottom();
                    }
                });
            }
        }
    });
}

fn send_message(ctx: &Ctx, text: String) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some((uid, tok)) = ctx.auth() else { return };
    let Some((cid, other)) = ctx.st.lock().unwrap().active.clone() else { return };
    let now = now_ms();
    let id = fb::push_id(now);
    {
        let mut st = ctx.st.lock().unwrap();
        st.pending.push(Msg { id: id.clone(), sender: uid.clone(), text: text.clone(), created: now, seen: false, deleted: false, edited: false, pending: true });
    }
    render_messages(ctx, true);
    let c = ctx.clone();
    std::thread::spawn(move || {
        let body = json!({"senderId": uid, "text": text, "createdAt": now, "seen": false, "deleted": false, "edited": false});
        match c.cl.put(&tok, &format!("chats/{cid}/messages/{id}"), &body) {
            Ok(()) => {
                {
                    let mut st = c.st.lock().unwrap();
                    for p in st.pending.iter_mut().filter(|p| p.id == id) {
                        p.pending = false;
                    }
                }
                render_messages(&c, false);
                let cur = c.st.lock().unwrap().metas.get(&cid).map(|m| m["unread"][other.as_str()].as_i64().unwrap_or(0)).unwrap_or(0);
                let mut upd = serde_json::Map::new();
                upd.insert(format!("chats/{cid}/meta/lastMessageText"), json!(text));
                upd.insert(format!("chats/{cid}/meta/lastMessageAt"), json!(now));
                upd.insert(format!("chats/{cid}/meta/lastMessageSender"), json!(uid));
                upd.insert(format!("chats/{cid}/meta/unread/{other}"), json!(cur + 1));
                let _ = c.cl.patch(&tok, "", &Value::Object(upd));
                let _ = c.cl.delete(&tok, &format!("chats/{cid}/meta/typing/{uid}"));
                if let Some(m) = c.st.lock().unwrap().metas.get_mut(&cid) {
                    m["unread"][other.as_str()] = json!(cur + 1);
                }
            }
            Err(_) => {
                c.st.lock().unwrap().pending.retain(|p| p.id != id);
                render_messages(&c, false);
                c.toast("Message failed to send");
            }
        }
    });
}

fn edit_message(ctx: &Ctx, id: String, text: String) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some((_, tok)) = ctx.auth() else { return };
    let Some((cid, _)) = ctx.st.lock().unwrap().active.clone() else { return };
    {
        let mut st = ctx.st.lock().unwrap();
        for m in st.msgs.iter_mut().filter(|m| m.id == id) {
            m.text = text.clone();
            m.edited = true;
        }
    }
    render_messages(ctx, false);
    let c = ctx.clone();
    std::thread::spawn(move || {
        if c.cl.patch(&tok, &format!("chats/{cid}/messages/{id}"), &json!({"text": text, "edited": true})).is_err() {
            c.toast("Could not edit message");
        }
    });
}

fn delete_message(ctx: &Ctx, id: String) {
    let Some((_, tok)) = ctx.auth() else { return };
    let Some((cid, _)) = ctx.st.lock().unwrap().active.clone() else { return };
    {
        let mut st = ctx.st.lock().unwrap();
        for m in st.msgs.iter_mut().filter(|m| m.id == id) {
            m.deleted = true;
            m.text.clear();
        }
    }
    render_messages(ctx, false);
    let c = ctx.clone();
    std::thread::spawn(move || {
        if c.cl.patch(&tok, &format!("chats/{cid}/messages/{id}"), &json!({"deleted": true, "text": ""})).is_err() {
            c.toast("Could not delete message");
        }
    });
}

fn typing(ctx: &Ctx) {
    let Some((uid, tok)) = ctx.auth() else { return };
    let Some((cid, _)) = ctx.st.lock().unwrap().active.clone() else { return };
    {
        let mut st = ctx.st.lock().unwrap();
        if st.last_typing.map(|t| t.elapsed() < Duration::from_millis(1500)).unwrap_or(false) {
            return;
        }
        st.last_typing = Some(Instant::now());
    }
    let cl = ctx.cl.clone();
    std::thread::spawn(move || {
        let _ = cl.put(&tok, &format!("chats/{cid}/meta/typing/{uid}"), &json!(now_ms()));
    });
}

// ───────────────────────── search / new chat ─────────────────────────
fn search(ctx: &Ctx, q: String) {
    let q = sanitize_username(q.trim().trim_start_matches('@'));
    let my_gen = {
        let mut st = ctx.st.lock().unwrap();
        st.search_gen += 1;
        st.search_gen
    };
    if q.is_empty() {
        ctx.ui_do(|ui| {
            RESULTS.with(|m| m.set_vec(Vec::new()));
            ui.set_searching(false);
        });
        return;
    }
    ctx.ui_do(|ui| ui.set_searching(true));
    let c = ctx.clone();
    std::thread::spawn(move || {
        let Some((uid, tok)) = c.auth() else { return };
        let cached = {
            let st = c.st.lock().unwrap();
            st.usernames.as_ref().filter(|u| u.0.elapsed() < Duration::from_secs(45)).map(|u| u.1.clone())
        };
        let all = match cached {
            Some(v) => v,
            None => {
                let v = c.cl.get_val(&tok, "usernames").unwrap_or(Value::Null);
                c.st.lock().unwrap().usernames = Some((Instant::now(), v.clone()));
                v
            }
        };
        let matches: Vec<(String, String)> = all
            .as_object()
            .map(|o| o.iter().filter(|(k, _)| k.starts_with(&q)).filter_map(|(k, v)| v.as_str().map(|u| (k.clone(), u.to_string()))).filter(|(_, u)| *u != uid).take(20).collect())
            .unwrap_or_default();
        let profiles: Vec<Value> = std::thread::scope(|s| {
            let hs: Vec<_> = matches.iter().map(|(_, u)| s.spawn(|| c.fetch_user(u))).collect();
            hs.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        if c.st.lock().unwrap().search_gen != my_gen {
            return;
        }
        let items: Vec<UserItem> = profiles
            .iter()
            .filter(|p| !p.is_null())
            .map(|p| {
                let name = p["displayName"].as_str().unwrap_or("User").to_string();
                UserItem {
                    uid: p["uid"].as_str().unwrap_or("").into(),
                    username: p["username"].as_str().unwrap_or("").into(),
                    initials: initials(&name).into(),
                    name: name.into(),
                    tint: parse_color(p["color"].as_str().unwrap_or("")),
                    bio: p["bio"].as_str().unwrap_or("").into(),
                }
            })
            .collect();
        c.ui_do(move |ui| {
            RESULTS.with(|m| sync(m, items));
            ui.set_searching(false);
        });
    });
}

fn start_chat(ctx: &Ctx, other: String) {
    let c = ctx.clone();
    std::thread::spawn(move || {
        let Some((uid, tok)) = c.auth() else { return };
        if c.fetch_user(&other).is_null() {
            c.toast("User not found");
            return;
        }
        let mut ids = [uid.clone(), other.clone()];
        ids.sort();
        let cid = ids.join("__");
        let meta = c.cl.get_val(&tok, &format!("chats/{cid}/meta")).unwrap_or(Value::Null);
        if meta.is_null() {
            let body = json!({"members": {uid.clone(): true, other.clone(): true}, "createdAt": now_ms()});
            if c.cl.put(&tok, &format!("chats/{cid}/meta"), &body).is_err() {
                c.toast("Could not start chat");
                return;
            }
            let upd = json!({format!("userChats/{uid}/{cid}"): other, format!("userChats/{other}/{cid}"): uid});
            let _ = c.cl.patch(&tok, "", &upd);
        }
        c.st.lock().unwrap().idx.insert(cid.clone(), other.clone());
        open_chat(&c, cid);
        poll_chats(&c);
    });
}

// ───────────────────────── auth actions ─────────────────────────
fn do_login(ctx: &Ctx, email: String, pw: String) {
    let email = email.trim().to_string();
    if email.is_empty() || pw.is_empty() {
        ctx.ui_do(|ui| ui.set_auth_error("Enter your email and password.".into()));
        return;
    }
    ctx.ui_do(|ui| {
        ui.set_busy(true);
        ui.set_auth_error("".into());
    });
    let c = ctx.clone();
    std::thread::spawn(move || match c.cl.sign_in(&email, &pw) {
        Ok(s) => establish(&c, s),
        Err(e) => c.ui_do(move |ui| {
            ui.set_busy(false);
            ui.set_auth_error(e.into());
        }),
    });
}

fn do_signup(ctx: &Ctx, name: String, username: String, email: String, pw: String) {
    let name = name.trim().to_string();
    let uname = sanitize_username(&username);
    let email = email.trim().to_string();
    let err = if name.is_empty() {
        Some("Please enter your name.")
    } else if uname.len() < 3 {
        Some("Username must be 3-20 letters, numbers or _.")
    } else if !email.contains('@') || !email.contains('.') {
        Some("Please enter a valid email.")
    } else if pw.len() < 6 {
        Some("Password must be at least 6 characters.")
    } else {
        None
    };
    if let Some(e) = err {
        ctx.ui_do(move |ui| ui.set_auth_error(e.into()));
        return;
    }
    ctx.ui_do(|ui| {
        ui.set_busy(true);
        ui.set_auth_error("".into());
    });
    let c = ctx.clone();
    std::thread::spawn(move || {
        // pre-check (rules may deny unauthenticated reads; ignore then)
        if let Ok(v) = c.cl.get_val("", &format!("usernames/{uname}")) {
            if !v.is_null() {
                return c.ui_do(|ui| {
                    ui.set_busy(false);
                    ui.set_auth_error("That username is taken.".into());
                });
            }
        }
        let sess = match c.cl.sign_up(&email, &pw) {
            Ok(s) => s,
            Err(e) => {
                return c.ui_do(move |ui| {
                    ui.set_busy(false);
                    ui.set_auth_error(e.into());
                })
            }
        };
        let mut final_name = uname.clone();
        if let Ok(v) = c.cl.get_val(&sess.id_token, &format!("usernames/{uname}")) {
            if !v.is_null() {
                final_name = format!("{uname}{}", fastrand::u32(100..999));
            }
        }
        let profile = json!({"uid": sess.uid, "username": final_name, "displayName": name, "bio": "", "color": color_for(&sess.uid), "createdAt": now_ms(), "email": email});
        let _ = c.cl.put(&sess.id_token, &format!("users/{}", sess.uid), &profile);
        let _ = c.cl.put(&sess.id_token, &format!("usernames/{final_name}"), &json!(sess.uid));
        establish(&c, sess);
    });
}

fn do_logout(ctx: &Ctx) {
    let c = ctx.clone();
    std::thread::spawn(move || {
        heartbeat(&c, false);
        {
            let mut st = c.st.lock().unwrap();
            let gen = st.gen + 1;
            *st = State::default();
            st.gen = gen;
        }
        clear_file("session.json");
        c.ui_do(|ui| {
            ui.set_chat_open(false);
            ui.set_search_open(false);
            ui.set_profile_open(false);
            ui.set_drawer_open(false);
            CHATS.with(|m| m.set_vec(Vec::new()));
            MSGS.with(|m| m.set_vec(Vec::new()));
            RESULTS.with(|m| m.set_vec(Vec::new()));
            ui.set_busy(false);
            ui.set_screen(1);
        });
    });
}

fn save_profile(ctx: &Ctx, name: String, bio: String) {
    let name = name.trim().to_string();
    if name.is_empty() {
        ctx.toast("Display name cannot be empty");
        return;
    }
    let Some((uid, tok)) = ctx.auth() else { return };
    let c = ctx.clone();
    std::thread::spawn(move || {
        if c.cl.patch(&tok, &format!("users/{uid}"), &json!({"displayName": name, "bio": bio})).is_ok() {
            let p = {
                let mut st = c.st.lock().unwrap();
                st.profile["displayName"] = json!(name);
                st.profile["bio"] = json!(bio);
                let p = st.profile.clone();
                st.users.insert(uid.clone(), p.clone());
                p
            };
            apply_profile(&c, &p);
            c.ui_do(|ui| ui.set_profile_open(false));
            c.toast("Profile saved");
        } else {
            c.toast("Could not save profile");
        }
    });
}

// ───────────────────────── entry ─────────────────────────
pub fn run() {
    init_tz();
    let ui = AppWindow::new().expect("window");
    CHATS.with(|m| ui.set_chats(ModelRc::from(m.clone())));
    MSGS.with(|m| ui.set_messages(ModelRc::from(m.clone())));
    RESULTS.with(|m| ui.set_results(ModelRc::from(m.clone())));

    let ctx = Ctx { ui: ui.as_weak(), st: Arc::new(Mutex::new(State::default())), cl: Client::new() };

    {
        let c = ctx.clone();
        ui.on_login(move |e, p| do_login(&c, e.to_string(), p.to_string()));
        let c = ctx.clone();
        ui.on_signup(move |n, u, e, p| do_signup(&c, n.to_string(), u.to_string(), e.to_string(), p.to_string()));
        let c = ctx.clone();
        ui.on_logout(move || do_logout(&c));
        let c = ctx.clone();
        ui.on_open_chat(move |id| open_chat(&c, id.to_string()));
        let c = ctx.clone();
        ui.on_close_chat(move || close_chat(&c));
        let c = ctx.clone();
        ui.on_send(move |t| send_message(&c, t.to_string()));
        let c = ctx.clone();
        ui.on_edit_message(move |id, t| edit_message(&c, id.to_string(), t.to_string()));
        let c = ctx.clone();
        ui.on_delete_message(move |id| delete_message(&c, id.to_string()));
        let c = ctx.clone();
        ui.on_typing(move || typing(&c));
        let c = ctx.clone();
        ui.on_search(move |q| search(&c, q.to_string()));
        let c = ctx.clone();
        ui.on_start_chat(move |u| start_chat(&c, u.to_string()));
        let c = ctx.clone();
        ui.on_save_profile(move |n, b| save_profile(&c, n.to_string(), b.to_string()));
        ui.on_dark_changed(|d| save_json("prefs.json", &json!({"dark": d})));
        let c = ctx.clone();
        ui.on_search_opened(move || {
            c.st.lock().unwrap().usernames = None;
        });
    }

    boot(ctx);
    ui.run().expect("event loop");
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    if let Some(p) = app.internal_data_path() {
        let _ = DATA_DIR.set(p);
    }
    slint::android::init(app).expect("android init");
    run();
}
