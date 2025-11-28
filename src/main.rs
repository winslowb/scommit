use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(version, about = "Smart git commit helper")]
struct Cli {
    /// Preview actions without committing or pushing
    #[arg(long)]
    dry_run: bool,

    /// Skip the git add -A step and use existing staged changes
    #[arg(long)]
    no_stage: bool,

    /// Skip pushing to the upstream remote
    #[arg(long)]
    no_push: bool,

    /// Skip pulling/rebasing even if branch is behind upstream
    #[arg(long)]
    skip_pull: bool,

    /// Provide a custom commit message subject (auto body will still be added)
    #[arg(long, short = 'm')]
    message: Option<String>,

    /// Disable AI generation even if OPENAI_API_KEY is present
    #[arg(long)]
    no_ai: bool,

    /// Override OpenAI model (default: gpt-4o-mini or env SCOMMIT_MODEL)
    #[arg(long)]
    model: Option<String>,
}

#[derive(Debug, Clone)]
enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed { from: String, to: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Category {
    Docs,
    Tests,
    Config,
    Code,
    Other,
}

#[derive(Debug, Clone)]
struct FileChange {
    path: String,
    status: FileStatus,
    added: u32,
    deleted: u32,
    category: Category,
}

#[derive(Debug, Default, Clone)]
struct Stats {
    files: usize,
    added: u32,
    deleted: u32,
    categories: HashMap<Category, usize>,
    new_files: usize,
    removed_files: usize,
}

static CATEGORY_NAMES: Lazy<HashMap<Category, &'static str>> = Lazy::new(|| {
    use Category::*;
    HashMap::from([
        (Docs, "docs"),
        (Tests, "tests"),
        (Config, "config"),
        (Code, "code"),
        (Other, "other"),
    ])
});

fn main() -> Result<()> {
    let cli = Cli::parse();

    let repo_root = repo_root()?;
    env::set_current_dir(&repo_root)
        .with_context(|| format!("failed to enter repo at {}", repo_root.display()))?;

    if !cli.no_stage {
        stage_everything()?;
    }

    if !has_staged_changes()? {
        println!("No staged changes found. Nothing to commit.");
        return Ok(());
    }

    let changes = collect_staged_changes()?;
    let stats = compute_stats(&changes);
    let ai_enabled = !cli.no_ai && env::var("OPENAI_API_KEY").is_ok();
    let model = cli
        .model
        .or_else(|| env::var("SCOMMIT_MODEL").ok())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let (subject, body) = match cli.message {
        Some(subject) => (subject, build_body(&changes, &stats)),
        None if ai_enabled => match ai_commit_message(&changes, &stats, &model) {
            Ok(Some(pair)) => pair,
            Ok(None) => build_commit_message(&changes, &stats),
            Err(e) => {
                eprintln!("AI generation failed ({e}); falling back to heuristic.");
                build_commit_message(&changes, &stats)
            }
        },
        _ => build_commit_message(&changes, &stats),
    };

    if cli.dry_run {
        println!("DRY RUN\nSubject: {}\n\n{}", subject, body);
        return Ok(());
    }

    create_commit(&subject, &body)?;

    if cli.no_push {
        println!("Skipping push (--no-push).");
        return Ok(());
    }

    if let Some(upstream) = upstream_branch()? {
        let (ahead, behind) = ahead_behind(&upstream)?;
        if behind > 0 && !cli.skip_pull {
            println!(
                "Branch is behind {} by {} commit(s); rebasing before push...",
                upstream, behind
            );
            git(&["pull", "--rebase"])?;
        } else if behind > 0 {
            println!(
                "Branch is behind {} by {} commit(s); skipping pull (--skip-pull).",
                upstream, behind
            );
        }

        if ahead > 0 || behind == 0 {
            git(&["push"])?;
        } else {
            println!("No local commits to push.");
        }
    } else {
        println!("No upstream configured; commit created but not pushed.");
    }

    Ok(())
}

fn repo_root() -> Result<PathBuf> {
    let out = git_output(&["rev-parse", "--show-toplevel"])?;
    let path = out.trim();
    if path.is_empty() {
        bail!("Could not resolve repository root");
    }
    Ok(PathBuf::from(path))
}

fn stage_everything() -> Result<()> {
    git(&["add", "-A"])?;
    Ok(())
}

fn has_staged_changes() -> Result<bool> {
    let status = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .status()
        .context("checking staged changes")?;
    Ok(!status.success())
}

