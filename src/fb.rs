//! Firebase REST client (Auth + Realtime Database). No SDK, plain HTTPS.
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const API_KEY: &str = "AIzaSyAGcepsaKBQV3dxrV6kBVFM5tFW9lE3JJY";
pub const DB_URL: &str = "https://aryancompanion-default-rtdb.asia-southeast1.firebasedatabase.app";

#[derive(Clone)]
pub struct Session {
    pub uid: String,
    pub email: String,
    pub id_token: String,
    pub refresh_token: String,
    pub expires: Instant,
}

impl Session {
    pub fn near_expiry(&self) -> bool {
        Instant::now() + Duration::from_secs(120) >= self.expires
    }
}

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
}

pub type Res<T> = Result<T, String>;

impl Client {
    pub fn new() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .build()
            .into();
        Client { agent }
    }

    fn read(mut resp: ureq::http::Response<ureq::Body>) -> Res<(u16, Value, Option<String>)> {
        let status = resp.status().as_u16();
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        let v = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
        Ok((status, v, etag))
    }

    // ───────── Auth ─────────
    fn auth_call(&self, endpoint: &str, email: &str, password: &str) -> Res<Session> {
        let url = format!("https://identitytoolkit.googleapis.com/v1/accounts:{endpoint}?key={API_KEY}");
        let resp = self
            .agent
            .post(&url)
            .send_json(json!({"email": email, "password": password, "returnSecureToken": true}))
            .map_err(|_| "No internet connection".to_string())?;
        let (status, v, _) = Self::read(resp)?;
        if status != 200 {
            let msg = v["error"]["message"].as_str().unwrap_or("UNKNOWN");
            return Err(friendly_auth_error(msg));
        }
        Ok(Session {
            uid: v["localId"].as_str().unwrap_or("").into(),
            email: email.into(),
            id_token: v["idToken"].as_str().unwrap_or("").into(),
            refresh_token: v["refreshToken"].as_str().unwrap_or("").into(),
            expires: Instant::now() + Duration::from_secs(v["expiresIn"].as_str().and_then(|s| s.parse().ok()).unwrap_or(3600)),
        })
    }

    pub fn sign_in(&self, email: &str, password: &str) -> Res<Session> {
        self.auth_call("signInWithPassword", email, password)
    }

    pub fn sign_up(&self, email: &str, password: &str) -> Res<Session> {
        self.auth_call("signUp", email, password)
    }

    pub fn refresh(&self, refresh_token: &str, email: &str) -> Res<Session> {
        let url = format!("https://securetoken.googleapis.com/v1/token?key={API_KEY}");
        let resp = self
            .agent
            .post(&url)
            .send_form([("grant_type", "refresh_token"), ("refresh_token", refresh_token)])
            .map_err(|_| "No internet connection".to_string())?;
        let (status, v, _) = Self::read(resp)?;
        if status != 200 {
            return Err("Session expired".into());
        }
        Ok(Session {
            uid: v["user_id"].as_str().unwrap_or("").into(),
            email: email.into(),
            id_token: v["id_token"].as_str().unwrap_or("").into(),
            refresh_token: v["refresh_token"].as_str().unwrap_or(refresh_token).into(),
            expires: Instant::now() + Duration::from_secs(v["expires_in"].as_str().and_then(|s| s.parse().ok()).unwrap_or(3600)),
        })
    }

    // ───────── Realtime Database (REST) ─────────
    fn url(path: &str, token: &str, extra: &str) -> String {
        let path = path.trim_matches('/');
        let mut q = Vec::new();
        if !token.is_empty() {
            q.push(format!("auth={token}"));
        }
        if !extra.is_empty() {
            q.push(extra.to_string());
        }
        if q.is_empty() {
            format!("{DB_URL}/{path}.json")
        } else {
            format!("{DB_URL}/{path}.json?{}", q.join("&"))
        }
    }

    /// GET. Returns Ok(None) when the server answers 304 (ETag unchanged).
    pub fn get(&self, token: &str, path: &str, query: &str, etag: Option<&str>) -> Res<Option<(Value, Option<String>)>> {
        let mut req = self.agent.get(&Self::url(path, token, query)).header("X-Firebase-ETag", "true");
        if let Some(e) = etag {
            req = req.header("if-none-match", e);
        }
        let resp = req.call().map_err(|e| e.to_string())?;
        let (status, v, new_etag) = Self::read(resp)?;
        match status {
            304 => Ok(None),
            200 => Ok(Some((v, new_etag))),
            401 | 403 => Err("permission".into()),
            s => Err(format!("HTTP {s}")),
        }
    }

    pub fn get_val(&self, token: &str, path: &str) -> Res<Value> {
        Ok(self.get(token, path, "", None)?.map(|x| x.0).unwrap_or(Value::Null))
    }

    pub fn put(&self, token: &str, path: &str, body: &Value) -> Res<()> {
        let resp = self.agent.put(&Self::url(path, token, "")).send_json(body).map_err(|e| e.to_string())?;
        let (s, _, _) = Self::read(resp)?;
        if s == 200 { Ok(()) } else { Err(format!("HTTP {s}")) }
    }

    pub fn patch(&self, token: &str, path: &str, body: &Value) -> Res<()> {
        let resp = self.agent.patch(&Self::url(path, token, "")).send_json(body).map_err(|e| e.to_string())?;
        let (s, _, _) = Self::read(resp)?;
        if s == 200 { Ok(()) } else { Err(format!("HTTP {s}")) }
    }

    pub fn delete(&self, token: &str, path: &str) -> Res<()> {
        let resp = self.agent.delete(&Self::url(path, token, "")).call().map_err(|e| e.to_string())?;
        let (s, _, _) = Self::read(resp)?;
        if s == 200 { Ok(()) } else { Err(format!("HTTP {s}")) }
    }
}

fn friendly_auth_error(code: &str) -> String {
    let c = code.split(' ').next().unwrap_or(code);
    match c {
        "EMAIL_EXISTS" => "That email is already registered. Try logging in.",
        "INVALID_EMAIL" => "That email address looks invalid.",
        "EMAIL_NOT_FOUND" => "No account found with that email.",
        "INVALID_PASSWORD" | "INVALID_LOGIN_CREDENTIALS" => "Incorrect email or password.",
        "WEAK_PASSWORD" => "Password must be at least 6 characters.",
        "TOO_MANY_ATTEMPTS_TRY_LATER" => "Too many attempts. Try again later.",
        "USER_DISABLED" => "This account has been disabled.",
        _ => return format!("Something went wrong ({c})"),
    }
    .to_string()
}

/// Firebase-style push id: time-ordered, unique, generated client side.
pub fn push_id(now_ms: i64) -> String {
    const ALPHA: &[u8] = b"-0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz";
    let mut t = now_ms as u64;
    let mut ts = [0u8; 8];
    for i in (0..8).rev() {
        ts[i] = ALPHA[(t % 64) as usize];
        t /= 64;
    }
    let mut s = String::with_capacity(20);
    s.push_str(std::str::from_utf8(&ts).unwrap());
    for _ in 0..12 {
        s.push(ALPHA[fastrand::usize(0..64)] as char);
    }
    s
}
