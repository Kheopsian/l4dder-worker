// tr4ker-worker — worker distribué ultra-léger pour L4DDER.
//
// Boucle : lease (demande une tranche au main) -> scrape tr4ker depuis SA propre IP
// -> submit (renvoie les profils bruts au main). Le main fait l'intégration seedtime.
//
// Le worker est volontairement bête : il ne connaît ni le composite ni le seedtime.
// Il scrape /api/users/<name> + /badges et renvoie du JSON brut horodaté.
//
// Config = variables d'environnement (voir .env.example). 3 secrets :
//   LADDER_URL, WORKER_TOKEN  (donnés par l'admin du ladder)
//   TR4KER_USER, TR4KER_PASS  (le PROPRE compte tr4ker du worker — jamais partagé)

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}
fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}
fn env_or(k: &str, d: &str) -> String {
    let v = env(k);
    if v.is_empty() { d.to_string() } else { v }
}
fn sleep(secs: f64) {
    std::thread::sleep(std::time::Duration::from_secs_f64(secs.max(0.0)));
}
fn log(msg: &str) {
    println!("[{:.0}] {}", now(), msg);
}

// ----------------------------- config -----------------------------
struct Cfg {
    ladder: String,   // ex https://ladder.kheopsian.com
    token: String,    // token worker (auth vers le ladder)
    tr_base: String,  // https://tr4ker.net
    tr_user: String,  // chemin A : identifiants (login auto + re-login à l'expiry)
    tr_pass: String,
    tr_cookie: String, // chemin B : cookie TR4KER_session collé (valeur du JWT, sans le nom)
    batch: u32,       // taille de lease demandée
}
impl Cfg {
    fn load() -> Cfg {
        // le cookie peut être fourni brut (JWT) ou déjà préfixé "TR4KER_session=..."
        let raw = env("TR4KER_COOKIE");
        let cookie = raw.trim().trim_start_matches("TR4KER_session=").to_string();
        Cfg {
            ladder: env("LADDER_URL").trim_end_matches('/').to_string(),
            token: env("WORKER_TOKEN"),
            tr_base: env_or("TR4KER_BASE", "https://tr4ker.net").trim_end_matches('/').to_string(),
            tr_user: env("TR4KER_USER"),
            tr_pass: env("TR4KER_PASS"),
            tr_cookie: cookie,
            batch: env_or("BATCH", "150").parse().unwrap_or(150),
        }
    }
    fn has_creds(&self) -> bool { !self.tr_user.is_empty() && !self.tr_pass.is_empty() }
}

// résultat d'un GET : Ok(json), Gone (404 = profil inexistant), Fail (transitoire)
enum Fetch { Ok(serde_json::Value), Gone, Fail }
// résultat d'un scrape user : Ok(profil), Gone (404 -> tombstone), Fail (à re-tenter)
enum ScrapeOut { Ok(Sample), Gone, Fail }