fn collect_staged_changes() -> Result<Vec<FileChange>> {
    let mut additions: HashMap<String, (u32, u32)> = HashMap::new();
    let numstat = git_output(&["diff", "--cached", "--numstat"])?;
    for line in numstat.lines() {
        let mut parts = line.split_whitespace();
        let added = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
        let deleted = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
        if let Some(path) = parts.next_back() {
            additions.insert(path.to_string(), (added, deleted));
        }
    }

    let mut changes = Vec::new();
    let name_status = git_output(&["diff", "--cached", "--name-status"])?;
    for line in name_status.lines() {
        let mut parts = line.split('\t');
        let status = parts.next().unwrap_or("").trim();
        let path = parts.next().unwrap_or("").trim();
        if path.is_empty() {
            continue;
        }

        let file_status = match status.chars().next().unwrap_or('M') {
            'A' => FileStatus::Added,
            'M' => FileStatus::Modified,
            'D' => FileStatus::Deleted,
            'R' => {
                let to = parts.next().unwrap_or("").trim().to_string();
                FileStatus::Renamed {
                    from: path.to_string(),
                    to,
                }
            }
            _ => FileStatus::Modified,
        };

        let display_path = match &file_status {
            FileStatus::Renamed { to, .. } => to.clone(),
            _ => path.to_string(),
        };
        let (added, deleted) = additions
            .get(&display_path)
            .or_else(|| additions.get(path))
            .copied()
            .unwrap_or((0, 0));

        changes.push(FileChange {
            path: display_path.clone(),
            status: file_status,
            added,
            deleted,
            category: categorize(&display_path),
        });
    }

    Ok(changes)
}

fn categorize(path: &str) -> Category {
    let lower = path.to_ascii_lowercase();
    let ext = PathBuf::from(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    if lower.contains("readme")
        || lower.contains("docs/")
        || matches!(
            ext.as_deref(),
            Some("md" | "markdown" | "rst" | "txt" | "adoc" | "org")
        )
    {
        return Category::Docs;
    }

    if lower.contains("test")
        || matches!(
            ext.as_deref(),
            Some("spec" | "snap" | "snap.new" | "snap.old")
        )
    {
        return Category::Tests;
    }

    if matches!(
        ext.as_deref(),
        Some(
            "yml"
                | "yaml"
                | "json"
                | "toml"
                | "ini"
                | "cfg"
                | "conf"
                | "lock"
                | "env"
                | "properties"
        )
    ) || lower.contains("config")
    {
        return Category::Config;
    }

    if matches!(
        ext.as_deref(),
        Some(
            "rs" | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "go"
                | "rb"
                | "java"
                | "kt"
                | "c"
                | "cc"
                | "cpp"
                | "h"
                | "hpp"
                | "swift"
                | "scala"
                | "php"
        )
    ) {
        return Category::Code;
    }

    Category::Other
}

fn compute_stats(changes: &[FileChange]) -> Stats {
    let mut stats = Stats::default();
    stats.files = changes.len();
    for c in changes {
        stats.added += c.added;
        stats.deleted += c.deleted;
        *stats.categories.entry(c.category).or_insert(0) += 1;
        match c.status {
            FileStatus::Added => stats.new_files += 1,
            FileStatus::Deleted => stats.removed_files += 1,
            _ => {}
        }
    }
    stats
}

fn build_commit_message(changes: &[FileChange], stats: &Stats) -> (String, String) {
    let subject = build_subject(changes, stats);
    let body = build_body(changes, stats);
    (subject, body)
}

fn build_subject(changes: &[FileChange], stats: &Stats) -> String {
    let prefix = choose_prefix(stats);

    let mut ranked: Vec<_> = changes
        .iter()
        .map(|c| (c.added + c.deleted, short_name(&c.path)))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0));

    let names: Vec<String> = ranked.into_iter().take(2).map(|(_, n)| n).collect();
    let focus = if names.is_empty() {
        "changes".to_string()
    } else {
        names.join(" & ")
    };

    let mut subject = format!("{prefix}: update {focus}");
    if subject.len() > 72 {
        subject.truncate(72);
    }
    subject
}

fn choose_prefix(stats: &Stats) -> &'static str {
    let only_category = if stats.categories.len() == 1 {
        stats.categories.keys().next().copied()
    } else {
        None
    };

    match only_category {
        Some(Category::Docs) => "docs",
        Some(Category::Tests) => "test",
        Some(Category::Config) => "chore",
        _ => {
            if stats.new_files > 0 && stats.added > stats.deleted {
                "feat"
            } else if stats.deleted > stats.added && stats.categories.contains_key(&Category::Code)
            {
                "refactor"
            } else {
                "chore"
            }
        }
    }
}

