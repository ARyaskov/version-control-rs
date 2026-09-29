use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use actix_web::http::{StatusCode, header};
use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use base64::Engine;
use flate2::read::ZlibDecoder;
use quick_xml::Reader;
use quick_xml::events::Event;
use uuid::Uuid;

use crate::auth::{is_password_hash, verify_password};
use crate::client::Client;
use crate::error::{Result, VcsError};
use crate::repo::{Repository, TreeEdits};
use crate::types::ChangedPathAction;

/// Cap on concurrently open commit activities (abandoned MKACTIVITY sessions
/// would otherwise leak memory indefinitely).
const MAX_ACTIVITIES: usize = 256;
/// Cap on bytes buffered in a single activity before it is committed. Bounds the
/// memory a client can pin with PUT requests.
const MAX_ACTIVITY_BYTES: usize = 128 * 1024 * 1024;
/// HTTP Basic auth realm advertised when a password file is configured.
const AUTH_REALM: &str = "vcrs";
/// Bound on remembered successful password checks (see `AppState::auth_cache`).
const AUTH_CACHE_LIMIT: usize = 1024;

/// Server behaviour switches (all default to the safe choice).
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Run `.vcrs/hooks` scripts on commits received over HTTP. Off by default:
    /// hook execution on a network-facing server must be an explicit decision.
    pub enable_hooks: bool,
    /// Without `.vcrs/passwd.json` every client is anonymous; writes are then
    /// refused unless this is set.
    pub allow_anonymous_write: bool,
    /// Serve plain HTTP on a non-loopback address. Off by default: Basic
    /// credentials and repository content would cross the network in clear
    /// text — terminate TLS in a reverse proxy instead.
    pub allow_insecure_http: bool,
}

#[derive(Debug)]
struct AppState {
    repo_root: PathBuf,
    options: ServeOptions,
    activities: Mutex<HashMap<String, TxnActivity>>,
    /// Serializes commit application (the repository lock also does, across
    /// processes).
    commit_lock: Mutex<()>,
    /// Successful Basic-auth checks, keyed by a hash of user, password and the
    /// stored hash: argon2 is deliberately slow, so it runs once per
    /// credential rather than on every request. A changed passwd.json entry
    /// changes the key.
    auth_cache: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone, Default)]
struct TxnActivity {
    /// Authenticated user that opened the activity; only they may use it.
    author: String,
    log_message: Option<String>,
    files: BTreeMap<String, Vec<u8>>,
    props: BTreeMap<String, BTreeMap<String, Option<String>>>,
    deletes: BTreeSet<String>,
    /// Revision each touched path was based on (first touch wins).
    bases: BTreeMap<String, i64>,
}

impl TxnActivity {
    fn touched_paths(&self) -> impl Iterator<Item = &String> {
        self.files
            .keys()
            .chain(self.props.keys())
            .chain(self.deletes.iter())
    }
}

/// Who made a request.
#[derive(Debug, Clone)]
struct Identity {
    name: String,
    authenticated: bool,
}

pub fn serve_http(repo_root: PathBuf, bind: &str, options: ServeOptions) -> Result<()> {
    validate_server_config(&repo_root)?;
    check_bind_is_safe(bind, &options)?;

    let data = web::Data::new(AppState {
        repo_root,
        options,
        activities: Mutex::new(HashMap::new()),
        commit_lock: Mutex::new(()),
        auth_cache: Mutex::new(HashSet::new()),
    });

    actix_web::rt::System::new()
        .block_on(async move {
            HttpServer::new(move || {
                App::new()
                    .app_data(data.clone())
                    // Allow request bodies up to the per-activity cap; the
                    // activity accounting bounds total buffered memory.
                    .app_data(web::PayloadConfig::new(MAX_ACTIVITY_BYTES))
                    .route("/{tail:.*}", web::to(svn_entry))
            })
            .bind(bind)?
            .run()
            .await
        })
        .map_err(VcsError::Io)
}

/// Refuse configurations that look protected but are not.
fn validate_server_config(repo_root: &Path) -> Result<()> {
    let vcrs = repo_root.join(".vcrs");
    if !vcrs.exists() {
        return Err(VcsError::RepositoryNotFound);
    }
    let passwd_path = vcrs.join("passwd.json");
    // authz keyed on an unauthenticated identity is meaningless.
    if vcrs.join("authz.json").exists() && !passwd_path.exists() {
        return Err(VcsError::ServerMisconfigured(
            "authz.json requires passwd.json: authorization rules are unenforceable without authentication".to_owned(),
        ));
    }
    if passwd_path.exists() {
        let passwd = load_passwd(&passwd_path)?;
        if let Some(user) = passwd
            .users
            .iter()
            .find(|(_, v)| !is_password_hash(v))
            .map(|(u, _)| u)
        {
            return Err(VcsError::ServerMisconfigured(format!(
                "passwd.json stores a plain-text password for '{user}'; replace it with a hash from `vcrs passwd {user}`"
            )));
        }
    }
    Ok(())
}

