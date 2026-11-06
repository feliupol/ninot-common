//! Bare repositories on disk and git's smart HTTP protocol through `git http-backend`.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Author and committer of the commits a fake makes itself, such as a new repository's
/// README. Which they are is the fake's business, so the caller says.
#[derive(Clone, Debug)]
pub struct Bot {
    pub name: &'static str,
    pub email: &'static str,
}

/// A `git` command isolated from the user's and system's configuration.
fn command(git_dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .stdin(Stdio::null());
    command
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

fn check(args: &[&str], output: &Output) -> io::Result<()> {
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Wraps a gitoxide error as an I/O error.
fn other(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(error)
}

/// Opens a bare repository, ignoring the user's and system's configuration.
fn open(git_dir: &Path) -> io::Result<gix::Repository> {
    gix::open_opts(git_dir, gix::open::Options::isolated()).map_err(other)
}

fn object_id(sha: &str) -> io::Result<gix::ObjectId> {
    gix::ObjectId::from_hex(sha.as_bytes()).map_err(other)
}

/// Creates an empty bare repository whose `HEAD` points at `branch`.
pub fn init_bare(git_dir: &Path, branch: &str) -> io::Result<()> {
    gix::init_bare(git_dir).map_err(other)?;
    set_head(git_dir, branch)
}

/// Points `HEAD` at `branch`, which need not exist yet.
pub fn set_head(git_dir: &Path, branch: &str) -> io::Result<()> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit};
    let target: gix::refs::FullName = format!("refs/heads/{branch}").try_into().map_err(other)?;
    open(git_dir)?
        .edit_reference(RefEdit {
            change: Change::Update {
                log: LogChange::default(),
                expected: PreviousValue::Any,
                new: gix::refs::Target::Symbolic(target),
            },
            name: "HEAD".try_into().map_err(other)?,
            deref: false,
        })
        .map_err(other)
        .map(drop)
}

/// Commits a README titled `title` on `branch`, as a fake initialising a repository does.
pub fn commit_readme(git_dir: &Path, branch: &str, title: &str, bot: &Bot) -> io::Result<()> {
    let repo = open(git_dir)?;
    let blob = repo
        .write_blob(format!("# {title}\n"))
        .map_err(other)?
        .detach();
    let tree = gix::objs::Tree {
        entries: vec![gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "README.md".into(),
            oid: blob,
        }],
    };
    let tree = repo.write_object(&tree).map_err(other)?.detach();
    let bot = gix::actor::Signature {
        name: bot.name.into(),
        email: bot.email.into(),
        time: gix::date::Time::now_local_or_utc(),
    };
    let (mut committer_time, mut author_time) = (
        gix::date::parse::TimeBuf::default(),
        gix::date::parse::TimeBuf::default(),
    );
    repo.commit_as(
        bot.to_ref(&mut committer_time),
        bot.to_ref(&mut author_time),
        format!("refs/heads/{branch}").as_str(),
        "Initial commit\n",
        tree,
        Vec::<gix::ObjectId>::new(),
    )
    .map_err(other)
    .map(drop)
}

/// A smart HTTP request addressed to a repository.
pub struct GitPath<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    /// `info/refs`, `git-upload-pack` or `git-receive-pack`.
    pub service_path: &'a str,
}

const INFO_REFS: &str = "info/refs";
const UPLOAD_PACK: &str = "git-upload-pack";
const RECEIVE_PACK: &str = "git-receive-pack";
/// The `info/refs` query parameter naming the service.
const SERVICE_PARAM: &str = "service";

/// Recognizes `/{owner}/{repo}[.git]/{info/refs|git-upload-pack|git-receive-pack}`.
pub fn parse_path(path: &str) -> Option<GitPath<'_>> {
    let rest = path.strip_prefix('/')?;
    let (owner, rest) = rest.split_once('/')?;
    let (repo, service_path) = rest.split_once('/')?;
    if !matches!(service_path, INFO_REFS | UPLOAD_PACK | RECEIVE_PACK) || owner.is_empty() {
        return None;
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (!repo.is_empty()).then_some(GitPath {
        owner,
        repo,
        service_path,
    })
}

