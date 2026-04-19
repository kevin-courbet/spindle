//! Transport-independent issue backend. The Mac inspector and orchestrator agents
//! should never shell out to `gh`/`glab` directly — every issue interaction goes
//! through a `dyn IssueTransport` so our orchestration layer can observe the state
//! transitions and emit coordinated events.
//!
//! Three impls ship today:
//! - `GithubTransport` — shells `gh` from the project cwd (GitHub remote)
//! - `GitlabTransport` — shells `glab` from the project cwd (GitLab remote)
//! - `LocalTransport` — reads/writes `.threadmill/issues/<n>.md` with YAML
//!   frontmatter, so repos with no external tracker (or fully offline ones)
//!   still get the same surface.
//!
//! Factory `for_project(project, explicit)` picks the impl: explicit
//! `.threadmill.yml` config wins, else git-remote detection, else Local as a
//! never-fails fallback.

use std::path::PathBuf;

use async_trait::async_trait;
use tokio::process::Command;

use crate::protocol;

// ---------- Public types ----------

/// Full live view of an issue. Wraps the wire-format `WorkflowIssueRef` (number,
/// title, url, author, created_at, body_preview) with the extra fields the
/// inspector needs: state, labels, assignees, and the full body.
#[derive(Debug, Clone)]
pub struct EnrichedIssue {
    pub r#ref: protocol::WorkflowIssueRef,
    pub state: IssueState,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub body: Option<String>,
}

// `IssueState` now lives in `crate::protocol` so the wire format can reference it
// directly. Re-exported here for existing call-sites.
pub use crate::protocol::IssueState;

pub(crate) fn issue_state_parse(s: &str) -> IssueState {
    match s.to_lowercase().as_str() {
        "closed" => IssueState::Closed,
        _ => IssueState::Open,
    }
}

pub(crate) fn issue_state_as_str(state: &IssueState) -> &'static str {
    match state {
        IssueState::Open => "open",
        IssueState::Closed => "closed",
    }
}

#[derive(Debug, Clone)]
pub struct IssueDraft {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
}

// ---------- Trait ----------

#[async_trait]
pub trait IssueTransport: Send + Sync {
    /// Kind identifier for the returned `WorkflowIssuePlatform` in list responses.
    fn kind(&self) -> protocol::WorkflowIssuePlatform;

    /// List open issues, newest first, capped at `limit`. An empty `label`
    /// means "no filter" (all issues); any non-empty value filters to issues
    /// carrying that label. All transports list OPEN issues only; closed-issue
    /// filtering is post-processed by callers that need it.
    async fn list(
        &self,
        label: &str,
        limit: u32,
    ) -> Result<Vec<protocol::WorkflowIssueRef>, String>;

    /// Resolve a single issue by URL / local id with its full live state.
    async fn resolve(&self, url: &str) -> Result<Option<EnrichedIssue>, String>;

    /// Post a comment on an issue. Some transports (Local) implement this as
    /// appending to the issue's markdown body under a `## Comments` heading.
    async fn comment(&self, url: &str, body: &str) -> Result<(), String>;

    /// Close an issue. `reason` is a free-form one-line reason posted as a
    /// comment before closing (best-effort — skipped if empty).
    async fn close(&self, url: &str, reason: Option<&str>) -> Result<(), String>;

    /// Create a new issue. Returns the canonical URL + number.
    async fn create(&self, draft: IssueDraft) -> Result<protocol::WorkflowIssueRef, String>;
}

// ---------- Factory ----------