fn build_body(changes: &[FileChange], stats: &Stats) -> String {
    use std::fmt::Write;
    let mut body = String::new();
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M");
    writeln!(
        &mut body,
        "Files: {} | +{} / -{} | generated {}",
        stats.files, stats.added, stats.deleted, timestamp
    )
    .ok();
    writeln!(&mut body, "Changes:").ok();

    let mut listed = 0usize;
    for change in changes.iter().take(12) {
        listed += 1;
        let category = CATEGORY_NAMES
            .get(&change.category)
            .copied()
            .unwrap_or("other");
        match &change.status {
            FileStatus::Added => {
                writeln!(
                    &mut body,
                    "- add {} (+{}/-{}) [{}]",
                    change.path, change.added, change.deleted, category
                )
                .ok();
            }
            FileStatus::Modified => {
                writeln!(
                    &mut body,
                    "- update {} (+{}/-{}) [{}]",
                    change.path, change.added, change.deleted, category
                )
                .ok();
            }
            FileStatus::Deleted => {
                writeln!(
                    &mut body,
                    "- remove {} (+{}/-{}) [{}]",
                    change.path, change.added, change.deleted, category
                )
                .ok();
            }
            FileStatus::Renamed { from, .. } => {
                writeln!(
                    &mut body,
                    "- rename {} -> {} (+{}/-{}) [{}]",
                    from, change.path, change.added, change.deleted, category
                )
                .ok();
            }
        }
    }

    if changes.len() > listed {
        writeln!(
            &mut body,
            "- ... {} more file(s) not listed",
            changes.len() - listed
        )
        .ok();
    }

    writeln!(
        &mut body,
        "\nAuto-generated by scommit. Edit with --message if you want to override."
    )
    .ok();

    body
}

fn short_name(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

fn recent_commit_subjects(n: usize) -> Result<Vec<String>> {
    let out = git_output(&["log", "-n", &n.to_string(), "--pretty=%s"])?;
    Ok(out
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|s| s.to_string())
        .collect())
}

fn sanitize_json_blob(content: &str) -> Option<String> {
    let trimmed = content.trim();

    // If fenced (``` or ```json), strip fence and grab JSON object inside.
    if trimmed.starts_with("```") {
        if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
            if start < end {
                return Some(trimmed[start..=end].to_string());
            }
        }
    }

    // Otherwise slice from first '{' to last '}'.
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if start < end {
            return Some(trimmed[start..=end].to_string());
        }
    }

    None
}

fn strip_bullet_prefix(line: &str) -> &str {
    line.trim().trim_start_matches(&['-', '•'][..]).trim_start()
}