/// Whether a smart HTTP request pushes.
pub fn is_push(service_path: &str, query: &str) -> bool {
    service_path == RECEIVE_PACK
        || url::form_urlencoded::parse(query.as_bytes())
            .any(|(key, value)| key == SERVICE_PARAM && value == RECEIVE_PACK)
}

/// The CGI response of `git http-backend`.
pub struct CgiResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Request details `git http-backend` needs.
pub struct CgiRequest<'a> {
    pub method: &'a str,
    pub service_path: &'a str,
    pub query: &'a str,
    pub content_type: Option<&'a str>,
    pub content_encoding: Option<&'a str>,
    pub git_protocol: Option<&'a str>,
    pub remote_user: Option<&'a str>,
    /// Serve the repository even without git's `git-daemon-export-ok` marker. Off by default; the caller must gate access itself.
    pub export_all: bool,
    pub body: &'a [u8],
}

/// Serves one smart HTTP request for the bare repository `git_dir`.
///
/// `REMOTE_USER` from `request.remote_user` is passed through to git unchecked,
/// so the caller must authenticate the client before calling. Repositories
/// without `git-daemon-export-ok` are refused unless `request.export_all` is set.
pub fn http_backend(git_dir: &Path, request: &CgiRequest) -> io::Result<CgiResponse> {
    let root = git_dir.parent().expect("repository has a parent directory");
    let name = git_dir.file_name().unwrap().to_string_lossy();
    let mut command = command(git_dir);
    command
        .env_remove("GIT_DIR")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("PATH_INFO", format!("/{name}/{}", request.service_path))
        .env("REQUEST_METHOD", request.method)
        .env("QUERY_STRING", request.query)
        .env("CONTENT_LENGTH", request.body.len().to_string())
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if request.export_all {
        command.env("GIT_HTTP_EXPORT_ALL", "1");
    }
    for (name, value) in [
        ("CONTENT_TYPE", request.content_type),
        ("HTTP_CONTENT_ENCODING", request.content_encoding),
        ("GIT_PROTOCOL", request.git_protocol),
        ("REMOTE_USER", request.remote_user),
    ] {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let body = request.body.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&body));
    let output = child.wait_with_output()?;
    writer.join().expect("stdin writer panicked")?;
    check(&["http-backend"], &output)?;
    Ok(parse_cgi(&output.stdout))
}

fn parse_cgi(output: &[u8]) -> CgiResponse {
    let (head, body) = [&b"\r\n\r\n"[..], b"\n\n"]
        .iter()
        .find_map(|separator| {
            output
                .windows(separator.len())
                .position(|window| window == *separator)
                .map(|at| (&output[..at], &output[at + separator.len()..]))
        })
        .unwrap_or((output, &[]));
    let mut status = 200;
    let mut headers = Vec::new();
    for line in String::from_utf8_lossy(head).lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            status = value
                .split_whitespace()
                .next()
                .and_then(|code| code.parse().ok())
                .unwrap_or(500);
        } else {
            headers.push((name.to_owned(), value.to_owned()));
        }
    }
    CgiResponse {
        status,
        headers,
        body: body.to_vec(),
    }
}

/// A ref and the object it points at.
pub struct Ref {
    /// Full name, such as `refs/heads/main`.
    pub name: String,
    pub sha: String,
    /// `commit`, `tag`, `tree` or `blob`.
    pub kind: String,
}

/// Every ref whose full name starts with `prefix`, sorted by name.
pub fn refs(git_dir: &Path, prefix: &str) -> io::Result<Vec<Ref>> {
    let repo = open(git_dir)?;
    let platform = repo.references().map_err(other)?;
    let mut refs = Vec::new();
    for reference in platform.all().map_err(other)? {
        let reference = reference.map_err(|error| io::Error::other(error.to_string()))?;
        let name = reference.name().as_bstr().to_string();
        let Some(id) = reference.target().try_id().map(ToOwned::to_owned) else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        let kind = repo.find_header(id).map_err(other)?.kind();
        refs.push(Ref {
            name,
            sha: id.to_string(),
            kind: kind.to_string(),
        });
    }
    refs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(refs)
}