/// Resolve the transport for a given project path. `explicit_kind` comes from
/// the project's `.threadmill.yml` `issue_transport` field if configured;
/// otherwise we detect from the git remote and fall back to Local.
pub async fn for_project(
    project_path: &str,
    explicit_kind: Option<&str>,
) -> Box<dyn IssueTransport> {
    let kind = match explicit_kind.map(str::to_lowercase) {
        Some(k) if !k.is_empty() => k,
        _ => detect_kind_from_remote(project_path).await,
    };

    match kind.as_str() {
        "github" => Box::new(GithubTransport {
            project_path: project_path.to_string(),
        }),
        "gitlab" => Box::new(GitlabTransport {
            project_path: project_path.to_string(),
        }),
        _ => Box::new(LocalTransport {
            project_path: PathBuf::from(project_path),
        }),
    }
}

async fn detect_kind_from_remote(project_path: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["remote", "get-url", "origin"])
        .output()
        .await;
    let Ok(output) = output else {
        return "local".to_string();
    };
    if !output.status.success() {
        return "local".to_string();
    }
    let url = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_lowercase();
    if url.contains("github.com") {
        "github".to_string()
    } else if url.contains("gitlab.com") || url.contains("//gitlab.") || url.contains("@gitlab.") {
        "gitlab".to_string()
    } else {
        "local".to_string()
    }
}

// ---------- GithubTransport ----------

pub struct GithubTransport {
    pub project_path: String,
}

#[async_trait]
impl IssueTransport for GithubTransport {
    fn kind(&self) -> protocol::WorkflowIssuePlatform {
        protocol::WorkflowIssuePlatform::Github
    }

