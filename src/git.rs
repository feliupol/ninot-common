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

/// A `git` command isolated from the user's and system's configuration, which runs no hooks and
/// never prompts.
fn command(git_dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", null_device()))
        .env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_TERMINAL_PROMPT", "0")
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

/// Who makes a commit, and when.
#[derive(Clone, Copy, Debug)]
pub struct Identity<'a> {
    pub name: &'a str,
    pub email: &'a str,
    /// When, in a format git takes for `GIT_AUTHOR_DATE`, such as `2026-09-29T20:38:27+02:00`;
    /// `None` for now.
    pub date: Option<&'a str>,
}

impl From<&Bot> for Identity<'static> {
    fn from(bot: &Bot) -> Self {
        Self {
            name: bot.name,
            email: bot.email,
            date: None,
        }
    }
}

/// Why an operation was declined, as opposed to failing: something the caller asked for that the
/// repository does not allow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Both sides change these paths differently.
    Conflict(Vec<String>),
    /// The two commits share no history.
    Unrelated,
    /// A fast-forward was asked for, but the base has commits the head lacks.
    NotFastForward,
    /// The ref or revision named does not exist.
    Missing(String),
    /// The ref no longer points where the caller expected.
    Moved(String),
    /// The name is not a valid full ref name.
    InvalidRefName(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(paths) => write!(f, "merge conflict in {}", paths.join(", ")),
            Self::Unrelated => f.write_str("the histories are unrelated"),
            Self::NotFastForward => f.write_str("not possible to fast-forward"),
            Self::Missing(name) => write!(f, "{name} does not exist"),
            Self::Moved(name) => write!(f, "{name} moved"),
            Self::InvalidRefName(name) => write!(f, "{name} is not a valid ref name"),
        }
    }
}

/// How [`merge`] brings a head into a base branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStyle {
    /// A merge commit, even when a fast-forward would do (`git merge --no-ff`).
    Merge,
    /// One commit on the base with everything the head changes (`git merge --squash`).
    Squash,
    /// Moves the base to the head, refusing when it has commits the head lacks (`--ff-only`).
    FastForward,
    /// Replays the head's commits onto the base (`git rebase`), keeping their authors.
    Rebase,
    /// Replays the head's commits onto the base, then makes a merge commit of them.
    RebaseMerge,
}

/// Whether `name` is a full ref name `git check-ref-format` accepts, such as `refs/heads/main`.
pub fn is_valid_ref_name(name: &str) -> bool {
    name.contains('/') && gix::refs::FullName::try_from(name).is_ok()
}

/// Deletes the ref `name`, packed or loose.
pub fn delete_ref(git_dir: &Path, name: &str) -> io::Result<Result<(), Refusal>> {
    use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
    if !is_valid_ref_name(name) {
        return Ok(Err(Refusal::InvalidRefName(name.to_owned())));
    }
    let repo = open(git_dir)?;
    let Some(reference) = repo.try_find_reference(name).map_err(other)? else {
        return Ok(Err(Refusal::Missing(name.to_owned())));
    };
    let edit = RefEdit {
        change: Change::Delete {
            expected: PreviousValue::MustExistAndMatch(reference.target().into_owned()),
            log: RefLog::AndReference,
        },
        name: reference.name().to_owned(),
        deref: false,
    };
    edit_ref(&repo, edit, name)
}

/// Points the ref `name` at `sha`, but only while it points at `expected`.
pub fn move_ref(
    git_dir: &Path,
    name: &str,
    sha: &str,
    expected: &str,
) -> io::Result<Result<(), Refusal>> {
    if !is_valid_ref_name(name) {
        return Ok(Err(Refusal::InvalidRefName(name.to_owned())));
    }
    let repo = open(git_dir)?;
    let new = object_id(sha)?;
    if !repo.has_object(new) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{sha} is not in the repository"),
        ));
    }
    update_ref(&repo, name, new, Some(object_id(expected)?))
}

