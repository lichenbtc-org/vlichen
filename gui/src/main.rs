//! `vlichen-gui`: a vlichen wallet as a page in your browser, served on
//! 127.0.0.1 only. Each launch has a token of its own in the URL it opens,
//! which every call to the API must carry: other pages in the browser
//! can't drive the wallet.


use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use vlichen_gui::api::{route, PAGE};
use vlichen_gui::app::{self, App, Conn};
use clap::Parser;
use serde_json::json;
use tiny_http::{Header, Request, Response, Server};


#[derive(Parser)]
#[command(about = "The vlichen wallet in your browser")]
struct Args {
    /// Where the GUI keeps its wallets and settings.
    #[arg(long)]
    dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:18340")]
    listen: String,
    /// Don't open the browser.
    #[arg(long)]
    no_open: bool,
    /// Connect to a devstack (its `devstack.json`).
    #[arg(long)]
    devstack: Option<PathBuf>,
}

/// The platform's place for an application's data.
fn default_dir() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").context("HOME")?);
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/vlichen-gui")
    } else {
        std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".local/share")).join("vlichen-gui")
    })
}

fn main() -> Result<()> {
    app::tune_malloc();
    let args = Args::parse();
    let host = args.listen.split(':').next().unwrap_or_default();
    ensure!(host == "127.0.0.1" || host == "localhost", "the GUI listens on this machine only (127.0.0.1), not {host}");
    let server = Server::http(&args.listen).map_err(|e| anyhow::anyhow!("listen on {}: {e}", args.listen))?;
    let dir = match args.dir {
        Some(d) => d,
        None => default_dir()?,
    };
    let app = App::new(dir.clone())?;
    if let Some(f) = &args.devstack {
        // the devstack writes it once it's up, and removes it when stopped
        if !f.exists() {
            eprintln!("waiting for the devstack to write {} ...", f.display());
            while !f.exists() {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        let conn: Conn = serde_json::from_slice(&std::fs::read(f)?).context("a devstack.json")?;
        app.saved.lock().unwrap().conn = conn;
        app.save()?;
    }
    app::launch(&app);
    let mut t = [0u8; 16];
    getrandom::getrandom(&mut t)?;
    let token = hex::encode(t);
    let url = format!("http://{}/?t={token}", args.listen);
    println!("vlichen-gui ({}): {url}", dir.display());
    if !args.no_open {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).spawn();
    }
    // tiny_http 0.12 ends its listener for good at an error accepting a
    // connection (a reset before accept, too many open files): listen
    // again on the same address, as lichen's sequencer and hub do
    let mut server = server;
    loop {
        match server.recv() {
            Ok(req) => {
                let app = app.clone();
                let token = token.clone();
                std::thread::spawn(move || serve(&app, &token, req));
            }
            Err(e) => {
                eprintln!("vlichen-gui: the listener stopped ({e}): listening again on {}", args.listen);
                let mut said = false;
                server = loop {
                    match Server::http(&args.listen) {
                        Ok(s) => break s,
                        Err(e) => {
                            if !said {
                                eprintln!("vlichen-gui: listening on {}: {e} (trying each second)", args.listen);
                                said = true;
                            }
                            std::thread::sleep(std::time::Duration::from_secs(1));
                        }
                    }
                };
                eprintln!("vlichen-gui: listening again on {}", args.listen);
            }
        }
    }
}

fn serve(app: &Arc<App>, token: &str, mut req: Request) {
    let mut body = String::new();
    let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let (code, text, ct) = if path == "/" {
        (200, PAGE.to_string(), "text/html; charset=utf-8")
    } else {
        let given = req.headers().iter().find(|h| h.field.equiv("X-Lichen-Token")).map(|h| h.value.as_str().to_string());
        let r = if given.as_deref() != Some(token) { Err(anyhow::anyhow!("not this launch's token: open the URL vlichen-gui printed")) } else { route(app, req.method().as_str(), path, query, &body) };
        match r {
            Ok((v, ct)) => (200, v, ct),
            Err(e) => (400, json!({ "error": format!("{e:#}") }).to_string(), "application/json"),
        }
    };
    let h = Header::from_bytes("Content-Type", ct).unwrap();
    let _ = req.respond(Response::from_string(text).with_status_code(code).with_header(h));
}

