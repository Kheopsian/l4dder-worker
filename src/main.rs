// l4dder-worker — worker distribué ultra-léger pour L4DDER.
//
// DEUX LANES CONCURRENTES par worker (le rate-limit tr4ker est par (IP,endpoint)) :
//   - refresh : lease de users -> GET /api/users/<name>(+/badges) -> renvoie profils bruts
//   - search  : lease de préfixes -> GET /api/users/search?q=<pfx> -> renvoie pseudos trouvés
// Les 2 tapent des endpoints DIFFÉRENTS -> buckets indépendants -> zéro contention.
// Chaque lane a son propre client tr4ker (cookie + AIMD indépendants). Le main
// (serveur) reste seul maître du calcul seedtime + de l'arbre de préfixes sweep.
//
// Config = variables d'environnement (voir .env.example).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

fn now() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() }
fn env(k: &str) -> String { std::env::var(k).unwrap_or_default() }
fn env_or(k: &str, d: &str) -> String { let v = env(k); if v.is_empty() { d.to_string() } else { v } }
fn sleep(secs: f64) { std::thread::sleep(std::time::Duration::from_secs_f64(secs.max(0.0))); }
fn log(msg: &str) {
    use std::io::Write;
    let line = format!("[{:.0}] {}", now(), msg);
    let _ = writeln!(std::io::stdout(), "{}", line);
    if let Ok(p) = std::env::var("WORKER_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{}", line);
        }
    }
}

// ----------------------------- config -----------------------------
#[derive(Clone)]
struct Cfg {
    ladder: String,
    token: String,
    tr_base: String,
    tr_user: String,
    tr_pass: String,
    tr_cookie: String,
    batch: u32,
}
impl Cfg {
    fn load() -> Cfg {
        let raw = env("TR4KER_COOKIE");
        let cookie = raw.trim().trim_start_matches("TR4KER_session=").to_string();
        Cfg {
            ladder: env("LADDER_URL").trim_end_matches('/').to_string(),
            token: env("WORKER_TOKEN"),
            tr_base: env_or("TR4KER_BASE", "https://tr4ker.net").trim_end_matches('/').to_string(),
            tr_user: env("TR4KER_USER"),
            tr_pass: env("TR4KER_PASS"),
            tr_cookie: cookie,
            batch: env_or("BATCH", "100").parse().unwrap_or(100),
        }
    }
    fn has_creds(&self) -> bool { !self.tr_user.is_empty() && !self.tr_pass.is_empty() }
}

// résultat d'un GET : Ok(json), Gone (404), Fail (transitoire)
enum Fetch { Ok(Value), Gone, Fail }
// résultat d'un scrape user
enum ScrapeOut { Ok(Sample), Gone, Fail }

// ------------------------- tr4ker session -------------------------
struct Tr4ker {
    cfg: Cfg,
    agent: ureq::Agent,
    cookie: Option<String>,
    rps: f64,
    target: f64,
    last_req: f64,
    consec429: u32,
    cooldown_until: f64,
    tag: &'static str, // "refresh" / "search" pour les logs
}
impl Tr4ker {
    fn new(cfg: Cfg, tag: &'static str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(25))
            .user_agent(UA)
            .build();
        let cookie = if cfg.tr_cookie.is_empty() { None }
                     else { Some(format!("TR4KER_session={}", cfg.tr_cookie)) };
        Tr4ker { cfg, agent, cookie, rps: 1.0, target: 1.0,
                 last_req: 0.0, consec429: 0, cooldown_until: 0.0, tag }
    }