/// Plain HTTP is only served on loopback unless explicitly allowed.
fn check_bind_is_safe(bind: &str, options: &ServeOptions) -> Result<()> {
    if options.allow_insecure_http {
        return Ok(());
    }
    let host = bind.rsplit_once(':').map_or(bind, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback {
        Ok(())
    } else {
        Err(VcsError::ServerMisconfigured(format!(
            "refusing to serve plain HTTP on non-loopback address '{host}': credentials and content would travel unencrypted; bind to 127.0.0.1 behind a TLS reverse proxy or pass --allow-insecure-http"
        )))
    }
}

async fn svn_entry(req: HttpRequest, body: web::Bytes, state: web::Data<AppState>) -> HttpResponse {
    let method = req.method().as_str().to_owned();
    let path = req.path().to_owned();

    // Authenticate before doing anything else.
    let identity = match authenticate(&state, &req) {
        Ok(identity) => identity,
        Err(resp) => return resp,
    };
    let user = identity.name.as_str();
    let authz = match Authz::load(&state.repo_root) {
        Ok(authz) => authz,
        Err(err) => return svn_error_response(err),
    };

    if let Some(action) = method_action(&method) {
        if action == Action::Write
            && !identity.authenticated
            && !state.options.allow_anonymous_write
        {
            return HttpResponse::Forbidden().body(
                "anonymous write access is disabled: configure .vcrs/passwd.json or start the server with --allow-anonymous-write",
            );
        }
        // Coarse gate on the request URL; handlers below check the concrete
        // repository paths (protocol URLs under /!svn carry them inside).
        let allowed = if path == "/" || path.starts_with("/!svn") {
            authz.allows_any(user, action)
        } else {
            authz.allows(user, action, &authz_check_path(&path))
        };
        if !allowed {
            return svn_error_response(denied(user, &path, action));
        }
    }

    let youngest = youngest_rev(&state.repo_root).unwrap_or(0);
    let repo_root_header = "/";
    let repo_uuid = format!(
        "vcrs-{}",
        blake3::hash(state.repo_root.to_string_lossy().as_bytes()).to_hex()
    );

    match method.as_str() {
        "OPTIONS" => HttpResponse::Ok()
            .insert_header(("DAV", "1,2"))
            .insert_header(("SVN-Youngest-Rev", youngest.to_string()))
            .insert_header(("SVN-Repository-Root", repo_root_header))
            .insert_header(("SVN-Repository-UUID", repo_uuid))
            .insert_header(("SVN-Me-Resource", "/!svn/me"))
            .insert_header(("MS-Author-Via", "DAV"))
            .insert_header((
                header::ALLOW,
                "OPTIONS, PROPFIND, REPORT, CHECKOUT, MERGE, MKACTIVITY, PROPPATCH, PUT, DELETE, LOCK, UNLOCK, GET, HEAD",
            ))
            .finish(),
        "PROPFIND" => {
            let xml = propfind_response_xml(&path, youngest);
            HttpResponse::build(StatusCode::MULTI_STATUS)
                .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
                .insert_header(("DAV", "1,2"))
                .insert_header(("SVN-Youngest-Rev", youngest.to_string()))
                .insert_header(("SVN-Repository-Root", repo_root_header))
                .insert_header(("SVN-Repository-UUID", repo_uuid))
                .body(xml)
        }
        "REPORT" => {
            let payload = String::from_utf8_lossy(&body);
            // Reports only ever contain what the user may read.
            let xml = if payload.contains("get-latest-rev-report") {
                latest_rev_report_xml(youngest)
            } else if payload.contains("log-report") {
                log_report_response_xml(&state.repo_root, &authz, user)
                    .unwrap_or_else(|_| empty_report_xml())
            } else if payload.contains("update-report") {
                let from_rev = parse_update_target_rev(&body).unwrap_or(youngest);
                update_report_xml(&state.repo_root, from_rev, youngest, &authz, user)
                    .unwrap_or_else(|_| empty_report_xml())
            } else {
                empty_report_xml()
            };
            HttpResponse::Ok()
                .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
                .insert_header(("SVN-Youngest-Rev", youngest.to_string()))
                .insert_header(("SVN-Repository-Root", repo_root_header))
                .insert_header(("SVN-Repository-UUID", repo_uuid))
                .body(xml)
        }
        "MKACTIVITY" => {
            let id = Uuid::new_v4().to_string();
            let activity = TxnActivity {
                author: user.to_owned(),
                ..TxnActivity::default()
            };
            if let Ok(mut map) = state.activities.lock() {
                if map.len() >= MAX_ACTIVITIES {
                    return HttpResponse::ServiceUnavailable().body("too many open activities");
                }
                map.insert(id.clone(), activity);
            }
            HttpResponse::Created()
                .insert_header(("Location", format!("/!svn/act/{id}")))
                .finish()
        }
        "CHECKOUT" => {
            let activity_href = parse_first_tag_text(&body, "href");
            let Some(activity_id) = extract_activity_id(activity_href.as_deref().unwrap_or(""))
            else {
                return svn_error_response(VcsError::Protocol(
                    "missing activity-set href".to_owned(),
                ));
            };
            match state.activities.lock() {
                Ok(map) => match map.get(&activity_id) {
                    Some(a) if a.author == user => {}
                    Some(_) => return foreign_activity(),
                    None => {
                        return svn_error_response(VcsError::Protocol(
                            "unknown activity".to_owned(),
                        ));
                    }
                },
                Err(_) => return HttpResponse::InternalServerError().body("activity lock poisoned"),
            }
            HttpResponse::Created()
                .insert_header(("Location", format!("/!svn/wrk/{activity_id}/")))
                .finish()
        }
        "PUT" | "PROPPATCH" | "DELETE" => {
            let Some((activity_id, rel_path)) = parse_wrk_path(&path) else {
                return svn_error_response(VcsError::PathOutsideRepository(
                    "expected /!svn/wrk/<activity>/<path>".to_owned(),
                ));
            };
            let rel = match sanitize_repo_rel(rel_path) {
                Ok(v) => v,
                Err(e) => return svn_error_response(e),
            };
            if !authz.allows_rel(user, Action::Write, &rel) {
                return svn_error_response(denied(user, &rel, Action::Write));
            }
            let mut map = match state.activities.lock() {
                Ok(m) => m,
                Err(_) => return HttpResponse::InternalServerError().body("activity lock poisoned"),
            };
            let Some(activity) = map.get_mut(&activity_id) else {
                return svn_error_response(VcsError::Protocol("unknown activity".to_owned()));
            };
            if activity.author != user {
                return foreign_activity();
            }
            let base_rev = *activity
                .bases
                .entry(rel.clone())
                .or_insert_with(|| request_base_rev(&req, youngest));
            match method.as_str() {
                "PUT" => put_into_activity(&req, &body, &state.repo_root, activity, rel, base_rev),
                "PROPPATCH" => {
                    // Setting properties supersedes a pending delete.
                    activity.deletes.remove(&rel);
                    let entry = activity.props.entry(rel).or_default();
                    for (k, v) in parse_proppatch(&body) {
                        entry.insert(k, v);
                    }
                    HttpResponse::build(StatusCode::MULTI_STATUS)
                        .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
                        .body(
                            r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:"><D:response><D:status>HTTP/1.1 200 OK</D:status></D:response></D:multistatus>"#,
                        )
                }
                _ => {
                    // A delete supersedes any pending PUT/PROPPATCH.
                    activity.files.remove(&rel);
                    activity.props.remove(&rel);
                    activity.deletes.insert(rel);
                    HttpResponse::NoContent().finish()
                }
            }
        }
        "MERGE" => {
            let activity_href = parse_first_tag_text(&body, "href");
            let Some(activity_id) = extract_activity_id(activity_href.as_deref().unwrap_or(""))
            else {
                return svn_error_response(VcsError::Protocol("missing source href".to_owned()));
            };
            let log_msg = parse_first_tag_text(&body, "log-message").or_else(|| {
                req.headers()
                    .get("SVN-Log")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            });
            let activity = {
                let mut map = match state.activities.lock() {
                    Ok(m) => m,
                    Err(_) => {
                        return HttpResponse::InternalServerError().body("activity lock poisoned");
                    }
                };
                match map.get(&activity_id) {
                    Some(a) if a.author != user => return foreign_activity(),
                    Some(_) => map.remove(&activity_id),
                    None => None,
                }
            };
            let Some(activity) = activity else {
                return svn_error_response(VcsError::Protocol("unknown activity".to_owned()));
            };
            // Re-check every path at commit time (rules may have changed).
            if let Some(path) = activity
                .touched_paths()
                .find(|p| !authz.allows_rel(user, Action::Write, p))
            {
                return svn_error_response(denied(user, path, Action::Write));
            }
            // Apply the commit on a blocking thread, serialized against other
            // commits.
            let state = state.clone();
            let repo_root = state.repo_root.clone();
            let hooks = state.options.enable_hooks;
            let outcome = web::block(move || {
                let _guard = state
                    .commit_lock
                    .lock()
                    .map_err(|_| VcsError::RepositoryNotFound)?;
                apply_activity_commit(&repo_root, activity, log_msg, hooks)
            })
            .await;
            match outcome {
                Ok(Ok(rev)) => HttpResponse::Ok()
                    .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
                    .insert_header(("SVN-Youngest-Rev", rev.to_string()))
                    .body(merge_ok_xml(rev)),
                Ok(Err(err)) => svn_error_response(err),
                Err(_) => HttpResponse::InternalServerError().body("commit task canceled"),
            }
        }
        "LOCK" | "UNLOCK" => {
            let rel = match sanitize_repo_rel(path.trim_start_matches('/')) {
                Ok(v) => v,
                Err(e) => return svn_error_response(e),
            };
            if !authz.allows_rel(user, Action::Write, &rel) {
                return svn_error_response(denied(user, &rel, Action::Write));
            }
            // Repository-level locks live in SQLite: acquisition is atomic,
            // and commits by anyone else are refused while the lock is held.
            let repo = match Repository::discover(&state.repo_root) {
                Ok(r) => r,
                Err(e) => return svn_error_response(e),
            };
            if method == "UNLOCK" {
                return match repo.unlock_path(&rel, user) {
                    Ok(()) => HttpResponse::NoContent().finish(),
                    Err(e) => svn_error_response(e),
                };
            }
            match repo.lock_path(&rel, user) {
                Ok(lock) => HttpResponse::Ok()
                    .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
                    .insert_header(("Lock-Token", format!("<{}>", lock.token)))
                    .body(lock_ok_xml(&rel, &lock.token, user)),
                Err(e) => svn_error_response(e),
            }
        }
        "GET" | "HEAD" => {
            if let Some((rev, rel_path)) = parse_versioned_get_path(&path) {
                let rel = match sanitize_repo_rel(rel_path) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::BadRequest().body("invalid path"),
                };
                if !authz.allows_rel(user, Action::Read, &rel) {
                    return svn_error_response(denied(user, &rel, Action::Read));
                }
                let client = match Client::discover(&state.repo_root) {
                    Ok(c) => c,
                    Err(err) => return HttpResponse::InternalServerError().body(err.to_string()),
                };
                match client.cat_revision_file(&rev.to_string(), &rel) {
                    Ok(bytes) => {
                        let mut res = HttpResponse::Ok();
                        res.insert_header((header::CONTENT_TYPE, "application/octet-stream"));
                        if method == "HEAD" {
                            res.finish()
                        } else {
                            res.body(bytes)
                        }
                    }
                    Err(err) => svn_error_response(err),
                }
            } else {
                HttpResponse::Ok()
                    .insert_header((header::CONTENT_TYPE, "text/plain; charset=\"utf-8\""))
                    .insert_header(("SVN-Youngest-Rev", youngest.to_string()))
                    .body("vcrs-svn-http: SVN/WebDAV compatibility endpoint")
            }
        }
        _ => HttpResponse::build(StatusCode::METHOD_NOT_ALLOWED)
            .insert_header((header::CONTENT_TYPE, "text/plain; charset=\"utf-8\""))
            .body("method not allowed"),
    }
}