/// Points `name` at `new`, only while it points at `expected`, or only while it does not exist
/// when `expected` is `None`.
fn update_ref(
    repo: &gix::Repository,
    name: &str,
    new: gix::ObjectId,
    expected: Option<gix::ObjectId>,
) -> io::Result<Result<(), Refusal>> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit};
    let expected = match expected {
        Some(id) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(id)),
        None => PreviousValue::MustNotExist,
    };
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange::default(),
            expected,
            new: gix::refs::Target::Object(new),
        },
        name: name.try_into().map_err(other)?,
        deref: false,
    };
    edit_ref(repo, edit, name)
}

/// Applies `edit`, telling a ref that is not where the edit expects it from a failure.
fn edit_ref(
    repo: &gix::Repository,
    edit: gix::refs::transaction::RefEdit,
    name: &str,
) -> io::Result<Result<(), Refusal>> {
    use gix::refs::file::transaction::prepare::Error as Prepare;
    match repo.edit_reference(edit) {
        Ok(_) => Ok(Ok(())),
        Err(gix::reference::edit::Error::FileTransactionPrepare(
            Prepare::ReferenceOutOfDate { .. }
            | Prepare::MustExist { .. }
            | Prepare::MustNotExist { .. }
            | Prepare::DeleteReferenceMustExist { .. },
        )) => Ok(Err(Refusal::Moved(name.to_owned()))),
        Err(error) => Err(other(error)),
    }
}

/// Resolves a revision to a commit, `None` when there is no such commit.
fn resolve(git_dir: &Path, revision: &str) -> io::Result<Option<gix::ObjectId>> {
    resolve_commit(git_dir, revision)?
        .map(|sha| object_id(&sha))
        .transpose()
}

/// The best common ancestor of `one` and `two`, `None` when they share no history.
fn merge_base(
    repo: &gix::Repository,
    one: gix::ObjectId,
    two: gix::ObjectId,
) -> io::Result<Option<gix::ObjectId>> {
    match repo.merge_base(one, two) {
        Ok(base) => Ok(Some(base.detach())),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => Ok(None),
        Err(error) => Err(other(error)),
    }
}

/// Merges the trees of `ours` and `theirs` with `git merge-tree`, from their merge base or from
/// `base` when given, and answers the merged tree or the paths that conflict.
fn merge_tree(
    git_dir: &Path,
    base: Option<gix::ObjectId>,
    ours: gix::ObjectId,
    theirs: gix::ObjectId,
) -> io::Result<Result<gix::ObjectId, Refusal>> {
    let mut args = vec![
        "merge-tree".to_owned(),
        "--write-tree".to_owned(),
        "--name-only".to_owned(),
        "--no-messages".to_owned(),
        "-z".to_owned(),
    ];
    args.extend(base.map(|base| format!("--merge-base={base}")));
    args.extend([ours.to_string(), theirs.to_string()]);
    let output = command(git_dir).args(&args).output()?;
    let mut fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let tree = fields.next().map(String::from_utf8_lossy);
    match (output.status.code(), tree) {
        (Some(0), Some(tree)) => Ok(Ok(object_id(&tree)?)),
        (Some(1), Some(_)) => {
            let mut paths: Vec<String> = Vec::new();
            for path in fields.map(|path| String::from_utf8_lossy(path).into_owned()) {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
            Ok(Err(Refusal::Conflict(paths)))
        }
        _ => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            check(&args, &output)?;
            Err(io::Error::other(format!(
                "git {} printed no tree",
                args.join(" ")
            )))
        }
    }
}

/// A gitoxide signature of `identity`, dated now unless it says when.
fn actor(identity: &Identity) -> io::Result<gix::actor::Signature> {
    let time = match identity.date {
        Some(date) => gix::date::parse(date, None).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid date {date:?}: {error:?}"),
            )
        })?,
        None => gix::date::Time::now_local_or_utc(),
    };
    Ok(gix::actor::Signature {
        name: identity.name.into(),
        email: identity.email.into(),
        time,
    })
}