/// Whether the repository has no refs at all.
pub fn is_empty(git_dir: &Path) -> io::Result<bool> {
    Ok(refs(git_dir, "refs/")?.is_empty())
}

/// Resolves a branch, tag or sha to a commit sha.
pub fn resolve_commit(git_dir: &Path, reference: &str) -> io::Result<Option<String>> {
    if reference.starts_with('-') {
        return Ok(None);
    }
    let repo = open(git_dir)?;
    Ok(repo
        .rev_parse_single(format!("{reference}^{{commit}}").as_str())
        .ok()
        .map(|id| id.to_string()))
}

/// Author or committer of a commit.
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Strict ISO 8601, such as `2026-09-29T20:38:27+02:00`.
    pub date: String,
}

/// The parts of a commit the API reports.
pub struct Commit {
    pub sha: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

fn signature(signature: gix::actor::SignatureRef<'_>) -> io::Result<Signature> {
    let time = signature.time().map_err(other)?;
    Ok(Signature {
        name: signature.name.to_string(),
        email: signature.email.to_string(),
        date: time.format_or_unix(gix::date::time::format::ISO8601_STRICT),
    })
}

/// Reads the commit `sha`.
pub fn commit(git_dir: &Path, sha: &str) -> io::Result<Commit> {
    let repo = open(git_dir)?;
    let commit = repo.find_commit(object_id(sha)?).map_err(other)?;
    let decoded = commit.decode().map_err(other)?;
    Ok(Commit {
        sha: commit.id.to_string(),
        tree: decoded.tree().to_string(),
        parents: decoded.parents().map(|parent| parent.to_string()).collect(),
        author: signature(decoded.author().map_err(other)?)?,
        committer: signature(decoded.committer().map_err(other)?)?,
        message: decoded.message.to_string().trim_end().to_owned(),
    })
}

/// An entry of a tree.
pub struct TreeEntry {
    /// `blob`, `tree` or `commit` (a submodule).
    pub kind: String,
    pub mode: String,
    pub sha: String,
    pub size: u64,
    /// Path from the repository root.
    pub path: String,
}

/// What `path` is at `commit`: a single entry for a file, the children for a directory.
pub enum PathContent {
    File(TreeEntry),
    Directory(Vec<TreeEntry>),
}

/// Describes one tree entry as `git ls-tree --long` does: blobs carry their size, trees and
/// submodules report zero.
fn tree_entry(
    repo: &gix::Repository,
    mode: gix::objs::tree::EntryMode,
    id: gix::ObjectId,
    path: String,
) -> io::Result<TreeEntry> {
    let (kind, size) = if mode.is_tree() {
        ("tree", 0)
    } else if mode.is_commit() {
        ("commit", 0)
    } else {
        ("blob", repo.find_header(id).map_err(other)?.size())
    };
    Ok(TreeEntry {
        kind: kind.to_owned(),
        mode: format!("{:06o}", mode.value()),
        sha: id.to_string(),
        size,
        path,
    })
}

/// Looks up `path` in the tree of `commit`; `None` when it does not exist.
pub fn path_content(git_dir: &Path, commit: &str, path: &str) -> io::Result<Option<PathContent>> {
    let path = path.trim_matches('/');
    let repo = open(git_dir)?;
    let mut tree = repo
        .find_commit(object_id(commit)?)
        .map_err(other)?
        .tree()
        .map_err(other)?;
    let Some(entry) = tree.peel_to_entry_by_path(path).map_err(other)? else {
        return Ok(None);
    };
    let (mode, id) = (entry.mode(), entry.object_id());
    if !mode.is_tree() {
        return Ok(Some(PathContent::File(tree_entry(
            &repo,
            mode,
            id,
            path.to_owned(),
        )?)));
    }
    let directory = repo.find_tree(id).map_err(other)?;
    let mut children = Vec::new();
    for child in directory.iter() {
        let child = child.map_err(other)?;
        children.push(tree_entry(
            &repo,
            child.mode(),
            child.object_id(),
            format!("{path}/{}", child.filename()),
        )?);
    }
    Ok(Some(PathContent::Directory(children)))
}

/// Points `name` at `sha`, creating the ref and recording `bot` in the reflog.
pub fn create_ref(git_dir: &Path, name: &str, sha: &str, bot: &Bot) -> io::Result<()> {
    open(git_dir)?
        .reference(
            name,
            object_id(sha)?,
            gix::refs::transaction::PreviousValue::Any,
            format!("{}: create ref", bot.name),
        )
        .map_err(other)
        .map(drop)
}

/// The bytes of the blob `sha`.
pub fn blob(git_dir: &Path, sha: &str) -> io::Result<Vec<u8>> {
    let repo = open(git_dir)?;
    let blob = repo.find_blob(object_id(sha)?).map_err(other)?;
    Ok(blob.data.clone())
}

/// Commits reachable from `new` but from none of `excluded`, oldest first.
pub fn new_commits(git_dir: &Path, new: &str, excluded: &[&str]) -> io::Result<Vec<String>> {
    let repo = open(git_dir)?;
    let hidden = excluded
        .iter()
        .map(|sha| object_id(sha))
        .collect::<io::Result<Vec<_>>>()?;
    let walk = repo
        .rev_walk([object_id(new)?])
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            Default::default(),
        ))
        .with_hidden(hidden)
        .all()
        .map_err(other)?;
    let mut shas = Vec::new();
    for info in walk {
        shas.push(info.map_err(other)?.id.to_string());
    }
    shas.reverse();
    Ok(shas)
}