/// Buffer a PUT into its activity (full text, or an svndiff against the
/// client's base revision of the file).
fn put_into_activity(
    req: &HttpRequest,
    body: &[u8],
    repo_root: &Path,
    activity: &mut TxnActivity,
    rel: String,
    base_rev: i64,
) -> HttpResponse {
    // Bound the memory a single activity can pin across PUTs. Compute the
    // remaining budget up front so the svndiff expansion below is capped
    // *before* it allocates (the declared target length is attacker-controlled).
    let buffered: usize = activity
        .files
        .iter()
        .filter(|(k, _)| **k != rel)
        .map(|(_, v)| v.len())
        .sum();
    let remaining = MAX_ACTIVITY_BYTES.saturating_sub(buffered);

    let ctype = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let data = if ctype.contains("svndiff") {
        // The delta is relative to the client's base revision, not to
        // whatever HEAD happens to be now.
        let base = load_file_bytes_at(repo_root, &rel, base_rev).unwrap_or_default();
        match apply_svndiff_stream(&base, body, remaining) {
            Ok(v) => v,
            Err(err) => return svn_error_response(err),
        }
    } else {
        body.to_vec()
    };
    if data.len() > remaining {
        return HttpResponse::PayloadTooLarge().body("activity byte limit exceeded");
    }
    // A PUT supersedes a pending delete for the same path (SVN replace).
    activity.deletes.remove(&rel);
    activity.files.insert(rel, data);
    HttpResponse::Created().finish()
}

fn foreign_activity() -> HttpResponse {
    HttpResponse::Forbidden().body("activity belongs to another user")
}

fn denied(user: &str, path: &str, action: Action) -> VcsError {
    VcsError::AuthzDenied {
        user: user.to_owned(),
        path: path.to_owned(),
        action: action.as_str().to_owned(),
    }
}

/// Commit an activity directly into the repository (never through the
/// server's working copy, which may be dirty or stale and must not be left
/// modified by a rejected commit).
fn apply_activity_commit(
    repo_root: &Path,
    activity: TxnActivity,
    log_message: Option<String>,
    hooks: bool,
) -> Result<i64> {
    let mut repo = Repository::discover(repo_root)?;
    repo.set_hooks_enabled(hooks);
    let message = log_message
        .or(activity.log_message)
        .unwrap_or_else(|| "HTTP commit".to_owned());
    let author = if activity.author.trim().is_empty() {
        "anonymous".to_owned()
    } else {
        activity.author
    };
    let edits = TreeEdits {
        puts: activity.files,
        props: activity.props,
        deletes: activity.deletes,
        bases: activity.bases,
    };
    Ok(repo.commit_edits(&edits, &message, &author)?.revision)
}

fn method_action(method: &str) -> Option<Action> {
    match method {
        "GET" | "HEAD" | "PROPFIND" | "REPORT" | "OPTIONS" => Some(Action::Read),
        "MKACTIVITY" | "CHECKOUT" | "PUT" | "PROPPATCH" | "MERGE" | "DELETE" | "LOCK"
        | "UNLOCK" => Some(Action::Write),
        _ => None,
    }
}

fn authz_check_path(req_path: &str) -> String {
    if req_path == "/" || req_path.starts_with("/!svn") {
        return "/".to_owned();
    }
    format!("/{}", req_path.trim_start_matches('/'))
}

#[derive(Debug, Default, serde::Deserialize)]
struct PasswdFile {
    /// Map of username -> Argon2 password hash (created by `vcrs passwd`).
    #[serde(default)]
    users: BTreeMap<String, String>,
}