    async fn list(
        &self,
        label: &str,
        limit: u32,
    ) -> Result<Vec<protocol::WorkflowIssueRef>, String> {
        let limit_str = limit.to_string();
        let mut args: Vec<&str> = vec!["issue", "list", "--state", "open"];
        if !label.is_empty() {
            args.push("--label");
            args.push(label);
        }
        args.extend_from_slice(&[
            "--limit",
            &limit_str,
            "--json",
            "number,title,url,createdAt,author,body",
        ]);
        let output = Command::new("gh")
            .current_dir(&self.project_path)
            .args(&args)
            .output()
            .await
            .map_err(|err| format!("gh issue list failed to start: {err}"))?;

        if !output.status.success() {
            return Err(format!(
                "gh issue list failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let raw: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("gh issue list: invalid JSON: {err}"))?;
        Ok(raw.into_iter().map(gh_row_to_ref).collect())
    }

    async fn resolve(&self, url: &str) -> Result<Option<EnrichedIssue>, String> {
        let output = Command::new("gh")
            .current_dir(&self.project_path)
            .args([
                "issue",
                "view",
                url,
                "--json",
                "number,title,url,state,labels,assignees,createdAt,author,body",
            ])
            .output()
            .await
            .map_err(|err| format!("gh issue view failed to start: {err}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let row: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("gh issue view: invalid JSON: {err}"))?;
        Ok(Some(gh_row_to_enriched(row)))
    }

    async fn comment(&self, url: &str, body: &str) -> Result<(), String> {
        let output = Command::new("gh")
            .current_dir(&self.project_path)
            .args(["issue", "comment", url, "--body", body])
            .output()
            .await
            .map_err(|err| format!("gh issue comment failed to start: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "gh issue comment failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    async fn close(&self, url: &str, reason: Option<&str>) -> Result<(), String> {
        if let Some(reason) = reason {
            if !reason.trim().is_empty() {
                let _ = self.comment(url, reason).await;
            }
        }
        let output = Command::new("gh")
            .current_dir(&self.project_path)
            .args(["issue", "close", url])
            .output()
            .await
            .map_err(|err| format!("gh issue close failed to start: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "gh issue close failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    async fn create(&self, draft: IssueDraft) -> Result<protocol::WorkflowIssueRef, String> {
        let mut args = vec![
            "issue".to_string(),
            "create".to_string(),
            "--title".to_string(),
            draft.title.clone(),
            "--body".to_string(),
            draft.body.clone(),
        ];
        for label in &draft.labels {
            args.push("--label".to_string());
            args.push(label.clone());
        }
        for assignee in &draft.assignees {
            args.push("--assignee".to_string());
            args.push(assignee.clone());
        }
        let output = Command::new("gh")
            .current_dir(&self.project_path)
            .args(&args)
            .output()
            .await
            .map_err(|err| format!("gh issue create failed to start: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "gh issue create failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        // `gh issue create` prints the new issue URL as the last line of stdout.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let url = stdout
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(protocol::WorkflowIssueRef {
            number,
            title: draft.title,
            url,
            author: None,
            created_at: None,
            body_preview: Some(truncate_preview(&draft.body)),
        })
    }
}

fn gh_row_to_ref(row: serde_json::Value) -> protocol::WorkflowIssueRef {
    protocol::WorkflowIssueRef {
        number: row
            .get("number")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        title: row
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        url: row
            .get("url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        author: row
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        created_at: row
            .get("createdAt")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        body_preview: row
            .get("body")
            .and_then(serde_json::Value::as_str)
            .map(truncate_preview),
    }
}

fn gh_row_to_enriched(row: serde_json::Value) -> EnrichedIssue {
    let base = gh_row_to_ref(row.clone());
    let state = row
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(issue_state_parse)
        .unwrap_or(IssueState::Open);
    let labels = row
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let assignees = row
        .get("assignees")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("login").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let body = row
        .get("body")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    EnrichedIssue {
        r#ref: base,
        state,
        labels,
        assignees,
        body,
    }
}

// ---------- GitlabTransport ----------

pub struct GitlabTransport {
    pub project_path: String,
}

#[async_trait]
impl IssueTransport for GitlabTransport {
    fn kind(&self) -> protocol::WorkflowIssuePlatform {
        protocol::WorkflowIssuePlatform::Gitlab
    }

    async fn list(
        &self,
        label: &str,
        limit: u32,
    ) -> Result<Vec<protocol::WorkflowIssueRef>, String> {
        let limit_str = limit.to_string();
        let mut args: Vec<&str> = vec!["issue", "list", "--opened"];
        if !label.is_empty() {
            args.push("--label");
            args.push(label);
        }
        args.extend_from_slice(&["--per-page", &limit_str, "--output", "json"]);
        let output = Command::new("glab")
            .current_dir(&self.project_path)
            .args(&args)
            .output()
            .await
            .map_err(|err| format!("glab issue list failed to start: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "glab issue list failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let raw: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("glab issue list: invalid JSON: {err}"))?;
        Ok(raw.into_iter().map(glab_row_to_ref).collect())
    }

    async fn resolve(&self, url: &str) -> Result<Option<EnrichedIssue>, String> {
        let output = Command::new("glab")
            .current_dir(&self.project_path)
            .args(["issue", "view", url, "--output", "json"])
            .output()
            .await
            .map_err(|err| format!("glab issue view failed to start: {err}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let row: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("glab issue view: invalid JSON: {err}"))?;
        Ok(Some(glab_row_to_enriched(row)))
    }

    async fn comment(&self, url: &str, body: &str) -> Result<(), String> {
        let output = Command::new("glab")
            .current_dir(&self.project_path)
            .args(["issue", "note", url, "--message", body])
            .output()
            .await
            .map_err(|err| format!("glab issue note failed to start: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "glab issue note failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    async fn close(&self, url: &str, reason: Option<&str>) -> Result<(), String> {
        if let Some(reason) = reason {
            if !reason.trim().is_empty() {
                let _ = self.comment(url, reason).await;
            }
        }
        let output = Command::new("glab")
            .current_dir(&self.project_path)
            .args(["issue", "close", url])
            .output()
            .await
            .map_err(|err| format!("glab issue close failed to start: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "glab issue close failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    async fn create(&self, draft: IssueDraft) -> Result<protocol::WorkflowIssueRef, String> {
        let mut args = vec![
            "issue".to_string(),
            "create".to_string(),
            "--title".to_string(),
            draft.title.clone(),
            "--description".to_string(),
            draft.body.clone(),
        ];
        if !draft.labels.is_empty() {
            args.push("--label".to_string());
            args.push(draft.labels.join(","));
        }
        if !draft.assignees.is_empty() {
            args.push("--assignee".to_string());
            args.push(draft.assignees.join(","));
        }
        let output = Command::new("glab")
            .current_dir(&self.project_path)
            .args(&args)
            .output()
            .await
            .map_err(|err| format!("glab issue create failed to start: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "glab issue create failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let url = stdout
            .lines()
            .rev()
            .find(|l| l.starts_with("http"))
            .unwrap_or("")
            .trim()
            .to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(protocol::WorkflowIssueRef {
            number,
            title: draft.title,
            url,
            author: None,
            created_at: None,
            body_preview: Some(truncate_preview(&draft.body)),
        })
    }
}

fn glab_row_to_ref(row: serde_json::Value) -> protocol::WorkflowIssueRef {
    protocol::WorkflowIssueRef {
        number: row
            .get("iid")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        title: row
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        url: row
            .get("web_url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        author: row
            .get("author")
            .and_then(|a| a.get("username"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        created_at: row
            .get("created_at")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        body_preview: row
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(truncate_preview),
    }
}

fn glab_row_to_enriched(row: serde_json::Value) -> EnrichedIssue {
    let base = glab_row_to_ref(row.clone());
    let state = row
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(issue_state_parse)
        .unwrap_or(IssueState::Open);
    let labels = row
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let assignees = row
        .get("assignees")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("username").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let body = row
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    EnrichedIssue {
        r#ref: base,
        state,
        labels,
        assignees,
        body,
    }
}

// ---------- LocalTransport ----------
//
// On-disk layout: `<project>/.threadmill/issues/<n>.md`. Each file has a YAML
// frontmatter block followed by the markdown body. Example:
//
//     ---
//     number: 42
//     title: Add auth middleware
//     state: open
//     labels: [prd, auth]
//     assignees: []
//     author: kevin
//     created_at: 2026-04-17T12:00:00Z
//     ---
//
//     ## Problem
//     …body…
//
// URLs are of the form `threadmill-local://<n>` and map to `<issues-dir>/<n>.md`.

const LOCAL_URL_PREFIX: &str = "threadmill-local://";

pub struct LocalTransport {
    pub project_path: PathBuf,
}

impl LocalTransport {
    fn issues_dir(&self) -> PathBuf {
        self.project_path.join(".threadmill").join("issues")
    }

    fn path_for_number(&self, number: u64) -> PathBuf {
        self.issues_dir().join(format!("{number}.md"))
    }

    fn number_from_url(url: &str) -> Option<u64> {
        let rest = url.strip_prefix(LOCAL_URL_PREFIX)?;
        rest.trim_end_matches('/').parse::<u64>().ok()
    }

    fn url_for_number(number: u64) -> String {
        format!("{LOCAL_URL_PREFIX}{number}")
    }

    fn next_number(&self) -> u64 {
        let dir = self.issues_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return 1;
        };
        let max = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0);
        max + 1
    }
}

#[async_trait]
impl IssueTransport for LocalTransport {
    fn kind(&self) -> protocol::WorkflowIssuePlatform {
        protocol::WorkflowIssuePlatform::Unknown
    }

    async fn list(
        &self,
        label: &str,
        limit: u32,
    ) -> Result<Vec<protocol::WorkflowIssueRef>, String> {
        let dir = self.issues_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut issues = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(parsed) = parse_local_issue_file(&contents) else {
                continue;
            };
            if parsed.state != IssueState::Open {
                continue;
            }
            if !label.is_empty() && !parsed.labels.iter().any(|l| l == label) {
                continue;
            }
            issues.push(parsed.into_ref());
        }
        issues.sort_by(|a, b| b.number.cmp(&a.number));
        issues.truncate(limit as usize);
        Ok(issues)
    }

    async fn resolve(&self, url: &str) -> Result<Option<EnrichedIssue>, String> {
        let Some(number) = Self::number_from_url(url) else {
            return Ok(None);
        };
        let path = self.path_for_number(number);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        let Some(parsed) = parse_local_issue_file(&contents) else {
            return Ok(None);
        };
        Ok(Some(parsed.into_enriched()))
    }

    async fn comment(&self, url: &str, body: &str) -> Result<(), String> {
        let Some(number) = Self::number_from_url(url) else {
            return Err(format!("not a local issue URL: {url}"));
        };
        let path = self.path_for_number(number);
        let contents = std::fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let ts = chrono::Utc::now().to_rfc3339();
        let appended = format!(
            "{}\n\n---\n_Comment at {ts}:_\n\n{}\n",
            contents.trim_end(),
            body
        );
        std::fs::write(&path, appended)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        Ok(())
    }

    async fn close(&self, url: &str, reason: Option<&str>) -> Result<(), String> {
        if let Some(reason) = reason {
            if !reason.trim().is_empty() {
                let _ = self.comment(url, reason).await;
            }
        }
        let Some(number) = Self::number_from_url(url) else {
            return Err(format!("not a local issue URL: {url}"));
        };
        let path = self.path_for_number(number);
        let contents = std::fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let Some(mut parsed) = parse_local_issue_file(&contents) else {
            return Err(format!("invalid issue file: {}", path.display()));
        };
        parsed.state = IssueState::Closed;
        let serialized = parsed.serialize();
        std::fs::write(&path, serialized)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        Ok(())
    }

    async fn create(&self, draft: IssueDraft) -> Result<protocol::WorkflowIssueRef, String> {
        let dir = self.issues_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
        let number = self.next_number();
        let now = chrono::Utc::now().to_rfc3339();
        let parsed = LocalIssue {
            number,
            title: draft.title.clone(),
            url: Self::url_for_number(number),
            state: IssueState::Open,
            labels: draft.labels,
            assignees: draft.assignees,
            author: None,
            created_at: Some(now),
            body: draft.body,
        };
        let path = self.path_for_number(number);
        std::fs::write(&path, parsed.serialize())
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        Ok(parsed.into_ref())
    }
}

struct LocalIssue {
    number: u64,
    title: String,
    url: String,
    state: IssueState,
    labels: Vec<String>,
    assignees: Vec<String>,
    author: Option<String>,
    created_at: Option<String>,
    body: String,
}

impl LocalIssue {
    fn into_ref(self) -> protocol::WorkflowIssueRef {
        protocol::WorkflowIssueRef {
            number: self.number,
            title: self.title,
            url: self.url,
            author: self.author,
            created_at: self.created_at,
            body_preview: Some(truncate_preview(&self.body)),
        }
    }

    fn into_enriched(self) -> EnrichedIssue {
        let base = protocol::WorkflowIssueRef {
            number: self.number,
            title: self.title.clone(),
            url: self.url.clone(),
            author: self.author.clone(),
            created_at: self.created_at.clone(),
            body_preview: Some(truncate_preview(&self.body)),
        };
        EnrichedIssue {
            r#ref: base,
            state: self.state,
            labels: self.labels,
            assignees: self.assignees,
            body: Some(self.body),
        }
    }

    fn serialize(&self) -> String {
        let labels = self
            .labels
            .iter()
            .map(|l| format!("\"{l}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let assignees = self
            .assignees
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let mut out = String::from("---\n");
        out.push_str(&format!("number: {}\n", self.number));
        out.push_str(&format!("title: \"{}\"\n", escape_yaml(&self.title)));
        out.push_str(&format!("state: {}\n", issue_state_as_str(&self.state)));
        out.push_str(&format!("labels: [{labels}]\n"));
        out.push_str(&format!("assignees: [{assignees}]\n"));
        if let Some(author) = &self.author {
            out.push_str(&format!("author: \"{}\"\n", escape_yaml(author)));
        }
        if let Some(created_at) = &self.created_at {
            out.push_str(&format!("created_at: {created_at}\n"));
        }
        out.push_str("---\n\n");
        out.push_str(&self.body);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }
}

fn parse_local_issue_file(contents: &str) -> Option<LocalIssue> {
    let after = contents.strip_prefix("---\n")?;
    let end = after.find("\n---\n")?;
    let frontmatter = &after[..end];
    let body = after[end + 5..].trim_start().to_string();

    let mut number = 0u64;
    let mut title = String::new();
    let mut state = IssueState::Open;
    let mut labels = Vec::new();
    let mut assignees = Vec::new();
    let mut author = None;
    let mut created_at = None;

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("number:") {
            number = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("title:") {
            title = unquote_yaml(v.trim()).to_string();
        } else if let Some(v) = line.strip_prefix("state:") {
            state = issue_state_parse(v.trim());
        } else if let Some(v) = line.strip_prefix("labels:") {
            labels = parse_yaml_list(v.trim());
        } else if let Some(v) = line.strip_prefix("assignees:") {
            assignees = parse_yaml_list(v.trim());
        } else if let Some(v) = line.strip_prefix("author:") {
            let unq = unquote_yaml(v.trim());
            if !unq.is_empty() {
                author = Some(unq.to_string());
            }
        } else if let Some(v) = line.strip_prefix("created_at:") {
            let unq = v.trim();
            if !unq.is_empty() {
                created_at = Some(unq.to_string());
            }
        }
    }

    if number == 0 || title.is_empty() {
        return None;
    }
    Some(LocalIssue {
        number,
        title,
        url: LocalTransport::url_for_number(number),
        state,
        labels,
        assignees,
        author,
        created_at,
        body,
    })
}

fn parse_yaml_list(value: &str) -> Vec<String> {
    let trimmed = value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(',')
        .map(|s| unquote_yaml(s.trim()).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn unquote_yaml(value: &str) -> &str {
    let v = value.trim();
    if (v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')) {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

fn escape_yaml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------- Shared helpers ----------

pub(crate) fn truncate_preview(body: &str) -> String {
    const LIMIT: usize = 200;
    if body.len() <= LIMIT {
        return body.to_string();
    }
    let mut end = LIMIT;
    while !body.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_transport_round_trip_create_resolve_close() {
        let tmp = tempfile_dir();
        let transport = LocalTransport {
            project_path: tmp.clone(),
        };
        let draft = IssueDraft {
            title: "Add auth".to_string(),
            body: "## Problem\nNeed login.\n".to_string(),
            labels: vec!["prd".to_string()],
            assignees: vec!["kevin".to_string()],
        };
        let created = transport.create(draft).await.expect("create");
        assert_eq!(created.number, 1);
        assert_eq!(created.url, "threadmill-local://1");

        let listed = transport.list("prd", 10).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].number, 1);

        let resolved = transport
            .resolve(&created.url)
            .await
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved.state, IssueState::Open);
        assert_eq!(resolved.labels, vec!["prd"]);
        assert_eq!(resolved.assignees, vec!["kevin"]);

        transport
            .close(&created.url, Some("Shipped."))
            .await
            .expect("close");
        let closed = transport
            .resolve(&created.url)
            .await
            .expect("resolve")
            .expect("present");
        assert_eq!(closed.state, IssueState::Closed);

        // `list` filters to open only — closed issue falls out.
        let listed_after = transport.list("prd", 10).await.expect("list");
        assert!(listed_after.is_empty());

        std::fs::remove_dir_all(&tmp).ok();
    }

    fn tempfile_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "threadmill-issue-tests-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).expect("mkdir");
        path
    }
}
