use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use actix_web::http::{StatusCode, header};
use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use base64::Engine;
use quick_xml::Reader;
use quick_xml::events::Event;
use uuid::Uuid;

use crate::client::Client;
use crate::error::{Result, VcsError};
use crate::types::ChangedPathAction;

#[derive(Debug)]
struct AppState {
    repo_root: PathBuf,
    activities: Mutex<HashMap<String, TxnActivity>>,
}

#[derive(Debug, Clone, Default)]
struct TxnActivity {
    author: String,
    log_message: Option<String>,
    files: BTreeMap<String, Vec<u8>>,
    props: BTreeMap<String, BTreeMap<String, Option<String>>>,
}

pub fn serve_http(repo_root: PathBuf, bind: &str) -> Result<()> {
    if !repo_root.join(".vcrs").exists() {
        return Err(VcsError::RepositoryNotFound);
    }

    let data = web::Data::new(AppState {
        repo_root,
        activities: Mutex::new(HashMap::new()),
    });

    actix_web::rt::System::new()
        .block_on(async move {
            HttpServer::new(move || {
                App::new()
                    .app_data(data.clone())
                    .route("/{tail:.*}", web::to(svn_entry))
            })
            .bind(bind)?
            .run()
            .await
        })
        .map_err(VcsError::Io)
}