/// Resolve the request's identity. With `.vcrs/passwd.json`, valid HTTP Basic
/// credentials are required. Without it every request is anonymous: the
/// client-supplied `SVN-UserName` header is never trusted as an identity.
fn authenticate(
    state: &AppState,
    req: &HttpRequest,
) -> std::result::Result<Identity, HttpResponse> {
    let passwd_path = state.repo_root.join(".vcrs").join("passwd.json");
    if !passwd_path.exists() {
        return Ok(Identity {
            name: "anonymous".to_owned(),
            authenticated: false,
        });
    }

    let passwd = match load_passwd(&passwd_path) {
        Ok(p) => p,
        Err(_) => return Err(HttpResponse::InternalServerError().body("invalid passwd file")),
    };
    let Some((user, pass)) = parse_basic_auth(req) else {
        return Err(auth_challenge());
    };
    let Some(stored) = passwd.users.get(&user) else {
        return Err(auth_challenge());
    };
    let key = {
        let mut h = blake3::Hasher::new();
        for part in [user.as_str(), pass.as_str(), stored.as_str()] {
            h.update(part.as_bytes());
            h.update(b"\0");
        }
        h.finalize().to_hex().to_string()
    };
    let cached = state
        .auth_cache
        .lock()
        .is_ok_and(|cache| cache.contains(&key));
    if !cached {
        if !verify_password(stored, &pass) {
            return Err(auth_challenge());
        }
        if let Ok(mut cache) = state.auth_cache.lock() {
            if cache.len() >= AUTH_CACHE_LIMIT {
                cache.clear();
            }
            cache.insert(key);
        }
    }
    Ok(Identity {
        name: user,
        authenticated: true,
    })
}

fn auth_challenge() -> HttpResponse {
    HttpResponse::Unauthorized()
        .insert_header(("WWW-Authenticate", format!("Basic realm=\"{AUTH_REALM}\"")))
        .body("authentication required")
}

fn parse_basic_auth(req: &HttpRequest) -> Option<(String, String)> {
    let header = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let b64 = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_owned(), pass.to_owned()))
}

fn load_passwd(path: &Path) -> Result<PasswdFile> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Read,
    Write,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Write => "write",
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct AuthzFile {
    #[serde(default)]
    users: BTreeMap<String, AuthzRules>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AuthzRules {
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
}

impl AuthzRules {
    fn for_action(&self, action: Action) -> &[String] {
        match action {
            Action::Read => &self.read,
            Action::Write => &self.write,
        }
    }
}

/// Path-prefix access rules from `.vcrs/authz.json` (`/` or `/dir` entries per
/// user and action). Without the file everything is allowed.
#[derive(Debug, Default)]
struct Authz {
    rules: Option<AuthzFile>,
}

impl Authz {
    fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(".vcrs").join("authz.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(Self {
            rules: Some(serde_json::from_slice(&fs::read(path)?)?),
        })
    }

    /// `path` is an absolute repository path (`/dir/file`).
    fn allows(&self, user: &str, action: Action, path: &str) -> bool {
        match &self.rules {
            None => true,
            Some(file) => file
                .users
                .get(user)
                .is_some_and(|r| is_allowed(r.for_action(action), path)),
        }
    }

    /// `rel` is a repository-relative path (`dir/file`).
    fn allows_rel(&self, user: &str, action: Action, rel: &str) -> bool {
        self.allows(user, action, &format!("/{rel}"))
    }

    /// Whether the user has any rule for `action` (gate for protocol URLs
    /// whose concrete paths are checked later).
    fn allows_any(&self, user: &str, action: Action) -> bool {
        match &self.rules {
            None => true,
            Some(file) => file
                .users
                .get(user)
                .is_some_and(|r| !r.for_action(action).is_empty()),
        }
    }
}

fn is_allowed(prefixes: &[String], path: &str) -> bool {
    prefixes
        .iter()
        .any(|p| p == "/" || path == p || path.starts_with(&format!("{p}/")))
}

fn sanitize_repo_rel(input: &str) -> Result<String> {
    // URL-decode first so percent-encoded paths (e.g. "my%20file.txt") map to
    // the real filename, and so "%2e%2e" / "%3a" cannot smuggle ".." or a drive
    // letter past the checks.
    let decoded = percent_decode(input);
    let norm = decoded.trim().trim_matches('/').replace('\\', "/");
    if norm.is_empty() || norm.starts_with("!svn") {
        return Err(VcsError::PathOutsideRepository(input.to_owned()));
    }
    // Any ':' is refused over the wire regardless of the server platform, so a
    // repository served from Linux never accumulates names a Windows client
    // cannot check out (drive prefixes, NTFS streams).
    if norm.contains(':') {
        return Err(VcsError::PathOutsideRepository(input.to_owned()));
    }
    // The shared validator rejects "..", empty components and metadata
    // directories case-insensitively (".VCRS" is ".vcrs" on macOS/Windows),
    // including Windows trailing-dot/space and 8.3 aliases.
    crate::path::validate_rel_path(&norm)?;
    Ok(norm)
}

/// Decode `%XX` escapes in a URL path segment. `+` is left literal (it only
/// means space in query strings, not path components). Invalid escapes are
/// passed through unchanged.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_wrk_path(path: &str) -> Option<(String, &str)> {
    let pref = "/!svn/wrk/";
    let tail = path.strip_prefix(pref)?;
    let mut parts = tail.splitn(2, '/');
    let activity = parts.next()?.to_owned();
    let rel = parts.next().unwrap_or_default();
    Some((activity, rel))
}

fn parse_versioned_get_path(path: &str) -> Option<(i64, &str)> {
    for pref in ["/!svn/bc/", "/!svn/ver/"] {
        if let Some(tail) = path.strip_prefix(pref) {
            let mut parts = tail.splitn(2, '/');
            let rev = parts.next()?.parse::<i64>().ok()?;
            let rel = parts.next().unwrap_or_default();
            return Some((rev, rel));
        }
    }
    None
}

fn extract_activity_id(href: &str) -> Option<String> {
    if href.is_empty() {
        return None;
    }
    if let Some(pos) = href.rfind("/!svn/act/") {
        return Some(
            href[pos + "/!svn/act/".len()..]
                .trim_matches('/')
                .to_owned(),
        );
    }
    href.split('/')
        .next_back()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_first_tag_text(xml: &[u8], wanted_local: &str) -> Option<String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut want_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref()).to_owned();
                want_text = local == wanted_local;
            }
            Ok(Event::Text(e)) if want_text => {
                let s = String::from_utf8_lossy(e.as_ref()).to_string();
                return Some(s);
            }
            Ok(Event::End(_)) => want_text = false,
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

