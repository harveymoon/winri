//! tiny_http worker thread that turns HTTP requests into either snapshot
//! reads or commands sent to the main loop.

use std::{io::Cursor, io::Write as _, sync::Arc, thread, time::Duration};

use tiny_http::{Header, Method, Request, Response, Server};

use crate::{
    api::{
        ApiCommand, NamedAction, ScrollRequest, build_state_response, capture, current_state,
        events, send_message,
        types::{ErrorBody, MoveToMonitorRequest, ResizeRequest},
    },
    app::Message,
    config,
};

/// Start the HTTP server on a background thread if the API is enabled in
/// config. Idempotent — calling twice is harmless but will warn.
pub fn launch() {
    let (enabled, bind, port) = {
        let cfg = config::current();
        (cfg.api.enabled, cfg.api.bind.clone(), cfg.api.port)
    };
    if !enabled {
        log::info!("Control API disabled (set api.enabled=true in config.toml to enable)");
        return;
    }

    let addr = format!("{bind}:{port}");
    log::info!("Starting control API on http://{addr}");

    let server = match Server::http(&addr) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log::error!("Failed to bind control API to {addr}: {e}");
            return;
        }
    };

    thread::Builder::new()
        .name("winri-api".into())
        .spawn(move || run(&server))
        .expect("spawn winri-api thread");
}

fn run(server: &Server) {
    for request in server.incoming_requests() {
        if let Err(e) = handle_request(request) {
            log::warn!("API request handling error: {e:#}");
        }
    }
}

fn handle_request(request: Request) -> anyhow::Result<()> {
    let method = request.method().clone();
    let url = request.url().to_string();
    log::debug!("API: {method} {url}");

    let (path_with_slash, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (url.as_str(), None),
    };
    let path = path_with_slash.trim_end_matches('/');

    match (method, path) {
        (Method::Get, "" | "/") => respond_text(request, "winri control API; see /state and /windows"),
        (Method::Get, "/state") => respond_state(request),
        (Method::Get, "/windows") => respond_windows(request),
        (Method::Get, "/monitors") => respond_monitors(request),
        (Method::Get, "/events") => handle_events(request),
        (Method::Post, path) if path.starts_with("/windows/")
            && path.ends_with("/move-to-monitor") =>
        {
            let id_str =
                &path["/windows/".len()..path.len() - "/move-to-monitor".len()];
            handle_move_to_monitor(request, id_str)
        }
        (Method::Post, path) if path.starts_with("/windows/") && path.ends_with("/resize") => {
            let id_str = &path["/windows/".len()..path.len() - "/resize".len()];
            handle_resize(request, id_str)
        }
        (Method::Get, path) if path.starts_with("/windows/") && path.ends_with("/thumbnail") => {
            let id_str = &path["/windows/".len()..path.len() - "/thumbnail".len()];
            let max_width = query.as_deref().and_then(parse_width_param);
            handle_thumbnail(request, id_str, max_width)
        }
        (Method::Post, path) if path.starts_with("/focus/") => {
            let id_str = &path["/focus/".len()..];
            handle_focus(request, id_str)
        }
        (Method::Post, "/scroll") => handle_scroll(request),
        (Method::Post, path) if path.starts_with("/action/") => {
            let name = &path["/action/".len()..];
            handle_action(request, name)
        }
        _ => respond_error(request, 404, "no such endpoint"),
    }
}

/// Parses `?w=NNN` (also accepts `?width=NNN`) from a query string. Returns
/// `None` for any malformed or zero value — that just falls back to native
/// resolution rather than 400-erroring on a thumbnail request.
fn parse_width_param(query: &str) -> Option<u32> {
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        if key == "w" || key == "width" {
            if let Ok(n) = value.parse::<u32>()
                && n > 0
            {
                return Some(n);
            }
        }
    }
    None
}

fn respond_text(request: Request, body: &str) -> anyhow::Result<()> {
    let response = Response::from_string(body).with_header(content_type("text/plain"));
    request.respond(response)?;
    Ok(())
}

fn respond_json<T: serde::Serialize>(request: Request, status: u16, body: &T) -> anyhow::Result<()> {
    let json = serde_json::to_vec(body)?;
    let response = Response::from_data(json)
        .with_status_code(status)
        .with_header(content_type("application/json"));
    request.respond(response)?;
    Ok(())
}

fn respond_error(request: Request, status: u16, msg: &str) -> anyhow::Result<()> {
    respond_json(
        request,
        status,
        &ErrorBody {
            error: msg.to_string(),
        },
    )
}

fn content_type(value: &'static str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).expect("static header")
}

fn respond_state(request: Request) -> anyhow::Result<()> {
    let state = current_state();
    let response = build_state_response(&state);
    respond_json(request, 200, &response)
}

fn respond_windows(request: Request) -> anyhow::Result<()> {
    let state = current_state();
    let response = build_state_response(&state);
    respond_json(request, 200, &response.windows)
}

fn respond_monitors(request: Request) -> anyhow::Result<()> {
    let state = current_state();
    let response = build_state_response(&state);
    respond_json(request, 200, &response.monitors)
}

/// SSE handler — hands the raw TCP writer to a dedicated thread that
/// writes HTTP headers + an initial state snapshot, then forwards every
/// subsequent event from the subscriber channel as a `data: <json>\n\n`
/// line. Returns immediately so the main request loop can keep accepting
/// new connections.
fn handle_events(request: Request) -> anyhow::Result<()> {
    thread::Builder::new()
        .name("winri-api-sse".into())
        .spawn(move || {
            if let Err(e) = run_sse_connection(request) {
                log::debug!("SSE connection closed: {e:#}");
            }
        })?;
    Ok(())
}

