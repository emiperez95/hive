//! `/api/box/<id>[/pack|/unpack|/mark]` — the HTTP half of `hive box push|pull`.
//!
//! Off unless `[web] accept_boxes = true`: these write files, and the dashboard has no
//! authentication (it is a trusted-LAN surface). What bounds the damage is not a secret but
//! the rules `boxes::unpack` enforces on every transport — the target must sit inside a
//! registered project, a box id/profile/slug is a plain name, and nothing local that exists
//! only here is overwritten.
//!
//! Each request runs on its own thread: the web server's request loop is serial, and a
//! transfer or an unpack (git, rsync, project hooks) would otherwise freeze the dashboard.

use tiny_http::{Header, Method, Request, Response};

use crate::common::boxes;
use crate::common::config::HiveConfig;

pub const PREFIX: &str = "/api/box/";

fn json(code: u16, v: serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(v.to_string())
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
        .with_status_code(code)
}

/// `/api/box/<id>/<action>` → (id, action).
pub fn parse(path: &str) -> Option<(&str, Option<&str>)> {
    let rest = path.strip_prefix(PREFIX)?;
    let (id, action) = match rest.split_once('/') {
        Some((i, a)) => (i, Some(a)),
        None => (rest, None),
    };
    boxes::valid_id(id).then_some((id, action))
}

pub fn handle(mut request: Request) {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let method = request.method().clone();

    if !HiveConfig::load().web.accept_boxes {
        let _ = request.respond(json(
            403,
            serde_json::json!({
                "error": "this server does not accept boxes; set `accept_boxes = true` under [web]"
            }),
        ));
        return;
    }
    let Some((id, action)) = parse(&path) else {
        let _ = request.respond(json(400, serde_json::json!({ "error": "bad box id" })));
        return;
    };
    let id = id.to_string();

    // Streaming responses are handled here; everything else returns a JSON result.
    if method == Method::Get && action.is_none() {
        match tar_for_download(&id) {
            Ok(file) => {
                let _ = request.respond(Response::from_file(file));
            }
            Err(e) => {
                let _ =
                    request.respond(json(404, serde_json::json!({ "error": format!("{e:#}") })));
            }
        }
        return;
    }

    let result: anyhow::Result<serde_json::Value> = (|| match (&method, action) {
        (Method::Put, None) => receive(&id, &mut request),
        (Method::Post, Some(a)) => {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            let req: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let flag = |k: &str| req.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            match a {
                "pack" => {
                    let (cid, _, _) = crate::cli::boxes::pack_conversation(
                        &id,
                        flag("force"),
                        !flag("without_ignored"),
                    )?;
                    Ok(serde_json::json!({ "ok": true, "id": cid }))
                }
                "unpack" => {
                    crate::cli::boxes::run_box_unpack(
                        &id,
                        flag("thaw"),
                        flag("force"),
                        flag("sandbox"),
                    )?;
                    Ok(serde_json::json!({ "ok": true }))
                }
                "mark" => {
                    let loc = req.get("location").and_then(|v| v.as_str());
                    crate::cli::boxes::run_box_mark(&id, loc, flag("remove_worktree"))?;
                    Ok(serde_json::json!({ "ok": true }))
                }
                _ => anyhow::bail!("unknown box action '{a}'"),
            }
        }
        _ => anyhow::bail!("unsupported request"),
    })();

    let _ = request.respond(match result {
        Ok(v) => json(200, v),
        Err(e) => json(400, serde_json::json!({ "error": format!("{e:#}") })),
    });
}

/// PUT: extract into scratch, validate, install.
fn receive(id: &str, request: &mut Request) -> anyhow::Result<serde_json::Value> {
    let scratch = boxes::boxes_dir()?.join(format!("{id}.incoming-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let r = (|| {
        boxes::untar_box(request.as_reader(), &scratch)?;
        boxes::install_received_box(&scratch, id)
    })();
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    r?;
    Ok(serde_json::json!({ "ok": true }))
}

/// GET: tar the box to a temp file and hand back a handle to stream from.
fn tar_for_download(id: &str) -> anyhow::Result<std::fs::File> {
    let dir = boxes::boxes_dir()?.join(id);
    boxes::read_manifest(&dir)?;
    let tmp = std::env::temp_dir().join(format!("hive-box-{id}-{}.tar", std::process::id()));
    boxes::tar_box(&dir, &tmp)?;
    let f = std::fs::File::open(&tmp)?;
    let _ = std::fs::remove_file(&tmp); // the open handle keeps the data
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_plain_ids_and_actions_only() {
        assert_eq!(parse("/api/box/abc-123"), Some(("abc-123", None)));
        assert_eq!(
            parse("/api/box/abc-123/unpack"),
            Some(("abc-123", Some("unpack")))
        );
        assert_eq!(parse("/api/box/../etc"), None);
        assert_eq!(parse("/api/box/a%2Fb"), None);
        assert_eq!(parse("/api/box/"), None);
        assert_eq!(parse("/api/other/abc"), None);
    }
}