/// Paths a commit added, removed and modified relative to its parent, as
/// `git diff-tree --root -r --name-status` lists them: nothing for merge commits, every file
/// for a root commit.
pub fn changed_paths(
    git_dir: &Path,
    sha: &str,
) -> io::Result<(Vec<String>, Vec<String>, Vec<String>)> {
    use gix::object::tree::diff::ChangeDetached;
    let (mut added, mut removed, mut modified) = (Vec::new(), Vec::new(), Vec::new());
    let repo = open(git_dir)?;
    let commit = repo.find_commit(object_id(sha)?).map_err(other)?;
    let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
    if parents.len() > 1 {
        return Ok((added, removed, modified));
    }
    let new_tree = commit.tree().map_err(other)?;
    let old_tree = match parents.first() {
        Some(parent) => repo
            .find_commit(*parent)
            .map_err(other)?
            .tree()
            .map_err(other)?,
        None => repo.empty_tree(),
    };
    let mut options = gix::diff::Options::default().with_rewrites(None);
    options.track_path();
    let changes = repo
        .diff_tree_to_tree(&old_tree, &new_tree, options)
        .map_err(other)?;
    for change in changes {
        if change.entry_mode().is_tree() {
            continue;
        }
        let path = change.location().to_string();
        match change {
            ChangeDetached::Addition { .. } => added.push(path),
            ChangeDetached::Deletion { .. } => removed.push(path),
            ChangeDetached::Modification { .. } | ChangeDetached::Rewrite { .. } => {
                modified.push(path)
            }
        }
    }
    Ok((added, removed, modified))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_smart_http_paths() {
        let path = parse_path("/alice/tool.git/info/refs").unwrap();
        assert_eq!(
            (path.owner, path.repo, path.service_path),
            ("alice", "tool", "info/refs")
        );
        assert_eq!(
            parse_path("/alice/tool/git-receive-pack").unwrap().repo,
            "tool"
        );
        assert!(parse_path("/repos/alice/tool").is_none());
        assert!(parse_path("/alice/tool.git/objects/info/packs").is_none());
        assert!(is_push("info/refs", "service=git-receive-pack"));
        assert!(!is_push("info/refs", "service=git-upload-pack"));
    }

    #[test]
    fn parses_cgi_status_and_headers() {
        let response = parse_cgi(b"Status: 403 Forbidden\r\nContent-Type: text/plain\r\n\r\nno");
        assert_eq!(response.status, 403);
        assert_eq!(
            response.headers,
            [("Content-Type".to_owned(), "text/plain".to_owned())]
        );
        assert_eq!(response.body, b"no");
    }
}
