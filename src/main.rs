use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use clap::Parser;
use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};

#[derive(Parser)]
#[command(about = "LSP proxy that adds file watching on behalf of the client")]
struct Cli {
    /// After injecting didChangeWatchedFiles, send a no-op didChange for each
    /// open file to force the language server to re-analyze and push diagnostics.
    /// Workaround for OmniSharp not re-pushing diagnostics for related files
    /// after external file changes.
    #[arg(long, default_value_t = false)]
    notify_open_files: bool,

    /// Language server binary and its arguments (everything after --)
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

type Bytes = Vec<u8>;

fn frame(body: &[u8]) -> Bytes {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

async fn read_message<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Bytes>> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            if let Ok(len) = val.trim().parse() {
                content_length = Some(len);
            }
        }
    }

    let len = match content_length {
        Some(l) => l,
        None => return Ok(None),
    };

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

struct WatchedPattern {
    base_path: Option<PathBuf>,
    glob_set: GlobSet,
    kind_mask: u32,
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let without_scheme = uri.strip_prefix("file://")?;
    // Handle Windows paths: file:///C:/... → C:/...
    // Unix paths: file:///foo → /foo (keep the slash)
    let path_str = if without_scheme.len() > 2
        && without_scheme.starts_with('/')
        && without_scheme.chars().nth(2) == Some(':')
    {
        &without_scheme[1..]
    } else {
        without_scheme
    };
    Some(PathBuf::from(path_str))
}

fn path_to_file_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    let s = s.replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        // Windows drive letter: C:/foo → file:///C:/foo
        format!("file:///{s}")
    }
}

fn notify_kind_to_lsp(kind: &EventKind) -> Option<u32> {
    match kind {
        EventKind::Create(_) => Some(1),
        EventKind::Modify(_) => Some(2),
        EventKind::Remove(_) => Some(3),
        _ => None,
    }
}

fn extract_patterns(msg: &Value) -> Vec<WatchedPattern> {
    let mut patterns = Vec::new();
    let registrations = match msg["params"]["registrations"].as_array() {
        Some(r) => r,
        None => return patterns,
    };

    for reg in registrations {
        if reg["method"].as_str() != Some("workspace/didChangeWatchedFiles") {
            continue;
        }
        let watchers = match reg["registerOptions"]["watchers"].as_array() {
            Some(w) => w,
            None => continue,
        };
        for entry in watchers {
            let kind_mask = entry["kind"].as_u64().unwrap_or(7) as u32;
            let glob_val = &entry["globPattern"];

            let (base_path, pattern_str) = if let Some(s) = glob_val.as_str() {
                (None, s.to_string())
            } else {
                // RelativePattern: { baseUri: string | WorkspaceFolder, pattern: string }
                let base_uri = glob_val["baseUri"]
                    .as_str()
                    .or_else(|| glob_val["baseUri"]["uri"].as_str());
                let pat = match glob_val["pattern"].as_str() {
                    Some(p) => p.to_string(),
                    None => continue,
                };
                (base_uri.and_then(file_uri_to_path), pat)
            };

            let mut builder = GlobSetBuilder::new();
            match Glob::new(&pattern_str) {
                Ok(g) => {
                    builder.add(g);
                }
                Err(e) => {
                    eprintln!("lsp-proxy: invalid glob {pattern_str:?}: {e}");
                    continue;
                }
            }
            match builder.build() {
                Ok(glob_set) => patterns.push(WatchedPattern {
                    base_path,
                    glob_set,
                    kind_mask,
                }),
                Err(e) => eprintln!("lsp-proxy: failed to build globset: {e}"),
            }
        }
    }
    patterns
}

fn maybe_strip_diagnostics_version(parsed: Option<Value>, raw: &[u8]) -> Bytes {
    let mut v = match parsed {
        Some(v) if v["method"].as_str() == Some("textDocument/publishDiagnostics") => v,
        _ => return frame(raw),
    };
    if !v["params"]["version"].is_number() {
        return frame(raw);
    }
    let uri = v["params"]["uri"].as_str().unwrap_or("unknown").to_string();
    if let Some(params) = v["params"].as_object_mut() {
        params.remove("version");
    }
    eprintln!("[strip-version] removed version from publishDiagnostics for {uri}");
    match serde_json::to_vec(&v) {
        Ok(body) => frame(&body),
        Err(e) => {
            eprintln!("lsp-proxy: failed to reserialize publishDiagnostics: {e}");
            frame(raw)
        }
    }
}