    fn login(&mut self) -> bool {
        let url = format!("{}/api/auth/login", self.cfg.tr_base);
        let body = json!({"identifier": self.cfg.tr_user,
                          "password": self.cfg.tr_pass, "remember_me": true});
        match self.agent.post(&url)
            .set("Origin", &self.cfg.tr_base)
            .set("Referer", &format!("{}/login", self.cfg.tr_base))
            .send_json(body)
        {
            Ok(resp) => {
                for h in resp.all("set-cookie") {
                    if let Some(pos) = h.find("TR4KER_session=") {
                        let rest = &h[pos..];
                        let end = rest.find(';').unwrap_or(rest.len());
                        self.cookie = Some(rest[..end].to_string());
                        log(&format!("[{}][auth] logged in as {}", self.tag, self.cfg.tr_user));
                        return true;
                    }
                }
                log(&format!("[{}][auth] login 200 mais pas de cookie", self.tag));
                false
            }
            Err(ureq::Error::Status(429, _)) => {
                self.consec429 += 1;
                let back = (30.0 * self.consec429 as f64).min(300.0);
                self.cooldown_until = now() + back;
                log(&format!("[{}][auth] 429 sur login, cooldown {:.0}s", self.tag, back));
                false
            }
            Err(e) => { log(&format!("[{}][auth] fail {}", self.tag, e)); false }
        }
    }

    fn get(&mut self, path: &str) -> Fetch {
        if now() < self.cooldown_until { sleep(self.cooldown_until - now()); }
        let gap = 1.0 / self.rps.max(0.2);
        let wait = self.last_req + gap - now();
        if wait > 0.0 { sleep(wait); }
        self.last_req = now();

        if self.cookie.is_none() {
            if self.cfg.has_creds() {
                if !self.login() { return Fetch::Fail; }
            } else {
                log(&format!("[{}][auth] cookie expiré et aucun identifiant — pause 5 min.", self.tag));
                self.cooldown_until = now() + 300.0;
                return Fetch::Fail;
            }
        }
        let cookie = self.cookie.clone().unwrap();
        let url = format!("{}{}", self.cfg.tr_base, path);
        match self.agent.get(&url).set("Cookie", &cookie).call() {
            Ok(r) => {
                self.consec429 = 0;
                if self.rps < self.target { self.rps = (self.rps + 0.15).min(self.target); }
                match r.into_json() { Ok(v) => Fetch::Ok(v), Err(_) => Fetch::Fail }
            }
            Err(ureq::Error::Status(429, _)) => {
                self.consec429 += 1;
                let back = (20.0 * self.consec429 as f64).min(300.0);
                self.cooldown_until = now() + back;
                if self.consec429 >= 2 { self.rps = (self.rps * 0.8).max(0.3); }
                log(&format!("[{}][429] x{} cooldown {:.0}s rps {:.2}", self.tag, self.consec429, back, self.rps));
                Fetch::Fail
            }
            Err(ureq::Error::Status(404, _)) => Fetch::Gone,
            Err(ureq::Error::Status(401, _)) => { self.cookie = None; Fetch::Fail }
            Err(ureq::Error::Status(_, _)) => Fetch::Fail,
            Err(e) => { log(&format!("[{}][req] {}", self.tag, e)); Fetch::Fail }
        }
    }

    // refresh : profil + tier upload. Gone=404 (tombstone), Fail=transitoire.
    fn scrape_user(&mut self, name: &str) -> ScrapeOut {
        let d = match self.get(&format!("/api/users/{}", enc(name))) {
            Fetch::Ok(d) => d,
            Fetch::Gone => return ScrapeOut::Gone,
            Fetch::Fail => return ScrapeOut::Fail,
        };
        let id = match d.get("id").and_then(|v| v.as_i64()) {
            Some(i) => i,
            None => return ScrapeOut::Gone,
        };
        let tier = self.badge_tier(name);
        ScrapeOut::Ok(Sample {
            username: name.to_string(), id,
            role: sval(&d, "role"),
            uploaded: ival(&d, "uploaded"),
            downloaded: ival(&d, "downloaded"),
            bonus_upload: ival(&d, "bonus_upload"),
            seeding_count: ival(&d, "seeding_count"),
            joined_at: sval(&d, "joined_at"),
            last_seen_at: sval(&d, "last_seen_at"),
            upload_tier: tier,
            scraped_at: now(),
        })
    }

