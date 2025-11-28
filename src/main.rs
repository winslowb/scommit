use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
    let (subject, body) = match cli.message {
        Some(subject) => (subject, build_body(&changes, &stats)),
        None => build_commit_message(&changes, &stats),
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
