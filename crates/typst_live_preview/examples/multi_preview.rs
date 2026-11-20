//! Checks whether starting a second preview moves the first one's document.
//!
//! Tinymist marks the first browsing preview as *primary*, and a primary preview
//! follows the server's main file, so a later `tinymist.focusMain` for another
//! file re-renders the first preview. `--not-primary` should pin every
//! preview to the document it was started for.
//!
//! ```sh
//! cargo run -p typst_live_preview --example multi_preview -- /path/to/tinymist
//! cargo run -p typst_live_preview --example multi_preview -- /path/to/tinymist --not-primary
//! ```

mod common;

use std::{
    env,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};

use anyhow::{Context as _, Result, bail};
use common::{LspServer, REQUEST_TIMEOUT};
use serde_json::json;
use typst_live_preview::{
    PreviewSession,
    connection::{self, PreviewConnectionConfig, PreviewEvent},
    protocol::{PreviewDataFrame, preview_update_from_bytes, split_preview_data_frame},
};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let tinymist = args.next().unwrap_or_else(|| "tinymist".to_owned());
    let not_primary = args.any(|arg| arg == "--not-primary");

    let root = env::temp_dir().join(format!("suzuri-multi-preview-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    let first = root.join("first.typ");
    let second = root.join("second.typ");
    std::fs::write(&first, "= First\n#pagebreak()\n= First 2\n#pagebreak()\n= First 3\n")?;
    std::fs::write(
        &second,
        "= Second\n#pagebreak()\n= Second 2\n#pagebreak()\n= Second 3\n#pagebreak()\n= Second 4\n#pagebreak()\n= Second 5\n",
    )?;

    println!(
        "mode: {}",
        if not_primary {
            "--not-primary"
        } else {
            "default"
        }
    );

    let root_path = root.to_str().context("temporary directory path")?;
    let mut server = LspServer::spawn(&tinymist, root_path)?;
    server.request(
        1,
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": format!("file://{root_path}"),
            "capabilities": {},
            "workspaceFolders": [{ "uri": format!("file://{root_path}"), "name": "suzuri" }],
        }),
    )?;
    server.notify("initialized", json!({}));
    open_document(&mut server, &first)?;
    open_document(&mut server, &second)?;

    let first_port = start_preview(&mut server, &first, "task-first", not_primary, 10)?;
    let first_before = page_count(first_port)?;
    println!("first preview: {first_before} pages");

    let second_port = start_preview(&mut server, &second, "task-second", not_primary, 20)?;
    let second_pages = page_count(second_port)?;
    println!("second preview: {second_pages} pages");

    let first_after = page_count(first_port)?;
    println!("first preview after the second started: {first_after} pages");

    server.shutdown();
    std::fs::remove_dir_all(&root).ok();

    if first_after == first_before {
        println!("OK: the first preview stayed on its own document");
        Ok(())
    } else {
        bail!("the first preview changed document: {first_before} pages became {first_after}")
    }
}

fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn open_document(server: &mut LspServer, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    server.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri(path),
                "languageId": "typst",
                "version": 1,
                "text": text,
            }
        }),
    );
    Ok(())
}

fn start_preview(
    server: &mut LspServer,
    path: &Path,
    task_id: &str,
    not_primary: bool,
    request_id: i64,
) -> Result<u16> {
    let path = path.display().to_string();
    server.request(
        request_id,
        "workspace/executeCommand",
        json!({
            "command": "tinymist.focusMain",
            "arguments": [path],
        }),
    )?;

    let mut arguments = vec![
        PathBuf::from(&path).display().to_string(),
        "--data-plane-host=127.0.0.1:0".to_owned(),
        "--control-plane-host=127.0.0.1:0".to_owned(),
        "--no-open".to_owned(),
        format!("--task-id={task_id}"),
    ];
    if not_primary {
        arguments.push("--not-primary".to_owned());
    }

    let response = server.request(
        request_id + 1,
        "workspace/executeCommand",
        json!({
            "command": "tinymist.doStartBrowsingPreview",
            "arguments": [arguments],
        }),
    )?;
    println!(
        "tinymist marked this preview primary: {}",
        response
            .pointer("/result/isPrimary")
            .and_then(|primary| primary.as_bool())
            .unwrap_or(false)
    );
    response
        .pointer("/result/dataPlanePort")
        .or_else(|| response.pointer("/dataPlanePort"))
        .and_then(|port| port.as_u64())
        .and_then(|port| u16::try_from(port).ok())
        .with_context(|| format!("no dataPlanePort in {response}"))
}

/// Connects to `port`, reads the first full frame, and returns its page count.
fn page_count(port: u16) -> Result<usize> {
    let (events_tx, events_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let connection_stop = stop.clone();
    let (_outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        if let Err(error) = connection::run(
            PreviewConnectionConfig::loopback(port),
            events_tx,
            outbound_rx,
            connection_stop,
        ) {
            eprintln!("connection stopped: {error}");
        }
    });

    let frame = first_frame(&events_rx)?;
    stop.store(true, Ordering::Relaxed);

    let update = preview_update_from_bytes(&frame).context("not a preview data frame")?;
    let mut session = PreviewSession::new();
    session.merge(update);
    Ok(session.page_metas().len())
}

fn first_frame(events: &mpsc::Receiver<PreviewEvent>) -> Result<Vec<u8>> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for a preview frame");
        }
        match events.recv_timeout(remaining) {
            Ok(PreviewEvent::Binary(frame)) => {
                if matches!(
                    split_preview_data_frame(&frame),
                    Some(PreviewDataFrame::FullCurrent(_))
                ) {
                    return Ok(frame);
                }
                // Control frames share the data plane; keep waiting.
            }
            Ok(_) => {}
            Err(error) => bail!("waiting for a preview frame: {error}"),
        }
    }
}