/// Writes a commit object, moving no ref.
fn write_commit(
    repo: &gix::Repository,
    tree: gix::ObjectId,
    parents: &[gix::ObjectId],
    message: impl Into<gix::bstr::BString>,
    author: gix::actor::Signature,
    committer: gix::actor::Signature,
) -> io::Result<gix::ObjectId> {
    let commit = gix::objs::Commit {
        tree,
        parents: parents.iter().copied().collect(),
        author,
        committer,
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    Ok(repo.write_object(&commit).map_err(other)?.detach())
}

/// A commit message as `git commit -m` stores it: ending in exactly one newline.
fn message_of(message: &str) -> String {
    format!("{}\n", message.trim_end())
}

fn tree_of(repo: &gix::Repository, commit: gix::ObjectId) -> io::Result<gix::ObjectId> {
    Ok(repo
        .find_commit(commit)
        .map_err(other)?
        .tree_id()
        .map_err(other)?
        .detach())
}

/// Whether `head` can be merged into `base` without conflicts. Both are revisions, such as
/// branch names or shas.
pub fn mergeable(git_dir: &Path, base: &str, head: &str) -> io::Result<Result<(), Refusal>> {
    let repo = open(git_dir)?;
    let (base, head) = match (resolve(git_dir, base)?, resolve(git_dir, head)?) {
        (Some(base), Some(head)) => (base, head),
        (None, _) => return Ok(Err(Refusal::Missing(base.to_owned()))),
        (_, None) => return Ok(Err(Refusal::Missing(head.to_owned()))),
    };
    match merge_base(&repo, base, head)? {
        None => Ok(Err(Refusal::Unrelated)),
        Some(upstream) if upstream == head => Ok(Ok(())),
        Some(_) => Ok(merge_tree(git_dir, None, base, head)?.map(drop)),
    }
}

/// Brings the revision `head` into the branch `base` as `style` says, with `message` for the
/// commit the style makes and `by` as its author and committer, and answers the commit the
/// branch points at afterwards. The commits a rebase replays keep their authors.
///
/// Nothing is checked out. The branch moves only once the commit is made, and only if it still
/// points where it did when the merge started; `head` never moves. Merging `head` when `base`
/// already has it changes nothing, except for [`MergeStyle::Squash`], which commits anyway.
pub fn merge(
    git_dir: &Path,
    base: &str,
    head: &str,
    style: MergeStyle,
    message: &str,
    by: &Identity,
) -> io::Result<Result<String, Refusal>> {
    let base_ref = format!("refs/heads/{base}");
    if !is_valid_ref_name(&base_ref) {
        return Ok(Err(Refusal::InvalidRefName(base_ref)));
    }
    let repo = open(git_dir)?;
    let Some(base_sha) = resolve(git_dir, &base_ref)? else {
        return Ok(Err(Refusal::Missing(base_ref)));
    };
    let Some(head_sha) = resolve(git_dir, head)? else {
        return Ok(Err(Refusal::Missing(head.to_owned())));
    };
    let Some(upstream) = merge_base(&repo, base_sha, head_sha)? else {
        return Ok(Err(Refusal::Unrelated));
    };
    let merged = match style {
        MergeStyle::Merge if upstream == head_sha => base_sha,
        MergeStyle::Merge => match merge_tree(git_dir, None, base_sha, head_sha)? {
            Ok(tree) => write_commit(
                &repo,
                tree,
                &[base_sha, head_sha],
                message_of(message),
                actor(by)?,
                actor(by)?,
            )?,
            Err(refusal) => return Ok(Err(refusal)),
        },
        MergeStyle::Squash => match merge_tree(git_dir, None, base_sha, head_sha)? {
            Ok(tree) => write_commit(
                &repo,
                tree,
                &[base_sha],
                message_of(message),
                actor(by)?,
                actor(by)?,
            )?,
            Err(refusal) => return Ok(Err(refusal)),
        },
        MergeStyle::FastForward if upstream == head_sha => base_sha,
        MergeStyle::FastForward if upstream == base_sha => head_sha,
        MergeStyle::FastForward => return Ok(Err(Refusal::NotFastForward)),
        MergeStyle::Rebase => match rebase(git_dir, &repo, base_sha, head_sha, upstream, by)? {
            Ok(rebased) => rebased,
            Err(refusal) => return Ok(Err(refusal)),
        },
        MergeStyle::RebaseMerge => {
            match rebase(git_dir, &repo, base_sha, head_sha, upstream, by)? {
                Ok(rebased) if rebased == base_sha => base_sha,
                Ok(rebased) => write_commit(
                    &repo,
                    tree_of(&repo, rebased)?,
                    &[base_sha, rebased],
                    message_of(message),
                    actor(by)?,
                    actor(by)?,
                )?,
                Err(refusal) => return Ok(Err(refusal)),
            }
        }
    };
    if merged != base_sha
        && let Err(refusal) = update_ref(&repo, &base_ref, merged, Some(base_sha))?
    {
        return Ok(Err(refusal));
    }
    Ok(Ok(merged.to_string()))
}

/// Replays the commits of `head` that `base` lacks onto `base`, as `git rebase` does: merge
/// commits are dropped, and so are commits whose changes `base` already has. Answers the commit
/// the replay ends at, which is `head` itself when `base` is behind it.
fn rebase(
    git_dir: &Path,
    repo: &gix::Repository,
    base: gix::ObjectId,
    head: gix::ObjectId,
    upstream: gix::ObjectId,
    by: &Identity,
) -> io::Result<Result<gix::ObjectId, Refusal>> {
    if upstream == base {
        return Ok(Ok(head));
    }
    let mut current = base;
    for commit in topological(repo, head, base)? {
        let commit = repo.find_commit(commit).map_err(other)?;
        let decoded = commit.decode().map_err(other)?;
        let parents: Vec<gix::ObjectId> = decoded.parents().collect();
        let [parent] = parents[..] else {
            continue;
        };
        let tree = match merge_tree(git_dir, Some(parent), current, commit.id)? {
            Ok(tree) => tree,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let became_empty =
            tree == tree_of(repo, current)? && decoded.tree() != tree_of(repo, parent)?;
        if became_empty {
            continue;
        }
        current = write_commit(
            repo,
            tree,
            &[current],
            decoded.message.to_owned(),
            decoded.author().map_err(other)?.to_owned().map_err(other)?,
            actor(by)?,
        )?;
    }
    Ok(Ok(current))
}

/// The commits reachable from `head` but not from `hidden`, every parent before its children.
fn topological(
    repo: &gix::Repository,
    head: gix::ObjectId,
    hidden: gix::ObjectId,
) -> io::Result<Vec<gix::ObjectId>> {
    let mut wanted = std::collections::HashSet::new();
    for info in repo
        .rev_walk([head])
        .with_hidden([hidden])
        .all()
        .map_err(other)?
    {
        wanted.insert(info.map_err(other)?.id);
    }
    let mut ordered = Vec::new();
    let mut done = std::collections::HashSet::new();
    let mut stack = vec![(head, false)];
    while let Some((id, expanded)) = stack.pop() {
        if !wanted.contains(&id) || (done.contains(&id) && !expanded) {
            continue;
        }
        if expanded {
            if done.insert(id) {
                ordered.push(id);
            }
            continue;
        }
        stack.push((id, true));
        let commit = repo.find_commit(id).map_err(other)?;
        for parent in commit.parent_ids() {
            stack.push((parent.detach(), false));
        }
    }
    Ok(ordered)
}

const CURRENT_DIR: &str = ".";
const PARENT_DIR: &str = "..";
/// The directory git keeps a repository in, which no committed path may name.
const DOT_GIT: &str = ".git";

/// Commits `files`, each a path from the repository root and its bytes, on `branch`, without a
/// working tree, and answers the commit's sha. The commit's parent is the branch, or the branch
/// `from` when `branch` does not exist yet, or none when neither does. It is made even when it
/// changes nothing, and fails rather than moving a branch someone else moved meanwhile.
pub fn commit_files(
    git_dir: &Path,
    branch: &str,
    from: Option<&str>,
    files: &[(&str, &[u8])],
    message: &str,
    author: &Identity,
    committer: &Identity,
) -> io::Result<String> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
    let branch_ref = format!("refs/heads/{branch}");
    if !is_valid_ref_name(&branch_ref) {
        return Err(invalid(format!("{branch} is not a valid branch name")));
    }
    let repo = open(git_dir)?;
    let current = resolve(git_dir, &branch_ref)?;
    let parent = match (current, from) {
        (Some(current), _) => Some(current),
        (None, Some(from)) => resolve(git_dir, &format!("refs/heads/{from}"))?,
        (None, None) => None,
    };
    let base_tree = match parent {
        Some(parent) => tree_of(&repo, parent)?,
        None => gix::ObjectId::empty_tree(repo.object_hash()),
    };
    let existing = repo.find_tree(base_tree).map_err(other)?;
    let mut editor = repo.edit_tree(base_tree).map_err(other)?;
    for (path, bytes) in files {
        let components: Vec<&str> = path.split('/').collect();
        if components.iter().any(|component| {
            component.is_empty()
                || matches!(*component, CURRENT_DIR | PARENT_DIR)
                || component.eq_ignore_ascii_case(DOT_GIT)
        }) {
            return Err(invalid(format!(
                "{path:?} is not a path inside the repository"
            )));
        }
        // An existing file keeps its executable bit, as `git add` keeps it.
        let mut kind = gix::objs::tree::EntryKind::Blob;
        for end in 1..=components.len() {
            let prefix = components[..end].join("/");
            let Some(entry) = existing.lookup_entry_by_path(&prefix).map_err(other)? else {
                break;
            };
            let leaf = end == components.len();
            let fits = if leaf {
                entry.mode().is_blob()
            } else {
                entry.mode().is_tree()
            };
            if !fits {
                return Err(invalid(format!("{prefix:?} is in the way of {path:?}")));
            }
            if leaf && entry.mode().is_executable() {
                kind = gix::objs::tree::EntryKind::BlobExecutable;
            }
        }
        let blob = repo.write_blob(bytes).map_err(other)?.detach();
        editor.upsert(*path, kind, blob).map_err(other)?;
    }
    let tree = editor.write().map_err(other)?.detach();
    let parents: Vec<gix::ObjectId> = parent.into_iter().collect();
    let sha = write_commit(
        &repo,
        tree,
        &parents,
        message_of(message),
        actor(author)?,
        actor(committer)?,
    )?;
    update_ref(&repo, &branch_ref, sha, current)?
        .map_err(|refusal| io::Error::other(refusal.to_string()))?;
    Ok(sha.to_string())
}

/// What `git diff --binary base head` prints. Both are revisions, such as branch names or shas.
pub fn diff(git_dir: &Path, base: &str, head: &str) -> io::Result<Vec<u8>> {
    let (base, head) = (commit_id(git_dir, base)?, commit_id(git_dir, head)?);
    stdout(git_dir, &["diff", "--binary", &base, &head])
}

/// What `git format-patch --stdout --no-signature base..head` prints: one patch per commit of
/// `head` that `base` lacks. Both are revisions, such as branch names or shas.
pub fn format_patch(git_dir: &Path, base: &str, head: &str) -> io::Result<Vec<u8>> {
    let (base, head) = (commit_id(git_dir, base)?, commit_id(git_dir, head)?);
    let range = format!("{base}..{head}");
    stdout(
        git_dir,
        &["format-patch", "--stdout", "--no-signature", &range],
    )
}

/// The sha of the commit `revision` names, failing with [`io::ErrorKind::NotFound`] without one.
fn commit_id(git_dir: &Path, revision: &str) -> io::Result<String> {
    resolve_commit(git_dir, revision)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{revision} is not a commit"),
        )
    })
}

/// Runs git with `args` in `git_dir` and answers what it prints, failing when git does.
fn stdout(git_dir: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = command(git_dir).args(args).output()?;
    check(args, &output)?;
    Ok(output.stdout)
}

/// Copies every branch of the repository `from`, and the objects they need, into `git_dir`,
/// overwriting branches of the same name.
pub fn copy_branches(git_dir: &Path, from: &Path) -> io::Result<()> {
    let output = command(git_dir)
        .args(["fetch", "--quiet", "--no-tags", "--"])
        .arg(from)
        .arg("+refs/heads/*:refs/heads/*")
        .output()?;
    check(&["fetch", "--no-tags"], &output)
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