async fn svn_entry(req: HttpRequest, body: web::Bytes, state: web::Data<AppState>) -> HttpResponse {
    let method = req.method().as_str().to_owned();
    let path = req.path().to_owned();
    let username = req
        .headers()
        .get("SVN-UserName")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous")
        .to_owned();

    if let Some(action) = method_action(&method) {
        let authz_path = authz_check_path(&path);
        if let Err(err) = authorize(&state.repo_root, &username, action, &authz_path) {
            return svn_error_response(err);
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
                "OPTIONS, PROPFIND, REPORT, CHECKOUT, MERGE, MKACTIVITY, PROPPATCH, PUT, LOCK, UNLOCK, GET, HEAD",
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
            let xml = if payload.contains("get-latest-rev-report") {
                latest_rev_report_xml(youngest)
            } else if payload.contains("log-report") {
                log_report_response_xml(&state.repo_root).unwrap_or_else(|_| empty_report_xml())
            } else if payload.contains("update-report") {
                let from_rev = parse_update_target_rev(&body).unwrap_or(youngest);
                update_report_xml(&state.repo_root, from_rev, youngest)
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
            let author = req
                .headers()
                .get("SVN-UserName")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("anonymous")
                .to_owned();
            let activity = TxnActivity {
                author,
                ..TxnActivity::default()
            };
            if let Ok(mut map) = state.activities.lock() {
                map.insert(id.clone(), activity);
            }
            HttpResponse::Created()
                .insert_header(("Location", format!("/!svn/act/{id}")))
                .finish()
        }
        "CHECKOUT" => {
            let activity_href = parse_first_tag_text(&body, "href");
            let Some(activity_id) = extract_activity_id(activity_href.as_deref().unwrap_or("")) else {
                return svn_error_response(VcsError::RevisionNotFound(
                    "missing activity-set href".to_owned(),
                ));
            };
            if let Ok(map) = state.activities.lock()
                && !map.contains_key(&activity_id)
            {
                return svn_error_response(VcsError::RevisionNotFound("unknown activity".to_owned()));
            }
            HttpResponse::Created()
                .insert_header(("Location", format!("/!svn/wrk/{activity_id}/")))
                .finish()
        }
        "PUT" => {
            let Some((activity_id, rel_path)) = parse_wrk_path(&path) else {
                return svn_error_response(VcsError::PathOutsideRepository(
                    "expected /!svn/wrk/<activity>/<path>".to_owned(),
                ));
            };
            let rel = match sanitize_repo_rel(rel_path) {
                Ok(v) => v,
                Err(e) => return svn_error_response(e),
            };
            let mut map = match state.activities.lock() {
                Ok(m) => m,
                Err(_) => {
                    return HttpResponse::InternalServerError()
                        .body("activity lock poisoned")
                }
            };
            let Some(activity) = map.get_mut(&activity_id) else {
                return svn_error_response(VcsError::RevisionNotFound("unknown activity".to_owned()));
            };

            let ctype = req
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            let data = if ctype.contains("svndiff") {
                let base = load_head_file_bytes(&state.repo_root, &rel).unwrap_or_default();
                match apply_svndiff_stream(&base, &body) {
                    Ok(v) => v,
                    Err(err) => return svn_error_response(err),
                }
            } else {
                body.to_vec()
            };
            activity.files.insert(rel, data);
            HttpResponse::Created().finish()
        }
        "PROPPATCH" => {
            let Some((activity_id, rel_path)) = parse_wrk_path(&path) else {
                return svn_error_response(VcsError::PathOutsideRepository(
                    "expected /!svn/wrk/<activity>/<path>".to_owned(),
                ));
            };
            let rel = match sanitize_repo_rel(rel_path) {
                Ok(v) => v,
                Err(e) => return svn_error_response(e),
            };
            let ops = parse_proppatch(&body);
            let mut map = match state.activities.lock() {
                Ok(m) => m,
                Err(_) => {
                    return HttpResponse::InternalServerError()
                        .body("activity lock poisoned")
                }
            };
            let Some(activity) = map.get_mut(&activity_id) else {
                return svn_error_response(VcsError::RevisionNotFound("unknown activity".to_owned()));
            };
            let entry = activity.props.entry(rel).or_default();
            for (k, v) in ops {
                entry.insert(k, v);
            }
            HttpResponse::build(StatusCode::MULTI_STATUS)
                .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
                .body(
                    r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:"><D:response><D:status>HTTP/1.1 200 OK</D:status></D:response></D:multistatus>"#,
                )
        }
        "MERGE" => {
            let activity_href = parse_first_tag_text(&body, "href");
            let Some(activity_id) = extract_activity_id(activity_href.as_deref().unwrap_or("")) else {
                return svn_error_response(VcsError::RevisionNotFound("missing source href".to_owned()));
            };
            let log_msg = parse_first_tag_text(&body, "log-message")
                .or_else(|| req.headers().get("SVN-Log").and_then(|v| v.to_str().ok()).map(str::to_owned));
            let activity = {
                let mut map = match state.activities.lock() {
                    Ok(m) => m,
                    Err(_) => {
                        return HttpResponse::InternalServerError().body("activity lock poisoned")
                    }
                };
                map.remove(&activity_id)
            };
            let Some(activity) = activity else {
                return svn_error_response(VcsError::RevisionNotFound("unknown activity".to_owned()));
            };
            match apply_activity_commit(&state.repo_root, activity, log_msg) {
                Ok(rev) => HttpResponse::Ok()
                    .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
                    .insert_header(("SVN-Youngest-Rev", rev.to_string()))
                    .body(merge_ok_xml(rev)),
                Err(err) => svn_error_response(err),
            }
        }
        "LOCK" => {
            let rel = match sanitize_repo_rel(path.trim_start_matches('/')) {
                Ok(v) => v,
                Err(e) => return svn_error_response(e),
            };
            let lock_path = state.repo_root.join(".vcrs").join("locks.json");
            let mut locks = match load_locks(&lock_path) {
                Ok(v) => v,
                Err(e) => return svn_error_response(e),
            };
            if let Some(owner) = locks.get(&rel)
                && owner != &username
            {
                return svn_error_response(VcsError::LockConflict {
                    path: rel,
                    owner: owner.clone(),
                });
            }
            locks.insert(rel.clone(), username.clone());
            if let Err(e) = save_locks(&lock_path, &locks) {
                return svn_error_response(e);
            }
            let token = format!("opaquelocktoken:vcrs:{}:{}", username, rel);
            HttpResponse::Ok()
                .insert_header((header::CONTENT_TYPE, "text/xml; charset=\"utf-8\""))
                .insert_header(("Lock-Token", format!("<{token}>")))
                .body(lock_ok_xml(&rel, &token, &username))
        }
        "UNLOCK" => {
            let rel = match sanitize_repo_rel(path.trim_start_matches('/')) {
                Ok(v) => v,
                Err(e) => return svn_error_response(e),
            };
            let lock_path = state.repo_root.join(".vcrs").join("locks.json");
            let mut locks = match load_locks(&lock_path) {
                Ok(v) => v,
                Err(e) => return svn_error_response(e),
            };
            if let Some(owner) = locks.get(&rel)
                && owner != &username
            {
                return svn_error_response(VcsError::LockConflict {
                    path: rel,
                    owner: owner.clone(),
                });
            }
            locks.remove(&rel);
            if let Err(e) = save_locks(&lock_path, &locks) {
                return svn_error_response(e);
            }
            HttpResponse::NoContent().finish()
        }
        "GET" | "HEAD" => {
            if let Some((rev, rel_path)) = parse_versioned_get_path(&path) {
                let rel = match sanitize_repo_rel(rel_path) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::BadRequest().body("invalid path"),
                };
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

fn apply_activity_commit(
    repo_root: &Path,
    activity: TxnActivity,
    log_message: Option<String>,
) -> Result<i64> {
    let client = Client::discover(repo_root)?;
    if !client.status()?.is_empty() {
        return Err(VcsError::WorkingCopyDirty);
    }

    for (rel, bytes) in &activity.files {
        let abs = join_repo_path(repo_root, rel)?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(abs, bytes)?;
    }
    for (rel, props) in &activity.props {
        for (name, value) in props {
            match value {
                Some(v) => client.set_property(rel, name, v)?,
                None => client.del_property(rel, name)?,
            }
        }
    }

    let message = log_message
        .or(activity.log_message)
        .unwrap_or_else(|| "HTTP commit".to_owned());
    let author = if activity.author.trim().is_empty() {
        "anonymous".to_owned()
    } else {
        activity.author
    };
    let commit = client.commit(&message, &author)?;
    Ok(commit.revision)
}

fn method_action(method: &str) -> Option<Action> {
    match method {
        "GET" | "HEAD" | "PROPFIND" | "REPORT" | "OPTIONS" => Some(Action::Read),
        "MKACTIVITY" | "CHECKOUT" | "PUT" | "PROPPATCH" | "MERGE" | "LOCK" | "UNLOCK" => {
            Some(Action::Write)
        }
        _ => None,
    }
}

fn authz_check_path(req_path: &str) -> String {
    if req_path == "/" || req_path.starts_with("/!svn") {
        return "/".to_owned();
    }
    format!("/{}", req_path.trim_start_matches('/'))
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Read,
    Write,
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

fn authorize(repo_root: &Path, user: &str, action: Action, path: &str) -> Result<()> {
    let authz_path = repo_root.join(".vcrs").join("authz.json");
    if !authz_path.exists() {
        return Ok(());
    }
    let bytes = fs::read(authz_path)?;
    let authz: AuthzFile = serde_json::from_slice(&bytes)?;
    let Some(rules) = authz.users.get(user) else {
        return Err(VcsError::AuthzDenied {
            user: user.to_owned(),
            path: path.to_owned(),
            action: match action {
                Action::Read => "read".to_owned(),
                Action::Write => "write".to_owned(),
            },
        });
    };
    let allow = match action {
        Action::Read => is_allowed(&rules.read, path),
        Action::Write => is_allowed(&rules.write, path),
    };
    if allow {
        Ok(())
    } else {
        Err(VcsError::AuthzDenied {
            user: user.to_owned(),
            path: path.to_owned(),
            action: match action {
                Action::Read => "read".to_owned(),
                Action::Write => "write".to_owned(),
            },
        })
    }
}

fn is_allowed(prefixes: &[String], path: &str) -> bool {
    prefixes
        .iter()
        .any(|p| p == "/" || path == p || path.starts_with(&format!("{p}/")))
}

fn load_locks(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_locks(path: &Path, locks: &BTreeMap<String, String>) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(locks)?)?;
    Ok(())
}

fn join_repo_path(repo_root: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = repo_root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(VcsError::PathOutsideRepository(rel.to_owned()));
        }
        out.push(part);
    }
    Ok(out)
}

fn sanitize_repo_rel(input: &str) -> Result<String> {
    let norm = input.trim().trim_matches('/').replace('\\', "/");
    if norm.is_empty() {
        return Err(VcsError::PathOutsideRepository(input.to_owned()));
    }
    if norm.starts_with(".vcrs") || norm.starts_with("!svn") {
        return Err(VcsError::PathOutsideRepository(input.to_owned()));
    }
    if norm.split('/').any(|p| p == "..") {
        return Err(VcsError::PathOutsideRepository(input.to_owned()));
    }
    Ok(norm)
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

fn parse_update_target_rev(xml: &[u8]) -> Option<i64> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref()).to_owned();
                if local == "target-revision" || local == "entry" {
                    for attr in e.attributes().flatten() {
                        let k = local_name(attr.key.as_ref());
                        if k == "rev" || k == "revision" {
                            if let Ok(v) = std::str::from_utf8(attr.value.as_ref())
                                && let Ok(num) = v.parse::<i64>()
                            {
                                return Some(num);
                            }
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
    None
}

fn load_head_file_bytes(repo_root: &Path, rel: &str) -> Option<Vec<u8>> {
    let client = Client::discover(repo_root).ok()?;
    client.cat_revision_file("HEAD", rel).ok()
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
            return Err(VcsError::RevisionNotFound(
                "truncated svndiff varint".to_owned(),
            ));
        }
        let b = bytes[*pos];
        *pos += 1;
        value |= u64::from(b & 0x7f) << shift;
        if (b & 0x80) == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(VcsError::RevisionNotFound(
        "invalid svndiff varint".to_owned(),
    ))
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

fn apply_svndiff_stream(base: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 4 || &data[..3] != b"SVN" {
        return Err(VcsError::RevisionNotFound(
            "invalid svndiff header".to_owned(),
        ));
    }
    let version = data[3];
    if version != 0 && version != 1 {
        return Err(VcsError::RevisionNotFound(format!(
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
        if pos + ins_len + new_len > data.len() {
            return Err(VcsError::RevisionNotFound(
                "truncated svndiff window".to_owned(),
            ));
        }
        let instructions = &data[pos..pos + ins_len];
        pos += ins_len;
        let new_data = &data[pos..pos + new_len];
        pos += new_len;

        let source_end = src_off.saturating_add(src_len);
        if source_end > base.len() {
            return Err(VcsError::RevisionNotFound(
                "source view out of bounds".to_owned(),
            ));
        }
        let source = &base[src_off..source_end];
        let target = apply_svndiff_window(source, instructions, new_data, tgt_len)?;
        out.extend_from_slice(&target);
    }
    Ok(out)
}

fn apply_svndiff_window(
    source: &[u8],
    instructions: &[u8],
    new_data: &[u8],
    target_len: usize,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(target_len);
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
                    return Err(VcsError::RevisionNotFound(
                        "copy-source out of bounds".to_owned(),
                    ));
                }
                out.extend_from_slice(&source[off..end]);
            }
            1 => {
                let off = decode_varint(instructions, &mut ip)? as usize;
                if off >= out.len() {
                    return Err(VcsError::RevisionNotFound(
                        "copy-target out of bounds".to_owned(),
                    ));
                }
                // Support overlap like memmove semantics.
                for i in 0..len {
                    if off + i >= out.len() {
                        return Err(VcsError::RevisionNotFound(
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
                    return Err(VcsError::RevisionNotFound(
                        "copy-new out of bounds".to_owned(),
                    ));
                }
                out.extend_from_slice(&new_data[np..end]);
                np = end;
            }
            _ => {
                return Err(VcsError::RevisionNotFound(
                    "unsupported svndiff instruction".to_owned(),
                ));
            }
        }
    }
    if out.len() != target_len {
        return Err(VcsError::RevisionNotFound(format!(
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

fn update_report_xml(repo_root: &Path, from_rev: i64, youngest: i64) -> Result<String> {
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
                    entries.push_str(&format!(
                        r#"<S:open-file name="{}" rev="{}"><S:apply-textdelta/><S:txdelta>{}</S:txdelta><S:close-file/></S:open-file>"#,
                        xml_escape(&cp.path),
                        rev.saturating_sub(1),
                        delta_b64
                    ));
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

fn log_report_response_xml(repo_root: &Path) -> Result<String> {
    let client = Client::discover(repo_root)?;
    let commits = client.log(20)?;
    let mut items = String::new();
    for c in commits {
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
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn svn_error_response(err: VcsError) -> HttpResponse {
    let (status, code) = match &err {
        VcsError::AuthzDenied { .. } => (StatusCode::FORBIDDEN, "E170001"),
        VcsError::LockConflict { .. } | VcsError::NeedsLockRequired { .. } => {
            (StatusCode::LOCKED, "E155004")
        }
        VcsError::OutOfDate { .. } => (StatusCode::CONFLICT, "E160024"),
        VcsError::PathOutsideRepository(_) | VcsError::RevisionNotFound(_) => {
            (StatusCode::BAD_REQUEST, "E200009")
        }
        VcsError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "E160013"),
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