fn run_sse_connection(request: Request) -> anyhow::Result<()> {
    let receiver = events::subscribe();
    let mut writer = request.into_writer();

    // Manually write the HTTP response head — `tiny_http`'s normal path
    // would set Content-Length, which is wrong for an open-ended stream.
    let head = b"HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Cache-Control: no-cache, no-transform\r\n\
                 Connection: keep-alive\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 X-Accel-Buffering: no\r\n\
                 \r\n";
    writer.write_all(head)?;
    writer.flush()?;

    // Send the current state immediately so subscribers don't have to
    // wait for the next tiler tick to populate their UI.
    {
        let state = current_state();
        let response = build_state_response(&state);
        drop(state);
        if let Ok(json) = serde_json::to_string(&response) {
            write_sse_event(&mut writer, &json)?;
        }
    }

    // Drain the per-subscriber channel until the client disconnects (a
    // write to the now-dead socket errors and we exit). A periodic
    // comment frame (":\n\n") doubles as a keepalive — many HTTP
    // intermediaries drop idle connections after ~60s, and a comment
    // doesn't trigger a client-side message handler.
    loop {
        match receiver.recv_timeout(Duration::from_secs(20)) {
            Ok(json) => write_sse_event(&mut writer, &json)?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                writer.write_all(b":keepalive\n\n")?;
                writer.flush()?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn write_sse_event(writer: &mut dyn std::io::Write, json: &str) -> std::io::Result<()> {
    writer.write_all(b"data: ")?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n\n")?;
    writer.flush()
}

fn handle_move_to_monitor(mut request: Request, id_str: &str) -> anyhow::Result<()> {
    let hwnd: u64 = match id_str.parse() {
        Ok(v) => v,
        Err(_) => return respond_error(request, 400, "invalid window id"),
    };
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).ok();
    let req: MoveToMonitorRequest = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return respond_error(request, 400, &format!("invalid JSON: {e}")),
    };
    dispatch_command(
        request,
        ApiCommand::MoveToMonitor {
            hwnd,
            device_name: req.device_name,
        },
    )
}

fn handle_resize(mut request: Request, id_str: &str) -> anyhow::Result<()> {
    let hwnd: u64 = match id_str.parse() {
        Ok(v) => v,
        Err(_) => return respond_error(request, 400, "invalid window id"),
    };
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).ok();
    let req: ResizeRequest = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return respond_error(request, 400, &format!("invalid JSON: {e}")),
    };
    if !req.width.is_finite() || req.width <= 0.0 {
        return respond_error(request, 400, "`width` must be a positive finite number");
    }
    dispatch_command(
        request,
        ApiCommand::ResizeWindow {
            hwnd,
            target_width: req.width,
            animate_ms: req.animate_ms,
            center: req.center,
        },
    )
}

fn handle_thumbnail(
    request: Request,
    id_str: &str,
    max_width: Option<u32>,
) -> anyhow::Result<()> {
    let id: u64 = match id_str.parse() {
        Ok(v) => v,
        Err(_) => return respond_error(request, 400, "invalid window id"),
    };
    match capture::capture_window_png(id, max_width) {
        Ok(bytes) => {
            let len = bytes.len();
            let response = Response::new(
                200.into(),
                vec![content_type("image/png")],
                Cursor::new(bytes),
                Some(len),
                None,
            );
            request.respond(response)?;
            Ok(())
        }
        Err(e) => respond_error(request, 500, &format!("capture failed: {e:#}")),
    }
}

fn handle_focus(request: Request, id_str: &str) -> anyhow::Result<()> {
    let id: u64 = match id_str.parse() {
        Ok(v) => v,
        Err(_) => return respond_error(request, 400, "invalid window id"),
    };
    dispatch_command(request, ApiCommand::Focus(id))
}

fn handle_scroll(mut request: Request) -> anyhow::Result<()> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).ok();

    let req: ScrollRequest = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return respond_error(request, 400, &format!("invalid JSON: {e}")),
    };

    let command = match (req.offset, req.delta) {
        (Some(offset), _) => ApiCommand::SetScrollOffset {
            offset,
            animate_ms: req.animate_ms,
        },
        (None, Some(delta)) => ApiCommand::ScrollBy {
            delta,
            animate_ms: req.animate_ms,
        },
        (None, None) => {
            return respond_error(request, 400, "must provide `offset` or `delta`");
        }
    };
    dispatch_command(request, command)
}

fn handle_action(request: Request, name: &str) -> anyhow::Result<()> {
    let action = match name {
        "focus-prev" => NamedAction::FocusPrev,
        "focus-next" => NamedAction::FocusNext,
        "swap-prev" => NamedAction::SwapPrev,
        "swap-next" => NamedAction::SwapNext,
        "resize-fullscreen" => NamedAction::ResizeFullscreen,
        "resize-halfscreen" => NamedAction::ResizeHalfscreen,
        "width-increment" => NamedAction::WidthIncrement,
        "width-decrement" => NamedAction::WidthDecrement,
        "refresh" => NamedAction::Refresh,
        "center-focused" => NamedAction::CenterFocused,
        "open-overview" => NamedAction::OpenOverview,
        "close-overview" => NamedAction::CloseOverview,
        "open-settings" => NamedAction::OpenSettings,
        "exit" => NamedAction::Exit,
        other => {
            return respond_error(request, 400, &format!("unknown action `{other}`"));
        }
    };
    dispatch_command(request, ApiCommand::Action(action))
}

/// Forward a command to the main loop. Responds 202 Accepted on success.
fn dispatch_command(request: Request, command: ApiCommand) -> anyhow::Result<()> {
    match send_message(Message::Api(command)) {
        Ok(()) => respond_json(request, 202, &serde_json::json!({ "ok": true })),
        Err(e) => respond_error(request, 503, e),
    }
}
