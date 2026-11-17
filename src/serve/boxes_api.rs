//! `/api/box/<id>[/pack|/unpack|/mark]` — the HTTP half of `hive box push|pull`.
//!
//! Off unless `[web] accept_boxes = true`: these write files, and the dashboard has no
//! authentication (it is a trusted-LAN surface). What bounds the damage is not a secret but
//! the rules `boxes::unpack` enforces on every transport — the target must sit inside a
//! registered project, a box id/profile/slug is a plain name, and nothing local that exists
//! only here is overwritten.
//!
//! `mark` with `remove_worktree` and `pack` are the two requests that take something away
//! (a working copy; a running window). Both require the WHOLE id — never the CLI's unique
//! prefix — and the removal is refused unless the request also says where the conversation
//! went and this machine's copy is still what the box holds (`check_unchanged_since_pack`).
//! `unpack` never honours `force` from the network, so it cannot replace a transcript that
//! has turns the box lacks.
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
            // A body we cannot read is refused, never read as "all flags off": for `mark`
            // that would mean "no location", i.e. claim the conversation back for this
            // machine while it runs on another.
            let req = parse_body(&body)?;
            let flag = |k: &str| req.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            match a {
                "pack" => {
                    let (cid, _, _) = crate::cli::boxes::pack_conversation(
                        &id,
                        flag("force"),
                        !flag("without_ignored"),
                        true,
                    )?;
                    Ok(serde_json::json!({ "ok": true, "id": cid }))
                }
                "unpack" | "mark" if !crate::cli::boxes::box_exists_exactly(&id) => {
                    anyhow::bail!("no box with id '{id}' (the full id is required)")
                }
                "unpack" => {
                    // No `force` from another machine: it is what lets an unpack replace a
                    // transcript longer than the box's, i.e. turns that exist only here.
                    let warning =
                        crate::cli::boxes::unpack_box(&id, flag("thaw"), false, flag("sandbox"))?;
                    Ok(match warning {
                        Some(w) => {
                            serde_json::json!({ "ok": true, "resumed": false, "warning": w })
                        }
                        None => serde_json::json!({ "ok": true }),
                    })
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

/// The JSON object an action is posted with. Anything else — empty, malformed, an array —
/// is an error.
fn parse_body(body: &str) -> anyhow::Result<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) if v.is_object() => Ok(v),
        _ => anyhow::bail!("bad request body: expected a JSON object"),
    }
}

/// PUT: extract into scratch, validate, install.
fn receive(id: &str, request: &mut Request) -> anyhow::Result<serde_json::Value> {
    boxes::receive_box(request.as_reader(), id)?;
    Ok(serde_json::json!({ "ok": true }))
}

/// GET: tar the box to a temp file and hand back a handle to stream from.
fn tar_for_download(id: &str) -> anyhow::Result<std::fs::File> {
    let dir = boxes::boxes_dir()?.join(id);
    boxes::read_manifest(&dir)?;
    // Unique per request (see `scratch_path`): two downloads of one box must not share it.
    let tmp = boxes::scratch_path(id, "outgoing")?.with_extension("tar");
    boxes::tar_box(&dir, &tmp)?;
    let f = std::fs::File::open(&tmp)?;
    let _ = std::fs::remove_file(&tmp); // the open handle keeps the data
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_that_is_not_a_json_object_is_refused_not_read_as_defaults() {
        for bad in ["", "not json", "[]", "null", "{\"location\":"] {
            assert!(parse_body(bad).is_err(), "{bad:?}");
        }
        let ok = parse_body(r#"{"location":"mini","remove_worktree":true}"#).unwrap();
        assert_eq!(ok["location"], "mini");
        assert!(parse_body("{}").is_ok());
    }

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