fn parse_proppatch(xml: &[u8]) -> Vec<(String, Option<String>)> {
    #[derive(Copy, Clone, PartialEq, Eq)]
    enum Mode {
        None,
        Set,
        Remove,
    }
    let mut out = Vec::new();
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut mode = Mode::None;
    let mut in_prop = false;
    let mut cur_name: Option<String> = None;
    let mut cur_text = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref()).to_owned();
                match local.as_str() {
                    "set" => mode = Mode::Set,
                    "remove" => mode = Mode::Remove,
                    "prop" => in_prop = true,
                    _ => {
                        if in_prop && mode != Mode::None {
                            cur_name = Some(local.clone());
                            cur_text.clear();
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if cur_name.is_some() {
                    cur_text.push_str(&String::from_utf8_lossy(e.as_ref()));
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref()).to_owned();
                if local == "set" || local == "remove" {
                    mode = Mode::None;
                } else if local == "prop" {
                    in_prop = false;
                } else if let Some(name) = cur_name.clone()
                    && local == name
                {
                    let val = if mode == Mode::Set {
                        Some(cur_text.clone())
                    } else {
                        None
                    };
                    out.push((name, val));
                    cur_name = None;
                    cur_text.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Parse the client's reported base revision from an update-report request.
/// A mixed-revision working copy sends one `<S:entry rev="N">` per path; we take
/// the LOWEST so the response includes every delta any path might still need
/// (over-sending is harmless — the client ignores deltas it already has).
fn parse_update_target_rev(xml: &[u8]) -> Option<i64> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut min_rev: Option<i64> = None;
    let mut record = |num: i64| {
        min_rev = Some(min_rev.map_or(num, |cur| cur.min(num)));
    };
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = local_name(e.name().as_ref()).to_owned();
                if local == "target-revision" || local == "entry" {
                    for attr in e.attributes().flatten() {
                        let k = local_name(attr.key.as_ref());
                        if (k == "rev" || k == "revision")
                            && let Ok(v) = std::str::from_utf8(attr.value.as_ref())
                            && let Ok(num) = v.parse::<i64>()
                        {
                            record(num);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    min_rev
}

fn load_file_bytes_at(repo_root: &Path, rel: &str, rev: i64) -> Option<Vec<u8>> {
    let client = Client::discover(repo_root).ok()?;
    client.cat_revision_file(&rev.to_string(), rel).ok()
}

/// Base revision the client's change to a path is relative to: the
/// `X-SVN-Version-Name` header when present, otherwise HEAD at the time of
/// the request (so changes committed later are still detected at MERGE).
fn request_base_rev(req: &HttpRequest, youngest: i64) -> i64 {
    req.headers()
        .get("X-SVN-Version-Name")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|rev| (0..=youngest).contains(rev))
        .unwrap_or(youngest)
}

fn encode_varint(mut n: u64, out: &mut Vec<u8>) {
    let mut buf = [0u8; 10];
    let mut i = 0usize;
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80;
        }
        buf[i] = b;
        i += 1;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[..i]);
}

fn decode_varint(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let mut shift = 0u32;
    let mut value = 0u64;
    for _ in 0..10 {
        if *pos >= bytes.len() {
            return Err(VcsError::Protocol("truncated svndiff varint".to_owned()));
        }
        let b = bytes[*pos];
        *pos += 1;
        value |= u64::from(b & 0x7f) << shift;
        if (b & 0x80) == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(VcsError::Protocol("invalid svndiff varint".to_owned()))
}

fn encode_svndiff_full(new_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"SVN\0");
    encode_varint(0, &mut out); // source offset
    encode_varint(0, &mut out); // source len
    encode_varint(new_data.len() as u64, &mut out); // target len

    let mut instr = Vec::new();
    let len = new_data.len();
    if len > 0 {
        if len < 64 {
            instr.push((2u8 << 6) | (len as u8));
        } else {
            instr.push(2u8 << 6);
            encode_varint(len as u64, &mut instr);
        }
    }
    encode_varint(instr.len() as u64, &mut out);
    encode_varint(new_data.len() as u64, &mut out);
    out.extend_from_slice(&instr);
    out.extend_from_slice(new_data);
    out
}

/// Decode an svndiff stream, refusing to produce more than `max_total` bytes of
/// output. The declared per-window target length is attacker-controlled, so it
/// is validated against the remaining budget *before* any allocation.
fn apply_svndiff_stream(base: &[u8], data: &[u8], max_total: usize) -> Result<Vec<u8>> {
    if data.len() < 4 || &data[..3] != b"SVN" {
        return Err(VcsError::Protocol("invalid svndiff header".to_owned()));
    }
    let version = data[3];
    if version != 0 && version != 1 {
        return Err(VcsError::Protocol(format!(
            "unsupported svndiff version {version}"
        )));
    }
    let mut pos = 4usize;
    let mut out = Vec::new();
    while pos < data.len() {
        let src_off = decode_varint(data, &mut pos)? as usize;
        let src_len = decode_varint(data, &mut pos)? as usize;
        let tgt_len = decode_varint(data, &mut pos)? as usize;
        let ins_len = decode_varint(data, &mut pos)? as usize;
        let new_len = decode_varint(data, &mut pos)? as usize;
        if tgt_len > max_total.saturating_sub(out.len()) {
            return Err(VcsError::Protocol(
                "svndiff output exceeds activity byte limit".to_owned(),
            ));
        }
        if pos + ins_len + new_len > data.len() {
            return Err(VcsError::Protocol("truncated svndiff window".to_owned()));
        }
        let instructions_raw = &data[pos..pos + ins_len];
        pos += ins_len;
        let new_data_raw = &data[pos..pos + new_len];
        pos += new_len;

        // svndiff1 (version 1) zlib-compresses the instruction and new-data
        // sections; svndiff0 stores them raw. Treating compressed bytes as raw
        // would silently corrupt the file, so decode per version.
        let (instructions, new_data): (Vec<u8>, Vec<u8>) = if version == 1 {
            (
                inflate_section(instructions_raw, max_total)?,
                inflate_section(new_data_raw, max_total)?,
            )
        } else {
            (instructions_raw.to_vec(), new_data_raw.to_vec())
        };

        let source_end = src_off.saturating_add(src_len);
        if source_end > base.len() {
            return Err(VcsError::Protocol("source view out of bounds".to_owned()));
        }
        let source = &base[src_off..source_end];
        let target = apply_svndiff_window(source, &instructions, &new_data, tgt_len)?;
        out.extend_from_slice(&target);
    }
    Ok(out)
}

/// Decode one svndiff1 section: a varint of the original length followed by the
/// payload, which is stored raw when its length equals the original length and
/// zlib-compressed otherwise. `max_out` bounds the decompressed size (zip-bomb
/// guard).
fn inflate_section(section: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut p = 0usize;
    let original_len = decode_varint(section, &mut p)? as usize;
    if original_len > max_out {
        return Err(VcsError::Protocol(
            "svndiff section exceeds activity byte limit".to_owned(),
        ));
    }
    let payload = &section[p..];
    if payload.len() == original_len {
        // Stored uncompressed (compression did not help).
        return Ok(payload.to_vec());
    }
    let mut out = Vec::with_capacity(original_len.min(1 << 20));
    // Cap reads one past the declared size so an over-producing stream is
    // detected as a mismatch rather than allowed to grow unbounded.
    let mut decoder = ZlibDecoder::new(payload).take(original_len as u64 + 1);
    decoder.read_to_end(&mut out)?;
    if out.len() != original_len {
        return Err(VcsError::Protocol(
            "svndiff section size mismatch".to_owned(),
        ));
    }
    Ok(out)
}

fn apply_svndiff_window(
    source: &[u8],
    instructions: &[u8],
    new_data: &[u8],
    target_len: usize,
) -> Result<Vec<u8>> {
    // target_len is validated against the activity budget by the caller, but
    // clamp the up-front reservation anyway so a malformed window cannot request
    // a giant allocation; the Vec grows as needed.
    let mut out = Vec::with_capacity(target_len.min(1 << 20));
    let mut ip = 0usize;
    let mut np = 0usize;
    while ip < instructions.len() {
        let op = instructions[ip];
        ip += 1;
        let opcode = op >> 6;
        let mut len = usize::from(op & 0x3f);
        if len == 0 {
            len = decode_varint(instructions, &mut ip)? as usize;
        }
        match opcode {
            0 => {
                let off = decode_varint(instructions, &mut ip)? as usize;
                let end = off.saturating_add(len);
                if end > source.len() {
                    return Err(VcsError::Protocol("copy-source out of bounds".to_owned()));
                }
                out.extend_from_slice(&source[off..end]);
            }
            1 => {
                let off = decode_varint(instructions, &mut ip)? as usize;
                if off >= out.len() {
                    return Err(VcsError::Protocol("copy-target out of bounds".to_owned()));
                }
                // Support overlap like memmove semantics.
                for i in 0..len {
                    if off + i >= out.len() {
                        return Err(VcsError::Protocol(
                            "copy-target overlap out of bounds".to_owned(),
                        ));
                    }
                    let b = out[off + i];
                    out.push(b);
                }
            }
            2 => {
                let end = np.saturating_add(len);
                if end > new_data.len() {
                    return Err(VcsError::Protocol("copy-new out of bounds".to_owned()));
                }
                out.extend_from_slice(&new_data[np..end]);
                np = end;
            }
            _ => {
                return Err(VcsError::Protocol(
                    "unsupported svndiff instruction".to_owned(),
                ));
            }
        }
    }
    if out.len() != target_len {
        return Err(VcsError::Protocol(format!(
            "svndiff target size mismatch: expected {target_len}, got {}",
            out.len()
        )));
    }
    Ok(out)
}

fn local_name(raw: &[u8]) -> &str {
    let s = std::str::from_utf8(raw).unwrap_or_default();
    s.rsplit(':').next().unwrap_or(s)
}

fn youngest_rev(repo_root: &Path) -> Result<i64> {
    let client = Client::discover(repo_root)?;
    Ok(client.log(1)?.first().map(|c| c.revision).unwrap_or(0))
}

fn propfind_response_xml(path: &str, youngest: i64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:S="svn:" xmlns:V="http://subversion.tigris.org/xmlns/dav/">
  <D:response>
    <D:href>{}</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <V:baseline-relative-path></V:baseline-relative-path>
        <S:youngest-rev>{}</S:youngest-rev>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#,
        xml_escape(path),
        youngest
    )
}

fn latest_rev_report_xml(youngest: i64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><S:get-latest-rev-report xmlns:S="svn:"><S:youngest-rev>{}</S:youngest-rev></S:get-latest-rev-report>"#,
        youngest
    )
}

fn update_report_xml(
    repo_root: &Path,
    from_rev: i64,
    youngest: i64,
    authz: &Authz,
    user: &str,
) -> Result<String> {
    let client = Client::discover(repo_root)?;
    if from_rev >= youngest {
        return Ok(format!(
            r#"<?xml version="1.0" encoding="utf-8"?><S:update-report xmlns:S="svn:"><S:target-revision rev="{}"/></S:update-report>"#,
            youngest
        ));
    }

    let mut entries = String::new();
    for rev in (from_rev + 1)..=youngest {
        let changed = match client.changed_paths_in_revision(&rev.to_string()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for cp in changed {
            // Paths the user may not read are never sent.
            if !authz.allows_rel(user, Action::Read, &cp.path) {
                continue;
            }
            match cp.action {
                ChangedPathAction::Delete => {
                    entries.push_str(&format!(
                        r#"<S:delete-entry name="{}" rev="{}"/>"#,
                        xml_escape(&cp.path),
                        rev
                    ));
                }
                ChangedPathAction::Add | ChangedPathAction::Modify | ChangedPathAction::Replace => {
                    let bytes = match client.cat_revision_file(&rev.to_string(), &cp.path) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let svndiff = encode_svndiff_full(&bytes);
                    let delta_b64 = base64::engine::general_purpose::STANDARD.encode(svndiff);
                    // A newly added/replaced path must be reported as add-file;
                    // open-file targets an existing node the client already has.
                    let is_add = matches!(
                        cp.action,
                        ChangedPathAction::Add | ChangedPathAction::Replace
                    );
                    let name = xml_escape(&cp.path);
                    if is_add {
                        entries.push_str(&format!(
                            r#"<S:add-file name="{name}"><S:apply-textdelta/><S:txdelta>{delta_b64}</S:txdelta><S:close-file/></S:add-file>"#
                        ));
                    } else {
                        entries.push_str(&format!(
                            r#"<S:open-file name="{name}" rev="{}"><S:apply-textdelta/><S:txdelta>{delta_b64}</S:txdelta><S:close-file/></S:open-file>"#,
                            rev.saturating_sub(1)
                        ));
                    }
                }
            }
        }
    }

    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<S:update-report xmlns:S="svn:" xmlns:D="DAV:">
  <S:target-revision rev="{}"/>
  <S:open-root rev="{}"/>
  {}
  <S:close-root/>
</S:update-report>"#,
        youngest, from_rev, entries
    ))
}