    fn badge_tier(&mut self, name: &str) -> Option<i64> {
        let d = match self.get(&format!("/api/users/{}/badges", enc(name))) {
            Fetch::Ok(d) => d, _ => return None,
        };
        let items = if d.is_array() { d.as_array().cloned().unwrap_or_default() }
                    else { d.get("badges").or_else(|| d.get("data"))
                            .and_then(|v| v.as_array()).cloned().unwrap_or_default() };
        let mut mx = 0i64;
        for b in items {
            let nm = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if nm.contains("Upload") {
                let digits: String = nm.chars().filter(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = digits.parse::<i64>() { mx = mx.max(n); }
                else if nm.contains("Premier") { mx = mx.max(1); }
            }
        }
        Some(mx)
    }

    // search : GET /api/users/search?q=<pfx> -> liste des pseudos (cap 10 côté serveur).
    // Some(vec) = OK (vec éventuellement vide) ; None = échec transitoire (re-lease).
    fn search_prefix(&mut self, pfx: &str) -> Option<Vec<String>> {
        let d = match self.get(&format!("/api/users/search?q={}", enc(pfx))) {
            Fetch::Ok(d) => d,
            Fetch::Gone => return Some(vec![]),
            Fetch::Fail => return None,
        };
        let arr = if d.is_array() { d.as_array().cloned().unwrap_or_default() }
                  else { d.get("users").or_else(|| d.get("data"))
                          .and_then(|v| v.as_array()).cloned().unwrap_or_default() };
        let mut users = Vec::new();
        for u in arr {
            if let Some(nm) = u.get("username").and_then(|v| v.as_str()) {
                users.push(nm.to_string());
            }
        }
        Some(users)
    }
}

fn ival(d: &Value, k: &str) -> i64 { d.get(k).and_then(|v| v.as_i64()).unwrap_or(0) }
fn sval(d: &Value, k: &str) -> Option<String> { d.get(k).and_then(|v| v.as_str()).map(|s| s.to_string()) }
fn enc(s: &str) -> String {
    s.chars().map(|c| match c {
        ' ' => "%20".to_string(), '/' => "%2F".to_string(), '?' => "%3F".to_string(),
        '#' => "%23".to_string(), '&' => "%26".to_string(), _ => c.to_string(),
    }).collect()
}

// --------------------------- protocole ladder ---------------------------
#[derive(Serialize)]
struct Sample {
    username: String, id: i64, role: Option<String>,
    uploaded: i64, downloaded: i64, bonus_upload: i64, seeding_count: i64,
    joined_at: Option<String>, last_seen_at: Option<String>,
    upload_tier: Option<i64>, scraped_at: f64,
}

#[derive(Deserialize)]
struct Lease {
    #[serde(default)] lease_id: String,
    #[serde(default)] items: Vec<String>,
}

fn lease(cfg: &Cfg, agent: &ureq::Agent, kind: &str) -> Option<Lease> {
    let url = format!("{}/api/worker/lease", cfg.ladder);
    match agent.post(&url).send_json(json!({"token": cfg.token, "kind": kind, "max": cfg.batch})) {
        Ok(r) => r.into_json::<Lease>().ok(),
        Err(ureq::Error::Status(code, _)) => { log(&format!("[{}][lease] HTTP {}", kind, code)); None }
        Err(e) => { log(&format!("[{}][lease] {}", kind, e)); None }
    }
}

fn submit_refresh(cfg: &Cfg, agent: &ureq::Agent, lease_id: &str,
                  results: &[Sample], failed: &[String], gone: &[String]) -> bool {
    let url = format!("{}/api/worker/submit", cfg.ladder);
    agent.post(&url).send_json(json!({
        "token": cfg.token, "lease_id": lease_id,
        "results": results, "failed": failed, "gone": gone
    })).map(|_| true).unwrap_or_else(|e| { log(&format!("[refresh][submit] {}", e)); false })
}

fn submit_search(cfg: &Cfg, agent: &ureq::Agent, lease_id: &str,
                 search_results: &[Value], failed: &[String]) -> bool {
    let url = format!("{}/api/worker/submit", cfg.ladder);
    agent.post(&url).send_json(json!({
        "token": cfg.token, "lease_id": lease_id,
        "search_results": search_results, "failed": failed
    })).map(|_| true).unwrap_or_else(|e| { log(&format!("[search][submit] {}", e)); false })
}

// ------------------------------- lanes -------------------------------
fn short(id: &str) -> &str { &id[..id.len().min(8)] }

fn refresh_lane(cfg: Cfg, la: ureq::Agent) {
    let mut tr = Tr4ker::new(cfg.clone(), "refresh");
    let mut idle = 0u32;
    loop {
        let l = match lease(&cfg, &la, "users_refresh") {
            Some(l) if !l.items.is_empty() => { idle = 0; l }
            _ => { idle = (idle + 1).min(12); sleep(5.0 * idle as f64); continue; }
        };
        let mut results = Vec::new();
        let mut failed = Vec::new();
        let mut gone = Vec::new();
        for name in &l.items {
            match tr.scrape_user(name) {
                ScrapeOut::Ok(s) => results.push(s),
                ScrapeOut::Gone => gone.push(name.clone()),
                ScrapeOut::Fail => failed.push(name.clone()),
            }
        }
        let ok = submit_refresh(&cfg, &la, &l.lease_id, &results, &failed, &gone);
        log(&format!("[refresh] lease={} ok={} scraped={} gone={} failed={}",
                     short(&l.lease_id), ok, results.len(), gone.len(), failed.len()));
    }
}

fn search_lane(cfg: Cfg, la: ureq::Agent) {
    let mut tr = Tr4ker::new(cfg.clone(), "search");
    let mut idle = 0u32;
    loop {
        let l = match lease(&cfg, &la, "search") {
            Some(l) if !l.items.is_empty() => { idle = 0; l }
            _ => { idle = (idle + 1).min(12); sleep(10.0 * idle as f64); continue; }
        };
        let mut search_results: Vec<Value> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut found_total = 0usize;
        for pfx in &l.items {
            match tr.search_prefix(pfx) {
                Some(users) => { found_total += users.len();
                    search_results.push(json!({"prefix": pfx, "users": users})); }
                None => failed.push(pfx.clone()),
            }
        }
        let ok = submit_search(&cfg, &la, &l.lease_id, &search_results, &failed);
        log(&format!("[search] lease={} ok={} prefixes={} users_found={} failed={}",
                     short(&l.lease_id), ok, search_results.len(), found_total, failed.len()));
    }
}

// ------------------------------- main -------------------------------
fn main() {
    let cfg = Cfg::load();
    for (k, v) in [("LADDER_URL", &cfg.ladder), ("WORKER_TOKEN", &cfg.token)] {
        if v.is_empty() { eprintln!("config manquante: {}", k); std::process::exit(2); }
    }
    if !cfg.has_creds() && cfg.tr_cookie.is_empty() {
        eprintln!("config manquante: fournir TR4KER_USER+TR4KER_PASS, ou TR4KER_COOKIE");
        std::process::exit(2);
    }
    let mode = if cfg.has_creds() { "identifiants" } else { "cookie" };
    log(&format!("worker start (2 lanes) -> ladder={} auth={} tr4ker_user={} batch={}",
                 cfg.ladder, mode, if cfg.tr_user.is_empty() { "(cookie)" } else { &cfg.tr_user }, cfg.batch));

    let la = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("l4dder-worker/0.2").build();

    // 2 lanes concurrentes : buckets (IP,endpoint) distincts -> pas de contention.
    let (c1, la1) = (cfg.clone(), la.clone());
    let h1 = std::thread::spawn(move || refresh_lane(c1, la1));
    let (c2, la2) = (cfg.clone(), la.clone());
    let h2 = std::thread::spawn(move || search_lane(c2, la2));
    let _ = h1.join();
    let _ = h2.join();
}
