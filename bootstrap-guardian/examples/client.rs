//! Reference client — the shape the harness MCP `protected_write` shim will
//! adopt in the next Phase 0 slice. Not production code; educational.
//!
//! Run: `cargo run -p bootstrap-guardian --example client -- \
//!   --config /opt/claudir/guardian.json \
//!   --path /opt/nova/data/foo.txt \
//!   --content 'hello, world' \
//!   --reason 'smoke-test'`

use anyhow::{Context, Result};
use bootstrap_guardian::auth::{compute_hmac, load_key};
use bootstrap_guardian::{GuardianConfig, Op, Req, Resp};
use clap::Parser;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short, long, default_value = "/opt/claudir/guardian.json")]
    config: PathBuf,
    #[arg(long)]
    path: String,
    #[arg(long)]
    content: String,
    #[arg(long, default_value = "example-client")]
    reason: String,
}

fn main() -> Result<()> {
    let args = Cli::parse();
    let cfg = GuardianConfig::load(&args.config)?;
    let key = load_key(&cfg.key_path()).context("reading key")?;

    let nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    let bytes = args.content.into_bytes();
    let hmac = compute_hmac(&key, Op::Write, &args.path, &bytes, nonce);

    use base64::Engine;
    let req = Req {
        op: Op::Write,
        path: args.path,
        bytes_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        nonce,
        hmac,
        reason: Some(args.reason),
        proto_version: Some(bootstrap_guardian::proto::PROTO_VERSION),
    };

    let mut stream = UnixStream::connect(cfg.socket_path())?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let line = serde_json::to_string(&req)? + "\n";
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut resp_line = String::new();
    BufReader::new(&stream).read_line(&mut resp_line)?;
    let resp: Resp = serde_json::from_str(resp_line.trim_end())?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