fn log_report_response_xml(repo_root: &Path, authz: &Authz, user: &str) -> Result<String> {
    let client = Client::discover(repo_root)?;
    let commits = client.log(20)?;
    let mut items = String::new();
    let reads_root = authz.allows(user, Action::Read, "/");
    for c in commits {
        // A revision is listed only if the user may read something it changed.
        if !reads_root
            && !c
                .changed_paths
                .iter()
                .any(|cp| authz.allows_rel(user, Action::Read, &cp.path))
        {
            continue;
        }
        items.push_str(&format!(
            r#"<S:log-item><D:version-name>{}</D:version-name><D:creator-displayname>{}</D:creator-displayname><S:date>{}</S:date><D:comment>{}</D:comment></S:log-item>"#,
            c.revision,
            xml_escape(&c.author),
            xml_escape(&c.created_at.to_rfc3339()),
            xml_escape(&c.message)
        ));
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<S:log-report xmlns:S="svn:" xmlns:D="DAV:">{}</S:log-report>"#,
        items
    ))
}

fn merge_ok_xml(new_rev: i64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:merge-response xmlns:D="DAV:" xmlns:S="svn:">
  <D:updated-set>
    <D:response>
      <D:href>/</D:href>
      <S:new-rev>{}</S:new-rev>
    </D:response>
  </D:updated-set>
</D:merge-response>"#,
        new_rev
    )
}

