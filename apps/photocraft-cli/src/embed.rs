//! `photocraft-cli embed`: the app as a live session with no window, for hosts that show PhotoCraft
//! inside their own UI (Creative Suite). The UI runs off-screen
//! ([`photocraft_ui_egui::embedded::Embedded`], drawn on the CPU; the canvas uses its CPU path) and
//! answers the same token-authenticated JSON-lines control protocol as `photocraft --control` on
//! 127.0.0.1:<PORT>. Nothing appears on the screen and nothing takes focus.

use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use photocraft_automation::AuthorizedWorkspace;
use photocraft_automation::budgets::write_reply;
use photocraft_automation::security::{
    AUTH_DEADLINE, AuthDeadlineStream, ConnectionLimiter, LineRead, MAX_CONNECTIONS, MAX_REQUEST_BYTES, authentication_reply, configure_stream, read_bounded_line,
    repeat_auth_reply, server_token,
};
use photocraft_doc::Document;
use photocraft_ui_egui::control::REPLY_DEADLINE;
use photocraft_ui_egui::embedded::Embedded;
use photocraft_ui_egui::ExportSettings;
use serde_json::{Value, json};

use crate::args::EmbedArgs;
use crate::{CliError, Result};

pub(crate) fn embed(a: &EmbedArgs, err: &mut dyn Write) -> Result {
    let token = server_token(a.control_token.control_token.as_deref(), a.control_token.control_token_file.as_deref())?;
    // As the desktop app: the workspace always confines the control channel; without a root that
    // direction of access is denied.
    let workspace = a.automation.workspace()?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", a.control)).map_err(|e| CliError::Other(format!("127.0.0.1:{}: {e}", a.control)))?;
    let bound = listener.local_addr().map_or(a.control, |addr| addr.port());
    let app = photocraft_ui_egui::PhotocraftApp::new(photocraft_engine::Session::new(), services(workspace));
    let mut session = Embedded::new(app, [a.size.0, a.size.1], a.scale);
    let _ = writeln!(err, "photocraft-cli embed: control channel on 127.0.0.1:{bound}");
    // Connections are read on their own threads (authenticated first, as the app's own server
    // does); the session answers on this one, between frames.
    let (tx, rx) = mpsc::channel::<(String, Value, mpsc::Sender<Value>)>();
    std::thread::Builder::new().name("embed-accept".into()).spawn(move || accept(listener, token, tx))?;
    let timeout = REPLY_DEADLINE;
    loop {
        let start = Instant::now();
        while let Ok((method, params, reply)) = rx.try_recv() {
            let _ = reply.send(session.request(&method, params, timeout)); // the client may have gone
        }
        if session.quit_requested() {
            break;
        }
        session.step();
        // ~30 frames a second while something moves, a few when idle.
        let frame = if session.busy() { Duration::from_millis(33) } else { Duration::from_millis(100) };
        if let Some(rest) = frame.checked_sub(start.elapsed()) {
            match rx.recv_timeout(rest) {
                Ok((method, params, reply)) => {
                    let _ = reply.send(session.request(&method, params, timeout));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    session.on_exit();
    Ok(())
}

type Request = (String, Value, mpsc::Sender<Value>);

fn accept(listener: std::net::TcpListener, token: String, tx: mpsc::Sender<Request>) {
    let limiter = ConnectionLimiter::new(MAX_CONNECTIONS);
    let token = std::sync::Arc::new(token);
    for mut stream in listener.incoming().flatten() {
        let Some(permit) = limiter.try_acquire() else {
            let _ = configure_stream(&stream);
            let _ = writeln!(stream, "{}", json!({"id": null, "ok": false, "error": "connection limit reached"}));
            continue;
        };
        let (tx, token) = (tx.clone(), std::sync::Arc::clone(&token));
        let _ = std::thread::Builder::new().name("embed-conn".into()).spawn(move || {
            let _permit = permit;
            serve(stream, &token, &tx);
        });
    }
}

/// One connection, as the desktop app's control server does it: bounded lines, the
/// authentication deadline, one reply per request.
fn serve(stream: TcpStream, token: &str, tx: &mpsc::Sender<Request>) {
    if configure_stream(&stream).is_err() {
        return;
    }
    let Ok(read) = stream.try_clone() else { return };
    let mut reader = BufReader::new(AuthDeadlineStream::new(read, AUTH_DEADLINE));
    let mut out = stream;
    let mut line = String::new();
    let mut authenticated = false;
    loop {
        match read_bounded_line(&mut reader, &mut line) {
            Ok(LineRead::Eof) | Err(_) => break,
            Ok(LineRead::TooLong) => {
                let _ = write_reply(&mut out, &json!({"id": null, "ok": false, "error": format!("request exceeds {MAX_REQUEST_BYTES} bytes")}));
                break;
            }
            Ok(LineRead::Line) if line.trim().is_empty() => continue,
            Ok(LineRead::Line) => {}
        }
        if !authenticated {
            let (reply, ok) = authentication_reply(&line, token);
            authenticated = ok;
            if ok && reader.get_mut().authenticated().is_err() {
                break;
            }
            if write_reply(&mut out, &reply).is_err() || out.flush().is_err() || !authenticated {
                break;
            }
            continue;
        }
        if let Some(reply) = repeat_auth_reply(&line) {
            if write_reply(&mut out, &reply).is_err() || out.flush().is_err() {
                break;
            }
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let (rtx, rrx) = mpsc::channel();
                if tx.send((method, params, rtx)).is_err() {
                    break;
                }
                let mut r = rrx.recv_timeout(REPLY_DEADLINE + Duration::from_secs(5)).unwrap_or_else(|_| json!({"ok": false, "error": "timed out"}));
                if let Some(o) = r.as_object_mut() {
                    o.insert("id".into(), id);
                }
                r
            }
            Err(e) => json!({"id": null, "ok": false, "error": format!("bad JSON: {e}")}),
        };
        if write_reply(&mut out, &reply).is_err() || out.flush().is_err() {
            break;
        }
    }
}

/// The desktop app's services minus everything that needs a window or the person's desktop (file
/// dialogs, clipboard, crash recovery, updates): open, export, write, and control-channel file
/// access confined to the automation workspace.
fn services(workspace: AuthorizedWorkspace) -> photocraft_ui_egui::Services {
    let read = workspace.clone();
    let automation_read = Box::new(move |path: &str| {
        let bytes = read.read(path).map_err(|error| error.to_string())?;
        let name = Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path).to_string();
        Ok((name, bytes))
    }) as photocraft_ui_egui::AutomationReadFn;
    let write = workspace.clone();
    let automation_write = Box::new(move |path: &str, bytes: &[u8]| write.write(path, bytes).map_err(|error| error.to_string())) as photocraft_ui_egui::AutomationWriteFn;
    let pcraft = workspace.clone();
    let automation_open_pcraft = Box::new(move |path: &str| {
        let name = Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path).to_string();
        Ok(pcraft.open_pcraft(path).map_err(|e| e.to_string())?.map(|doc| (name, doc)))
    }) as photocraft_ui_egui::AutomationOpenPcraftFn;
    let save = workspace;
    let automation_save = std::sync::Arc::new(move |doc: &Document, path: &str, settings: &ExportSettings, ctl: &photocraft_ui_egui::jobs_ui::SaveCtl| {
        ctl.progress(0.0, "Encoding");
        let cancelled = || ctl.check().is_err();
        let progress = |f: f32| ctl.progress(f * 0.85, "");
        let interrupt = photocraft_raster::Interrupt::new(&cancelled, &progress);
        let r = photocraft_io::encode_with(doc, path, &settings.io_options(), &interrupt).map_err(|e| e.to_string())?;
        ctl.commit()?;
        ctl.progress(0.85, "Writing");
        save.write_stream(path, &mut |w| r.encoded.write_to(w)).map_err(|e| e.to_string())?;
        ctl.progress(1.0, "");
        Ok(r.warnings)
    }) as photocraft_ui_egui::SaveFileFn;
    let step: fn(&str, &Value) -> photocraft_engine::Result<()> = photocraft_automation::workspace::authorize_desktop_engine_step;
    let automation_command = Box::new(|id: &str, params: &Value| {
        photocraft_automation::workspace::authorize_desktop_engine_command(id, params).map_err(|error| error.to_string())
    }) as photocraft_ui_egui::AutomationCommandFn;
    photocraft_ui_egui::Services {
        import: Some(Box::new(|name: &str, bytes: &[u8], opts: &photocraft_io::ImportOptions| {
            photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
        })),
        export: Some(Box::new(|doc: &Document, path: &str, settings: &ExportSettings| {
            photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
        })),
        write: Some(Box::new(|path: &str, bytes: &[u8]| photocraft_format::atomic_write(Path::new(path), bytes).map_err(|e| e.to_string()))),
        automation_read: Some(automation_read),
        automation_write: Some(automation_write),
        automation_open_pcraft: Some(automation_open_pcraft),
        automation_save: Some(automation_save),
        automation_command: Some(automation_command),
        automation_authorize: Some(step),
        encode_png: Some(Box::new(|w, h, rgba| {
            let img = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgba, rgba.to_vec()).map_err(|e| e.to_string())?;
            photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &photocraft_codecs::EncodeOptions::default()).map_err(|e| e.to_string())
        })),
        ..Default::default()
    }
}
