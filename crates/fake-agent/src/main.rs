//! Fake Agent - A deterministic executable for testing agent runtime integration.
//!
//! Supports deterministic scenarios via command-line arguments:
//! - `--scenario startup` : Prints startup message and exits
//! - `--scenario working` : Continuously prints working messages
//! - `--scenario streaming` : Streams large output then exits
//! - `--scenario waiting` : Waits for input, echoes it, then exits
//! - `--scenario approval` : Prints approval request and waits
//! - `--scenario completion` : Prints success message and exits 0
//! - `--scenario failure` : Prints error message and exits 1
//! - `--scenario auth-failure` : Prints auth error and exits 2
//! - `--scenario flaky` : Fails with exit 3 on attempt 1, succeeds on
//!   `--attempt N` whenever N > 1 (deterministic retry fixture)
//! - `--scenario modify` : Phase 3C worktree fixture — writes files listed
//!   via `--write-file <relpath>` (content = `--set-content <text>` or a
//!   deterministic default), then exits 0. Runs in the agent's cwd, so in
//!   an isolated worktree it only ever touches that worktree.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args
        .iter()
        .position(|a| a == "--scenario")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "working".to_string());
    let attempt: u64 = args
        .iter()
        .position(|a| a == "--attempt")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let duration_secs: Option<u64> = args
        .iter()
        .position(|a| a == "--duration")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    // Phase 3C: files to write under the agent cwd (relative paths only).
    let write_files: Vec<String> = args
        .iter()
        .position(|a| a == "--write-file")
        .map(|start| {
            let mut v = Vec::new();
            let mut i = start + 1;
            while i < args.len() && args[i] != "--set-content" && args[i] != "--duration" {
                v.push(args[i].clone());
                i += 1;
            }
            v
        })
        .unwrap_or_default();
    let content: String = args
        .iter()
        .position(|a| a == "--set-content")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "change from fake-agent\n".to_string());
    // `--echo <text>` prints the given text first (secret-safety tests use
    // this to prove redaction works end-to-end: the agent emits a sentinel
    // and the pipeline must mask it everywhere).
    if let Some(text) = args
        .iter()
        .position(|a| a == "--echo")
        .and_then(|i| args.get(i + 1))
    {
        println!("{text}");
    }

    match scenario.as_str() {
        "startup" => {
            println!("Fake agent starting up...");
            thread::sleep(Duration::from_millis(500));
            println!("Startup complete.");
        }
        "working" => {
            println!("Fake agent is working...");
            for i in 1..=10 {
                println!("Working step {}/10", i);
                thread::sleep(Duration::from_millis(200));
            }
            println!("Work complete.");
        }
        "streaming" => {
            for i in 1..=1000 {
                println!("Stream line {}", i);
                if i % 100 == 0 {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
        "waiting" => {
            println!("Fake agent waiting for input...");
            let stdin = io::stdin();
            for text in stdin.lock().lines().map_while(Result::ok) {
                if text.trim().is_empty() || text.trim() == "exit" || text.trim() == "quit" {
                    break;
                }
                println!("Echo: {}", text);
            }
            println!("Exiting.");
        }
        "approval" => {
            println!("⚠️ APPROVAL REQUIRED: Execute dangerous command? [y/N]");
            let stdin = io::stdin();
            if let Some(Ok(line)) = stdin.lock().lines().next() {
                if line.trim().eq_ignore_ascii_case("y") {
                    println!("Approved. Executing...");
                    thread::sleep(Duration::from_millis(500));
                    println!("Done.");
                } else {
                    println!("Denied. Aborting.");
                }
            }
        }
        "completion" => {
            println!("Task completed successfully.");
            std::process::exit(0);
        }
        "failure" => {
            eprintln!("Error: Simulated agent failure");
            std::process::exit(1);
        }
        "auth-failure" => {
            eprintln!("Error: Invalid API credentials");
            std::process::exit(2);
        }
        "flaky" => {
            // Deterministic retry fixture (3a.md §33): attempt 1 fails with
            // a transient failure; later attempts succeed.
            if attempt <= 1 {
                eprintln!("Error: Transient provider hiccup");
                std::process::exit(3);
            }
            println!("Task completed successfully (attempt {attempt}).");
            std::process::exit(0);
        }
        "modify" => {
            for rel in &write_files {
                let path = Path::new(rel);
                // Refuse absolute/escaping paths (worktree fixture safety).
                if path.is_absolute() || rel.contains("..") || rel.contains("\\") {
                    eprintln!("refusing unsafe path {rel}");
                    std::process::exit(1);
                }
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let mut f = std::fs::File::create(path).expect("write file");
                use std::io::Write as _;
                f.write_all(content.as_bytes()).expect("write content");
                println!("Modified {rel}");
            }
            // Phase 3C: a real coding agent commits its work to its feature
            // branch — the artifact the review/merge lifecycle consumes.
            // Commit in the agent's cwd (the worktree) so the branch
            // actually diverges from the base (§22 merge needs a commit).
            let _ = std::process::Command::new("git")
                .args(["add", "-A"])
                .status();
            let _ = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=fake-agent",
                    "-c",
                    "user.email=fake-agent@flashterminal.local",
                    "commit",
                    "-q",
                    "-m",
                    "fake-agent: apply task changes",
                ])
                .status();
            println!("Task completed successfully.");
            // `--duration <secs>` keeps the modified worktree alive (the
            // cancellation fixture, 3c.md §42 — cancel while working).
            if let Some(secs) = duration_secs {
                let deadline = Instant::now() + Duration::from_secs(secs);
                while Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            std::process::exit(0);
        }
        "crash" => {
            std::process::exit(139); // SIGSEGV
        }
        "large-output" => {
            for i in 1..=100_000 {
                println!(
                    "Line {} of large output with some padding to make it realistic",
                    i
                );
                if i % 10000 == 0 {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
        "long-running" => {
            // Continuous work output until stdin closes (or --duration).
            println!("Long-running agent is working...");
            let deadline = duration_secs.map(|s| Instant::now() + Duration::from_secs(s));
            let mut step = 0u64;
            // Drain any pending input (EOF/"exit" terminates the session just
            // like a real agent) without ever blocking the output loop.
            loop {
                let status = unsafe {
                    let mut pfd = libc::pollfd {
                        fd: 0,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    libc::poll(&mut pfd, 1, 20)
                };
                if status > 0 {
                    let mut line = String::new();
                    let got = io::stdin().read_line(&mut line).unwrap_or(0);
                    if got == 0 {
                        println!("Long-running agent finished (input closed).");
                        break;
                    }
                    if line.trim() == "exit" || line.trim() == "quit" {
                        println!("Long-running agent finished (exit requested).");
                        break;
                    }
                }
                for _ in 0..10 {
                    step += 1;
                    println!("Long-running step {step}");
                    thread::sleep(Duration::from_millis(20));
                }
                if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                    println!("Long-running agent finished (duration reached).");
                    break;
                }
            }
        }
        _ => {
            eprintln!(
                "Unknown scenario: {}. Using 'working' as default.",
                scenario
            );
            println!("Fake agent is working...");
            for i in 1..=5 {
                println!("Working step {}/5", i);
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    // Ensure stdout is flushed
    let _ = io::stdout().flush();
}