fn coerce_subject(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(extract_text)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn coerce_body(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.trim().to_string(),
        Some(serde_json::Value::Array(items)) => {
            let lines: Vec<String> = items
                .iter()
                .filter_map(extract_text)
                .map(|l| {
                    let cleaned = strip_bullet_prefix(&l);
                    format!("- {}", cleaned)
                })
                .collect();
            lines.join("\n")
        }
        Some(serde_json::Value::Object(map)) => {
            // Some models nest body under "bullets" or "lines".
            if let Some(bullets) = map.get("bullets").or_else(|| map.get("lines")) {
                return coerce_body(Some(bullets));
            }
            if let Some(text) = extract_text(&serde_json::Value::Object(map.clone())) {
                return text.trim().to_string();
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn extract_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Array(items) => {
            let joined: Vec<String> = items.iter().filter_map(extract_text).collect();
            if joined.is_empty() {
                None
            } else {
                Some(joined.join("\n"))
            }
        }
        serde_json::Value::Object(map) => {
            // Look for common textual keys.
            for key in ["text", "value", "content", "message", "summary"] {
                if let Some(v) = map.get(key) {
                    if let Some(s) = extract_text(v) {
                        return Some(s);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn diff_stat() -> Result<String> {
    Ok(git_output(&["diff", "--cached", "--stat", "--no-color"])?
        .trim()
        .to_string())
}

fn diff_excerpt(max_chars: usize) -> Result<String> {
    let raw = git_output(&["diff", "--cached", "--unified=3", "--no-color"])?;
    let excerpt: String = raw.chars().take(max_chars).collect();
    Ok(excerpt)
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

// Extract a JSON blob even if the model wrapped it in markdown fences.
fn ai_commit_message(
    changes: &[FileChange],
    stats: &Stats,
    model: &str,
) -> Result<Option<(String, String)>> {
    let key = match env::var("OPENAI_API_KEY") {
        Ok(k) => k,
        Err(_) => return Ok(None),
    };

    let stat = diff_stat().unwrap_or_default();
    let patch = diff_excerpt(4000).unwrap_or_default();

    let recent = recent_commit_subjects(6).unwrap_or_default();
    let mut change_lines = String::new();
    for c in changes.iter().take(24) {
        let (action, detail) = match &c.status {
            FileStatus::Added => ("add", c.path.clone()),
            FileStatus::Modified => ("update", c.path.clone()),
            FileStatus::Deleted => ("remove", c.path.clone()),
            FileStatus::Renamed { from, .. } => ("rename", format!("{from} -> {}", c.path)),
        };
        use std::fmt::Write;
        writeln!(
            &mut change_lines,
            "{} {} (+{}/-{}) [{}]",
            action,
            detail,
            c.added,
            c.deleted,
            CATEGORY_NAMES.get(&c.category).copied().unwrap_or("other")
        )
        .ok();
    }

    let prompt = format!(
        "Repo stats: files {}, +{}, -{}; categories {:?}; new {}, removed {}.\nRecent commit subjects:\n- {}\nChanges (staged):\n{}\n\nDiffstat:\n{}\n\nDiff excerpt (trimmed):\n{}\n\nWrite 2-5 bullets that capture the most meaningful changes (what/why), call out new commands/flags/examples or config/doc topics when present, and note any behavioral impacts or risks. Avoid generic wording; be specific to these changes.",
        stats.files,
        stats.added,
        stats.deleted,
        stats.categories,
        stats.new_files,
        stats.removed_files,
        recent.join("\n- "),
        change_lines,
        stat,
        patch
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("building http client")?;

    let system = "You are a git commit assistant. Produce informative, specific commit messages that mirror the repo's tone. Respond strictly as JSON with keys \"subject\" and \"body\". Subject <=72 chars, sentence case, no trailing period. Body must be 2-5 bullets starting with '- ', focusing on concrete changes and motivations; mention new commands/flags/examples, doc sections touched, and any behavioral impacts.";

    let payload = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": prompt }
        ],
        "response_format": { "type": "json_object" },
        "temperature": 0.25,
        "max_tokens": 480
    });

    let res = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(key)
        .json(&payload)
        .send()
        .context("calling OpenAI API")?;

    if !res.status().is_success() {
        bail!("OpenAI API error: {}", res.status());
    }

    let parsed: ChatResponse = res.json().context("parsing OpenAI response")?;
    let choice = parsed.choices.into_iter().next();
    let content = match choice {
        Some(c) => c.message.content,
        None => return Ok(None),
    };

    let json_blob = sanitize_json_blob(&content).ok_or_else(|| {
        anyhow!(
            "AI response missing JSON object: {}",
            content.chars().take(200).collect::<String>()
        )
    })?;

    let ai: serde_json::Value = serde_json::from_str(&json_blob).context("decoding AI json")?;
    let subject = coerce_subject(ai.get("subject"))
        .ok_or_else(|| anyhow!("AI JSON missing usable subject"))?;
    let body = coerce_body(ai.get("body"));

    if subject.is_empty() {
        return Ok(None);
    }

    Ok(Some((subject, body)))
}

fn create_commit(subject: &str, body: &str) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("commit").arg("-m").arg(subject);
    if !body.trim().is_empty() {
        cmd.arg("-m").arg(body);
    }
    let status = cmd.status().context("running git commit")?;
    if !status.success() {
        bail!("git commit failed");
    }
    Ok(())
}

fn upstream_branch() -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() { Ok(None) } else { Ok(Some(s)) }
        }
        _ => Ok(None),
    }
}

fn ahead_behind(upstream: &str) -> Result<(u32, u32)> {
    let range = format!("HEAD...{}", upstream);
    let out = git_output(&["rev-list", "--left-right", "--count", &range])?;
    let mut parts = out.split_whitespace();
    let ahead = parts
        .next()
        .ok_or_else(|| anyhow!("unexpected rev-list output"))?
        .parse()
        .unwrap_or(0);
    let behind = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Ok((ahead, behind))
}

fn git(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("running git {:?}", args))?;
    if !status.success() {
        bail!("git {:?} failed", args);
    }
    Ok(())
}

fn git_output(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("running git {:?}", args))?;
    if !output.status.success() {
        bail!("git {:?} failed", args);
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