async fn track_open_file(msg: &Value, open_files: &Mutex<HashMap<String, (String, i32)>>) {
    match msg["method"].as_str() {
        Some("textDocument/didOpen") => {
            let td = &msg["params"]["textDocument"];
            if let (Some(uri), Some(text), Some(version)) = (
                td["uri"].as_str(),
                td["text"].as_str(),
                td["version"].as_i64(),
            ) {
                open_files
                    .lock()
                    .await
                    .insert(uri.to_string(), (text.to_string(), version as i32));
            }
        }
        Some("textDocument/didClose") => {
            if let Some(uri) = msg["params"]["textDocument"]["uri"].as_str() {
                open_files.lock().await.remove(uri);
            }
        }
        Some("textDocument/didChange") => {
            let td = &msg["params"]["textDocument"];
            if let Some(uri) = td["uri"].as_str() {
                let version = td["version"].as_i64().unwrap_or(0) as i32;
                if let Some(text) = msg["params"]["contentChanges"]
                    .as_array()
                    .and_then(|cs| cs.last())
                    .and_then(|c| c["text"].as_str())
                {
                    open_files
                        .lock()
                        .await
                        .insert(uri.to_string(), (text.to_string(), version));
                }
            }
        }
        _ => {}
    }
}

async fn nudge_open_files(
    open_files: &Mutex<HashMap<String, (String, i32)>>,
    tx: &mpsc::Sender<Bytes>,
) {
    let mut files = open_files.lock().await;
    if files.is_empty() {
        return;
    }
    let mut messages = Vec::with_capacity(files.len());
    for (uri, (text, version)) in files.iter_mut() {
        *version += 1;
        eprintln!("[notify-open-files] nudging {uri}");
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": *version },
                "contentChanges": [{ "text": text }]
            }
        });
        match serde_json::to_vec(&notification) {
            Ok(body) => messages.push(frame(&body)),
            Err(e) => eprintln!("lsp-proxy: failed to serialize nudge for {uri}: {e}"),
        }
    }
    drop(files); // release lock before awaiting sends
    for msg in messages {
        let _ = tx.send(msg).await;
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let notify_open_files = cli.notify_open_files;
    let (bin, ls_args) = cli.command.split_first().expect("command is required");

    let mut ls = Command::new(bin)
        .args(ls_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("lsp-proxy: failed to spawn {bin}: {e}");
            std::process::exit(1);
        });

    let ls_stdin = ls.stdin.take().unwrap();
    let ls_stdout = ls.stdout.take().unwrap();

    // All messages destined for the language server's stdin
    let (to_ls_tx, to_ls_rx) = mpsc::channel::<Bytes>(128);
    // All messages destined for the client's stdout
    let (to_client_tx, to_client_rx) = mpsc::channel::<Bytes>(128);
    // New glob patterns discovered from client/registerCapability interception
    let (pattern_tx, pattern_rx) = mpsc::channel::<Vec<WatchedPattern>>(16);
    // URI → (content, version) for all currently open text documents
    let open_files: Arc<Mutex<HashMap<String, (String, i32)>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Task: client stdin → language server stdin
    let t_client_in = {
        let tx = to_ls_tx.clone();
        let open_files = Arc::clone(&open_files);
        tokio::spawn(async move {
            let mut reader = BufReader::new(tokio::io::stdin());
            loop {
                match read_message(&mut reader).await {
                    Ok(Some(msg)) => {
                        if notify_open_files {
                            if let Ok(v) = serde_json::from_slice::<Value>(&msg) {
                                track_open_file(&v, &open_files).await;
                            }
                        }
                        if tx.send(frame(&msg)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("lsp-proxy: error reading client stdin: {e}");
                        break;
                    }
                }
            }
        })
    };

    // Task: language server stdout → client stdout, intercepting client/registerCapability
    let t_ls_out = {
        let to_client = to_client_tx;
        let to_ls = to_ls_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(ls_stdout);
            loop {
                let msg = match read_message(&mut reader).await {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("lsp-proxy: error reading language server stdout: {e}");
                        break;
                    }
                };

                let parsed: Option<Value> = serde_json::from_slice(&msg).ok();
                let is_register_capability = parsed.as_ref().and_then(|v| v["method"].as_str())
                    == Some("client/registerCapability");

                if is_register_capability {
                    let v = parsed.as_ref().unwrap();

                    let patterns = extract_patterns(v);
                    if !patterns.is_empty() {
                        let _ = pattern_tx.send(patterns).await;
                    }

                    // Respond to the language server as if the client accepted
                    if let Some(id) = v.get("id") {
                        let response = json!({"jsonrpc": "2.0", "result": null, "id": id});
                        match serde_json::to_vec(&response) {
                            Ok(body) => {
                                let _ = to_ls.send(frame(&body)).await;
                            }
                            Err(e) => eprintln!("lsp-proxy: failed to serialize response: {e}"),
                        }
                    }
                } else {
                    let out = maybe_strip_diagnostics_version(parsed, &msg);
                    if to_client.send(out).await.is_err() {
                        break;
                    }
                }
            }
        })
    };

    // Task: drain to_ls_rx → language server stdin
    let t_ls_in = tokio::spawn(async move {
        let mut stdin = ls_stdin;
        let mut rx = to_ls_rx;
        while let Some(msg) = rx.recv().await {
            if let Err(e) = stdin.write_all(&msg).await {
                eprintln!("lsp-proxy: error writing to language server stdin: {e}");
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    // Task: drain to_client_rx → client stdout
    let t_client_out = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut rx = to_client_rx;
        while let Some(msg) = rx.recv().await {
            if let Err(e) = stdout.write_all(&msg).await {
                eprintln!("lsp-proxy: error writing to client stdout: {e}");
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    // Task: watch filesystem and inject workspace/didChangeWatchedFiles notifications
    let t_watcher = {
        let tx = to_ls_tx;
        let open_files = Arc::clone(&open_files);
        tokio::spawn(async move {
            let (notify_tx, mut notify_rx) =
                mpsc::unbounded_channel::<notify::Result<notify::Event>>();

            let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |res| {
                let _ = notify_tx.send(res);
            }) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("lsp-proxy: failed to create file watcher: {e}");
                    return;
                }
            };

            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let mut all_patterns: Vec<WatchedPattern> = Vec::new();
            let mut watched_dirs: HashSet<PathBuf> = HashSet::new();
            let mut pattern_rx = pattern_rx;
            let mut pattern_rx_done = false;

            loop {
                tokio::select! {
                    maybe = pattern_rx.recv(), if !pattern_rx_done => {
                        match maybe {
                            Some(new_patterns) => {
                                for pat in &new_patterns {
                                    let dir = pat.base_path.clone().unwrap_or_else(|| cwd.clone());
                                    if watched_dirs.insert(dir.clone()) {
                                        if let Err(e) = watcher.watch(&dir, RecursiveMode::Recursive) {
                                            eprintln!("lsp-proxy: cannot watch {dir:?}: {e}");
                                        }
                                    }
                                }
                                all_patterns.extend(new_patterns);
                            }
                            None => { pattern_rx_done = true; }
                        }
                    }
                    event_result = notify_rx.recv() => {
                        match event_result {
                            Some(Ok(event)) => {
                                let lsp_kind = match notify_kind_to_lsp(&event.kind) {
                                    Some(k) => k,
                                    None => continue,
                                };

                                let mut changes: Vec<Value> = Vec::new();
                                'paths: for path in &event.paths {
                                    for pat in &all_patterns {
                                        if pat.kind_mask & lsp_kind == 0 {
                                            continue;
                                        }
                                        let matches = if let Some(ref base) = pat.base_path {
                                            path.strip_prefix(base)
                                                .map(|rel| pat.glob_set.is_match(rel))
                                                .unwrap_or(false)
                                        } else {
                                            let rel = path.strip_prefix(&cwd).unwrap_or(path);
                                            pat.glob_set.is_match(rel) || pat.glob_set.is_match(path)
                                        };

                                        if matches {
                                            changes.push(json!({
                                                "uri": path_to_file_uri(path),
                                                "type": lsp_kind
                                            }));
                                            continue 'paths;
                                        }
                                    }
                                }

                                if !changes.is_empty() {
                                    let notification = json!({
                                        "jsonrpc": "2.0",
                                        "method": "workspace/didChangeWatchedFiles",
                                        "params": { "changes": changes }
                                    });
                                    match serde_json::to_vec(&notification) {
                                        Ok(body) => { let _ = tx.send(frame(&body)).await; }
                                        Err(e) => eprintln!("lsp-proxy: failed to serialize notification: {e}"),
                                    }

                                    if notify_open_files {
                                        nudge_open_files(&open_files, &tx).await;
                                    }
                                }
                            }
                            Some(Err(e)) => eprintln!("lsp-proxy: file watch error: {e}"),
                            None => break,
                        }
                    }
                }
            }
        })
    };

    let status = ls.wait().await.unwrap_or_else(|e| {
        eprintln!("lsp-proxy: failed to wait for language server: {e}");
        std::process::exit(1);
    });

    t_client_in.abort();
    t_ls_out.abort();
    t_ls_in.abort();
    t_client_out.abort();
    t_watcher.abort();

    std::process::exit(status.code().unwrap_or(1));
}