fn lock_ok_xml(path: &str, token: &str, owner: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:prop xmlns:D="DAV:">
  <D:lockdiscovery>
    <D:activelock>
      <D:locktype><D:write/></D:locktype>
      <D:lockscope><D:exclusive/></D:lockscope>
      <D:depth>Infinity</D:depth>
      <D:owner><D:href>{}</D:href></D:owner>
      <D:locktoken><D:href>{}</D:href></D:locktoken>
      <D:lockroot><D:href>{}</D:href></D:lockroot>
    </D:activelock>
  </D:lockdiscovery>
</D:prop>"#,
        xml_escape(owner),
        xml_escape(token),
        xml_escape(path)
    )
}

fn empty_report_xml() -> String {
    r#"<?xml version="1.0" encoding="utf-8"?><S:report xmlns:S="svn:"/>"#.to_owned()
}

fn xml_escape(input: &str) -> String {
    // Escape the full entity set so the result is safe in both element text and
    // double-quoted attribute values (e.g. name="..." in update-report). Control
    // characters illegal in XML 1.0 are dropped (they cannot be represented even
    // as numeric refs) so arbitrary stored content can't produce malformed XML.
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

fn svn_error_response(err: VcsError) -> HttpResponse {
    let (status, code) = match &err {
        VcsError::AuthzDenied { .. } => (StatusCode::FORBIDDEN, "E170001"),
        VcsError::LockConflict { .. } | VcsError::NeedsLockRequired { .. } => {
            (StatusCode::LOCKED, "E155004")
        }
        VcsError::OutOfDate { .. } => (StatusCode::CONFLICT, "E160024"),
        VcsError::PathOutsideRepository(_)
        | VcsError::Protocol(_)
        | VcsError::RevisionNotFound(_) => (StatusCode::BAD_REQUEST, "E200009"),
        VcsError::CommitNotFound(_) | VcsError::BlobNotFound(_) => {
            (StatusCode::NOT_FOUND, "E160013")
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "E000000"),
    };
    let xml = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:error xmlns:D="DAV:" xmlns:m="http://apache.org/dav/xmlns">
  <m:human-readable errcode="{}">{}</m:human-readable>
</D:error>"#,
        code,
        xml_escape(&err.to_string())
    );
    HttpResponse::build(status)
        .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
        .body(xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svndiff_roundtrip_full() {
        let data = b"hello world, this is some content\nwith two lines\n";
        let encoded = encode_svndiff_full(data);
        let out = apply_svndiff_stream(&[], &encoded, 1 << 20).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn svndiff_empty_roundtrip() {
        let encoded = encode_svndiff_full(b"");
        let out = apply_svndiff_stream(&[], &encoded, 1 << 20).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn svndiff_rejects_oversized_target_before_allocating() {
        // A tiny body that declares a ~1 TiB target length must be rejected by
        // the budget check, not turned into a giant allocation.
        let mut body = Vec::new();
        body.extend_from_slice(b"SVN\0");
        encode_varint(0, &mut body); // source offset
        encode_varint(0, &mut body); // source len
        encode_varint(1u64 << 40, &mut body); // target len (~1 TiB)
        encode_varint(0, &mut body); // instructions len
        encode_varint(0, &mut body); // new data len
        let res = apply_svndiff_stream(&[], &body, 1024);
        assert!(res.is_err(), "oversized target must be rejected");
    }

    #[test]
    fn svndiff_respects_exact_budget() {
        let data = b"1234567890"; // 10 bytes
        let encoded = encode_svndiff_full(data);
        // Exactly enough budget succeeds.
        assert!(apply_svndiff_stream(&[], &encoded, 10).is_ok());
        // One byte short fails.
        assert!(apply_svndiff_stream(&[], &encoded, 9).is_err());
    }

    fn section_v1(orig: &[u8]) -> Vec<u8> {
        use flate2::{Compression, write::ZlibEncoder};
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(orig).unwrap();
        let comp = enc.finish().unwrap();
        let mut section = Vec::new();
        encode_varint(orig.len() as u64, &mut section);
        // Store raw when compression did not shrink it (matches svndiff1 rules).
        if comp.len() < orig.len() {
            section.extend_from_slice(&comp);
        } else {
            section.extend_from_slice(orig);
        }
        section
    }

    fn build_svndiff1(target: &[u8]) -> Vec<u8> {
        // Single window, single "copy from new data" (opcode 2) instruction.
        let mut instr = Vec::new();
        let l = target.len();
        if l < 64 {
            instr.push((2u8 << 6) | l as u8);
        } else {
            instr.push(2u8 << 6);
            encode_varint(l as u64, &mut instr);
        }
        let instr_section = section_v1(&instr);
        let new_section = section_v1(target);
        let mut out = Vec::new();
        out.extend_from_slice(b"SVN\x01");
        encode_varint(0, &mut out); // source offset
        encode_varint(0, &mut out); // source len
        encode_varint(l as u64, &mut out); // target len
        encode_varint(instr_section.len() as u64, &mut out);
        encode_varint(new_section.len() as u64, &mut out);
        out.extend_from_slice(&instr_section);
        out.extend_from_slice(&new_section);
        out
    }

    #[test]
    fn svndiff1_compressed_roundtrip() {
        // Highly repetitive content so the new-data section actually compresses.
        let data = "abc".repeat(500).into_bytes();
        let stream = build_svndiff1(&data);
        let out = apply_svndiff_stream(&[], &stream, 1 << 20).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn svndiff1_small_raw_section_roundtrip() {
        // Tiny content stores sections raw (zlib would be larger).
        let data = b"hello";
        let stream = build_svndiff1(data);
        let out = apply_svndiff_stream(&[], &stream, 1 << 20).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode("my%20file.txt"), "my file.txt");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("plain"), "plain");
        // `+` stays literal in path components.
        assert_eq!(percent_decode("a+b"), "a+b");
        // Malformed escape is passed through.
        assert_eq!(percent_decode("100%done"), "100%done");
    }

    #[test]
    fn sanitize_blocks_encoded_traversal() {
        // "%2e%2e" must not smuggle ".." past the guard.
        assert!(sanitize_repo_rel("foo/%2e%2e/etc").is_err());
        assert_eq!(
            sanitize_repo_rel("dir/my%20file.txt").unwrap(),
            "dir/my file.txt"
        );
    }

    #[test]
    fn sanitize_blocks_drive_and_absolute() {
        // Windows drive prefixes (incl. percent-encoded colon) and mid-path
        // drive letters must be rejected — PathBuf::push would escape the repo.
        assert!(sanitize_repo_rel("C:/Windows/evil.txt").is_err());
        assert!(sanitize_repo_rel("foo/C:/evil.txt").is_err());
        assert!(sanitize_repo_rel("C:evil.txt").is_err());
        assert!(sanitize_repo_rel("C%3A/Windows/evil.txt").is_err());
        assert!(sanitize_repo_rel("file:stream").is_err());
        // A normal nested path still works.
        assert_eq!(sanitize_repo_rel("a/b/c.txt").unwrap(), "a/b/c.txt");
    }

    #[test]
    fn sanitize_blocks_metadata_directories_in_any_case() {
        for bad in [
            ".vcrs/hooks/pre-commit",
            ".VCRS/hooks/pre-commit",
            ".Vcrs./hooks/x",
            "%2eVCRS/x",
            "sub/.vcrs/x",
            ".git/hooks/post-checkout",
            ".GIT/config",
            "VCRS~1/x",
        ] {
            assert!(sanitize_repo_rel(bad).is_err(), "{bad} must be rejected");
        }
        // Ordinary dot-files that merely start with the name stay allowed.
        assert_eq!(sanitize_repo_rel(".vcrsignore").unwrap(), ".vcrsignore");
    }

    #[test]
    fn join_repo_path_rejects_escaping_components() {
        let root = Path::new("/repo/root");
        assert!(crate::path::safe_join(root, "a/b.txt").is_ok());
        assert!(crate::path::safe_join(root, "../escape").is_err());
        assert!(crate::path::safe_join(root, "C:/Windows").is_err());
        assert!(crate::path::safe_join(root, "foo/C:/x").is_err());
    }

    #[test]
    fn xml_escape_drops_illegal_control_chars() {
        // Bare control chars (here a NUL and a 0x01) are dropped; tab/newline kept.
        let dirty = "a\u{0}b\u{1}c\td\ne";
        let clean = xml_escape(dirty);
        assert_eq!(clean, "abc\td\ne");
        assert!(!clean.contains('\u{0}'));
    }

    #[test]
    fn xml_escape_covers_attribute_chars() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn http_commit_bypasses_the_server_working_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let client = Client::init(root).unwrap();
        fs::write(root.join("a.txt"), "a\n").unwrap();
        client.add(&["a.txt".to_owned()]).unwrap();
        client.commit("r1", "local").unwrap();
        // The server's own working copy is dirty.
        fs::write(root.join("a.txt"), "local edit\n").unwrap();

        let mut activity = TxnActivity {
            author: "alice".to_owned(),
            ..TxnActivity::default()
        };
        activity
            .files
            .insert("b.txt".to_owned(), b"remote\n".to_vec());
        let rev = apply_activity_commit(root, activity, Some("remote".into()), false).unwrap();
        assert_eq!(rev, 2);
        assert!(
            !root.join("b.txt").exists(),
            "nothing written to the working copy"
        );
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "local edit\n"
        );
        assert_eq!(client.cat_revision_file("2", "b.txt").unwrap(), b"remote\n");
        assert_eq!(client.cat_revision_file("2", "a.txt").unwrap(), b"a\n");

        // A rejected commit leaves no trace and later commits still work.
        let mut bad = TxnActivity::default();
        bad.props.insert(
            "missing.txt".to_owned(),
            BTreeMap::from([("x".to_owned(), Some("1".to_owned()))]),
        );
        assert!(apply_activity_commit(root, bad, None, false).is_err());
        let mut next = TxnActivity::default();
        next.deletes.insert("b.txt".to_owned());
        assert_eq!(apply_activity_commit(root, next, None, false).unwrap(), 3);
    }

    #[test]
    fn stale_http_edits_are_rejected_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let client = Client::init(root).unwrap();
        fs::write(root.join("a.txt"), "v1\n").unwrap();
        client.add(&["a.txt".to_owned()]).unwrap();
        client.commit("r1", "local").unwrap();

        // Bob commits r2 on top of r1.
        let mut bob = TxnActivity::default();
        bob.files.insert("a.txt".to_owned(), b"bob\n".to_vec());
        bob.bases.insert("a.txt".to_owned(), 1);
        assert_eq!(apply_activity_commit(root, bob, None, false).unwrap(), 2);

        // Alice still edits the r1 version: her commit must not silently
        // overwrite Bob's change.
        let mut alice = TxnActivity::default();
        alice.files.insert("a.txt".to_owned(), b"alice\n".to_vec());
        alice.bases.insert("a.txt".to_owned(), 1);
        let err = apply_activity_commit(root, alice, None, false).unwrap_err();
        assert!(matches!(err, VcsError::OutOfDate { .. }), "{err}");
        assert_eq!(client.cat_revision_file("HEAD", "a.txt").unwrap(), b"bob\n");

        // Based on r2 it goes through.
        let mut alice = TxnActivity::default();
        alice.files.insert("a.txt".to_owned(), b"alice\n".to_vec());
        alice.bases.insert("a.txt".to_owned(), 2);
        assert_eq!(apply_activity_commit(root, alice, None, false).unwrap(), 3);
    }

    #[test]
    fn plain_http_is_loopback_only_unless_allowed() {
        let safe = ServeOptions::default();
        for ok in ["127.0.0.1:3690", "localhost:80", "[::1]:3690"] {
            assert!(check_bind_is_safe(ok, &safe).is_ok(), "{ok}");
        }
        for bad in [
            "0.0.0.0:3690",
            "10.0.0.5:80",
            "example.org:3690",
            "[::]:3690",
        ] {
            assert!(check_bind_is_safe(bad, &safe).is_err(), "{bad}");
        }
        let insecure = ServeOptions {
            allow_insecure_http: true,
            ..ServeOptions::default()
        };
        assert!(check_bind_is_safe("0.0.0.0:3690", &insecure).is_ok());
    }

    #[test]
    fn plain_text_passwords_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        Client::init(dir.path()).unwrap();
        let passwd = dir.path().join(".vcrs/passwd.json");
        fs::write(&passwd, r#"{"users":{"alice":"secret"}}"#).unwrap();
        assert!(validate_server_config(dir.path()).is_err());
        let hash = crate::auth::hash_password("secret").unwrap();
        fs::write(
            &passwd,
            serde_json::json!({"users": {"alice": hash}}).to_string(),
        )
        .unwrap();
        assert!(validate_server_config(dir.path()).is_ok());
    }

    #[test]
    fn authz_is_checked_per_path() {
        let authz = Authz {
            rules: Some(
                serde_json::from_str(
                    r#"{"users":{"alice":{"read":["/pub"],"write":["/pub/docs"]}}}"#,
                )
                .unwrap(),
            ),
        };
        assert!(authz.allows_rel("alice", Action::Read, "pub/a.txt"));
        assert!(!authz.allows_rel("alice", Action::Read, "secret/a.txt"));
        assert!(!authz.allows_rel("alice", Action::Read, "public/a.txt"));
        assert!(authz.allows_rel("alice", Action::Write, "pub/docs/x"));
        assert!(!authz.allows_rel("alice", Action::Write, "pub/x"));
        assert!(authz.allows_any("alice", Action::Write));
        assert!(!authz.allows_any("bob", Action::Read));
        assert!(Authz::default().allows_rel("anyone", Action::Write, "x"));
    }
}
