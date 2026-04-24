use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use classify_pr::classify::{ClassifyInput, classify_and_exit_code};
use classify_pr::verdict::Verdict;

/// Phase 4 mechanical auto-merge classifier.
///
/// Runs deterministic checks against a PR diff and emits a JSON verdict.
/// The workflow uses the process exit code for its decision; stdout JSON
/// is consumed by the reconciler and the canary.
#[derive(Parser)]
#[command(
    name = "classify-pr",
    version,
    about = "Phase 4 mechanical auto-merge classifier (fmt-equivalence v1)"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Classify a PR. Reads the list of changed paths from `--changed-paths-file`
    /// (one path per line) or `--stdin-paths`, runs rustfmt + protected-path
    /// gates against `--repo-root`, and writes the verdict to stdout.
    Check {
        /// Path to the repo's root directory. Used as the CWD for `cargo fmt`.
        #[arg(long)]
        repo_root: PathBuf,

        /// File containing the list of changed paths, one per line.
        /// Produced by `git diff --name-only` or `gh pr diff --name-only`.
        #[arg(long, conflicts_with = "stdin_paths")]
        changed_paths_file: Option<PathBuf>,

        /// Read changed paths from stdin, one per line. Local debug loop:
        ///   `gh pr diff 42 --name-only | classify-pr check --stdin-paths --repo-root .`
        #[arg(long)]
        stdin_paths: bool,

        /// PR head commit SHA. Embedded in the verdict for attribution.
        #[arg(long)]
        head_sha: Option<String>,

        /// PR base commit SHA. Embedded in the verdict for attribution.
        #[arg(long)]
        base_sha: Option<String>,

        /// Value of the AUTOMERGE_ENABLED repo variable. Positive polarity:
        /// the ONLY value that un-pauses is exactly `1`. Missing/unset = paused.
        #[arg(long)]
        automerge_enabled: Option<String>,

        /// Optional path to write the verdict JSON. If omitted, stdout.
        #[arg(long)]
        verdict_out: Option<PathBuf>,

        /// Also print a human-readable summary to stderr (handy in interactive use).
        #[arg(long)]
        verbose: bool,
    },

    /// Render a previously-emitted verdict JSON as a human-readable summary.
    /// Used by `docs/phase4-debugging.md` and the digest cron.
    Explain {
        /// Path to a verdict JSON file. Use `-` for stdin.
        #[arg(long)]
        verdict_file: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Check {
            repo_root,
            changed_paths_file,
            stdin_paths,
            head_sha,
            base_sha,
            automerge_enabled,
            verdict_out,
            verbose,
        } => run_check(
            repo_root,
            changed_paths_file,
            stdin_paths,
            head_sha,
            base_sha,
            automerge_enabled,
            verdict_out,
            verbose,
        ),
        Cmd::Explain { verdict_file } => match run_explain(verdict_file) {
            Ok(()) => ExitCode::from(0),
            Err(e) => {
                eprintln!("explain: {e:#}");
                ExitCode::from(classify_pr::exit_codes::OPERATIONAL_ERROR as u8)
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn run_check(
    repo_root: PathBuf,
    changed_paths_file: Option<PathBuf>,
    stdin_paths: bool,
    head_sha: Option<String>,
    base_sha: Option<String>,
    automerge_enabled: Option<String>,
    verdict_out: Option<PathBuf>,
    verbose: bool,
) -> ExitCode {
    let changed_paths = match read_changed_paths(changed_paths_file, stdin_paths) {
        Ok(p) => p,
        Err(e) => {
            let v = Verdict::classifier_error(&format!("reading changed paths: {e:#}"));
            emit_verdict(&v, verdict_out.as_deref(), verbose);
            return ExitCode::from(classify_pr::exit_codes::OPERATIONAL_ERROR as u8);
        }
    };

    let input = ClassifyInput {
        repo_root,
        changed_paths,
        head_sha,
        base_sha,
        automerge_enabled,
    };
    let (v, code) = classify_and_exit_code(&input);
    emit_verdict(&v, verdict_out.as_deref(), verbose);
    ExitCode::from(code as u8)
}

fn read_changed_paths(file: Option<PathBuf>, stdin: bool) -> Result<Vec<String>> {
    use std::io::BufRead;
    if stdin {
        let stdin = std::io::stdin();
        let mut paths = Vec::new();
        for line in stdin.lock().lines() {
            let line = line?;
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                paths.push(trimmed.to_string());
            }
        }
        return Ok(paths);
    }
    let file = file.ok_or_else(|| {
        anyhow::anyhow!("either --changed-paths-file or --stdin-paths is required")
    })?;
    let contents = std::fs::read_to_string(&file)?;
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn emit_verdict(v: &Verdict, out: Option<&std::path::Path>, verbose: bool) {
    let json = v.to_json();
    if let Some(path) = out {
        if let Err(e) = std::fs::write(path, &json) {
            eprintln!("warning: failed to write verdict to {path:?}: {e:#}");
            println!("{json}");
        }
    } else {
        println!("{json}");
    }
    if verbose {
        eprintln!(
            "verdict: eligible={} reason_code={:?}",
            v.eligible, v.reason_code
        );
        eprintln!("  {}", v.human_message);
        if !v.suggested_fix.is_empty() {
            eprintln!("  fix: {}", v.suggested_fix);
        }
    }
}

fn run_explain(verdict_file: PathBuf) -> Result<()> {
    let json = if verdict_file == std::path::Path::new("-") {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        std::fs::read_to_string(&verdict_file)?
    };
    let v: Verdict = serde_json::from_str(&json)?;
    println!(
        "eligible: {}\nreason_code: {:?}\nmessage: {}\nsuggested_fix: {}",
        v.eligible, v.reason_code, v.human_message, v.suggested_fix
    );
    if !v.protected_paths_touched.is_empty() {
        println!("protected_paths_touched:");
        for p in &v.protected_paths_touched {
            println!("  - {p}");
        }
    }
    println!("docs: {}", v.docs_url);
    Ok(())
}