// ------------------------- tr4ker session -------------------------
// Auth cookie-based. On capture Set-Cookie: TR4KER_session au login et on le
// réinjecte manuellement (pas de cookie-jar -> plus léger). Re-login sur 401.
struct Tr4ker<'a> {
    cfg: &'a Cfg,
    agent: ureq::Agent,
    cookie: Option<String>, // "TR4KER_session=..."
    // AIMD local : chaque worker a son propre bucket (IP,endpoint) côté tr4ker.
    rps: f64,
    target: f64,
    last_req: f64,
    consec429: u32,
    cooldown_until: f64,
}
impl<'a> Tr4ker<'a> {
    fn new(cfg: &'a Cfg) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(25))
            .user_agent(UA)
            .build();
        // chemin B : si un cookie est fourni, on démarre déjà authentifié.
        let cookie = if cfg.tr_cookie.is_empty() { None }
                     else { Some(format!("TR4KER_session={}", cfg.tr_cookie)) };
        Tr4ker { cfg, agent, cookie, rps: 1.0, target: 1.0,
                 last_req: 0.0, consec429: 0, cooldown_until: 0.0 }
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
                // Set-Cookie: TR4KER_session=<jwt>; Path=/; HttpOnly; ...
                for h in resp.all("set-cookie") {
                    if let Some(pos) = h.find("TR4KER_session=") {
                        let rest = &h[pos..];
                        let end = rest.find(';').unwrap_or(rest.len());
                        self.cookie = Some(rest[..end].to_string());
                        log(&format!("[auth] logged in as {}", self.cfg.tr_user));
                        return true;
                    }
                }
                log("[auth] login 200 mais pas de cookie");
                false
            }
            // login throttlé -> backoff (ne PAS marteler, ça aggrave le rate-limit)
            Err(ureq::Error::Status(429, _)) => {
                self.consec429 += 1;
                let back = (30.0 * self.consec429 as f64).min(300.0);
                self.cooldown_until = now() + back;
                log(&format!("[auth] 429 sur login, cooldown {:.0}s", back));
                false
            }
            Err(e) => { log(&format!("[auth] fail {}", e)); false }
        }
    }

    // GET authentifié avec pacing AIMD + gestion 429/404/401.
    fn get(&mut self, path: &str) -> Fetch {
        if now() < self.cooldown_until { sleep(self.cooldown_until - now()); }
        let gap = 1.0 / self.rps.max(0.2);
        let wait = self.last_req + gap - now();
        if wait > 0.0 { sleep(wait); }
        self.last_req = now();

        if self.cookie.is_none() {
            // pas de cookie -> tenter un login si on a des identifiants
            if self.cfg.has_creds() {
                if !self.login() { return Fetch::Fail; }
            } else {
                log("[auth] cookie expiré et aucun identifiant fourni — fournir TR4KER_COOKIE frais \
                     ou TR4KER_USER/PASS. Pause 5 min.");
                self.cooldown_until = now() + 300.0;
                return Fetch::Fail;
            }
        }
        let cookie = self.cookie.clone().unwrap();
        let url = format!("{}{}", self.cfg.tr_base, path);
        let resp = self.agent.get(&url).set("Cookie", &cookie).call();

        match resp {
            Ok(r) => {
                self.consec429 = 0;
                if self.rps < self.target {
                    self.rps = (self.rps + 0.15).min(self.target);
                }
                match r.into_json() { Ok(v) => Fetch::Ok(v), Err(_) => Fetch::Fail }
            }
            Err(ureq::Error::Status(429, _)) => {
                self.consec429 += 1;
                let back = (20.0 * self.consec429 as f64).min(300.0);
                self.cooldown_until = now() + back;
                if self.consec429 >= 2 { self.rps = (self.rps * 0.8).max(0.3); }
                log(&format!("[429] x{} cooldown {:.0}s rps {:.2}", self.consec429, back, self.rps));
                Fetch::Fail
            }
            Err(ureq::Error::Status(404, _)) => Fetch::Gone,
            Err(ureq::Error::Status(401, _)) => { self.cookie = None; Fetch::Fail }
            Err(ureq::Error::Status(_, _)) => Fetch::Fail,
            Err(e) => { log(&format!("[req] {}", e)); Fetch::Fail }
        }
    }

    // Scrape complet d'un user : profil + tier upload (badges).
    // Gone = 404 (profil inexistant, à tombstone) ; Fail = transitoire (re-tenter).
    fn scrape_user(&mut self, name: &str) -> ScrapeOut {
        let d = match self.get(&format!("/api/users/{}", enc(name))) {
            Fetch::Ok(d) => d,
            Fetch::Gone => return ScrapeOut::Gone,
            Fetch::Fail => return ScrapeOut::Fail,
        };
        let id = match d.get("id").and_then(|v| v.as_i64()) {
            Some(i) => i,
            None => return ScrapeOut::Gone,   // 200 sans id -> traiter comme introuvable
        };
        let tier = self.badge_tier(name); // best-effort, peut être None
        ScrapeOut::Ok(Sample {
            username: name.to_string(),
            id,
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
            Fetch::Ok(d) => d,
            _ => return None,
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
}

// -------- helpers extraction JSON (tolérants aux champs manquants) --------
fn ival(d: &serde_json::Value, k: &str) -> i64 { d.get(k).and_then(|v| v.as_i64()).unwrap_or(0) }
fn sval(d: &serde_json::Value, k: &str) -> Option<String> {
    d.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
}
fn enc(s: &str) -> String {
    // encodage minimal du segment de path (les pseudos peuvent avoir . _ - déjà sûrs)
    s.chars().map(|c| match c {
        ' ' => "%20".to_string(),
        '/' => "%2F".to_string(),
        '?' => "%3F".to_string(),
        '#' => "%23".to_string(),
        '&' => "%26".to_string(),
        _ => c.to_string(),
    }).collect()
}

// --------------------------- protocole ladder ---------------------------
#[derive(Serialize)]
struct Sample {
    username: String,
    id: i64,
    role: Option<String>,
    uploaded: i64,
    downloaded: i64,
    bonus_upload: i64,
    seeding_count: i64,
    joined_at: Option<String>,
    last_seen_at: Option<String>,
    upload_tier: Option<i64>,
    scraped_at: f64,
}

#[derive(Deserialize)]
struct Lease {
    #[serde(default)]
    lease_id: String,
    #[serde(default)]
    items: Vec<String>,
}

fn lease(cfg: &Cfg, agent: &ureq::Agent) -> Option<Lease> {
    let url = format!("{}/api/worker/lease", cfg.ladder);
    match agent.post(&url).send_json(json!({
        "token": cfg.token, "kind": "users_refresh", "max": cfg.batch
    })) {
        Ok(r) => r.into_json::<Lease>().ok(),
        Err(ureq::Error::Status(code, _)) => { log(&format!("[lease] HTTP {}", code)); None }
        Err(e) => { log(&format!("[lease] {}", e)); None }
    }
}

fn submit(cfg: &Cfg, agent: &ureq::Agent, lease_id: &str,
          results: &[Sample], failed: &[String], gone: &[String]) -> bool {
    let url = format!("{}/api/worker/submit", cfg.ladder);
    match agent.post(&url).send_json(json!({
        "token": cfg.token, "lease_id": lease_id,
        "results": results, "failed": failed, "gone": gone
    })) {
        Ok(_) => true,
        Err(e) => { log(&format!("[submit] {}", e)); false }
    }
}

// ------------------------------- main -------------------------------
fn main() {
    let cfg = Cfg::load();
    for (k, v) in [("LADDER_URL", &cfg.ladder), ("WORKER_TOKEN", &cfg.token)] {
        if v.is_empty() { eprintln!("config manquante: {}", k); std::process::exit(2); }
    }
    // il faut AU MOINS un chemin d'auth tr4ker : identifiants OU cookie.
    if !cfg.has_creds() && cfg.tr_cookie.is_empty() {
        eprintln!("config manquante: fournir TR4KER_USER+TR4KER_PASS, ou TR4KER_COOKIE");
        std::process::exit(2);
    }
    let mode = if cfg.has_creds() { "identifiants" } else { "cookie" };
    log(&format!("worker start -> ladder={} auth={} tr4ker_user={} batch={}",
                 cfg.ladder, mode, if cfg.tr_user.is_empty() { "(cookie)" } else { &cfg.tr_user }, cfg.batch));

    let ladder_agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("l4dder-worker/0.1").build();
    let mut tr = Tr4ker::new(&cfg);
    let mut idle = 0u32;

    loop {
        let l = match lease(&cfg, &ladder_agent) {
            Some(l) if !l.items.is_empty() => { idle = 0; l }
            _ => {
                idle = (idle + 1).min(12);
                let s = 5.0 * idle as f64; // backoff quand pas de travail (max 60s)
                log(&format!("pas de travail, sleep {:.0}s", s));
                sleep(s);
                continue;
            }
        };
        log(&format!("lease {} — {} users", &l.lease_id[..l.lease_id.len().min(8)], l.items.len()));

        let mut results: Vec<Sample> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut gone: Vec<String> = Vec::new();
        for name in &l.items {
            match tr.scrape_user(name) {
                ScrapeOut::Ok(s) => results.push(s),
                ScrapeOut::Gone => gone.push(name.clone()),
                ScrapeOut::Fail => failed.push(name.clone()),
            }
        }
        let ok = submit(&cfg, &ladder_agent, &l.lease_id, &results, &failed, &gone);
        log(&format!("submit lease={} ok={} scraped={} gone={} failed={}",
                     &l.lease_id[..l.lease_id.len().min(8)], ok, results.len(), gone.len(), failed.len()));
    }
}
