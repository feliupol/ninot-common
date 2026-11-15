//! Typed git operations against real bare repositories.
#![cfg(feature = "git")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ninot_common::git::{self, Identity, MergeStyle, PathContent, Refusal};

const TESTER: Identity<'static> = Identity {
    name: "tester",
    email: "tester@localhost",
    date: Some("2000-01-01T00:00:00+00:00"),
};

const MERGER: Identity<'static> = Identity {
    name: "merger",
    email: "merger@localhost",
    date: Some("2001-02-03T04:05:06+02:00"),
};

/// A directory no other test uses, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ninot-common-git-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The git CLI, isolated from the user's configuration, as `TESTER` at a fixed date.
fn cli(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", TESTER.name)
        .env("GIT_AUTHOR_EMAIL", TESTER.email)
        .env("GIT_AUTHOR_DATE", TESTER.date.unwrap())
        .env("GIT_COMMITTER_NAME", TESTER.name)
        .env("GIT_COMMITTER_EMAIL", TESTER.email)
        .env("GIT_COMMITTER_DATE", TESTER.date.unwrap())
        .env_remove("GIT_DIR");
    command
}

/// Runs the git CLI in `dir` and answers its standard output, failing when git does.
fn run(dir: &Path, args: &[&str]) -> String {
    let output = cli(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A bare repository with `a.txt` = `one` on `main`, and `feature` one commit ahead that changes
/// `a.txt` to `two` and adds `b.txt`; plus the clone it was built with.
struct Repo {
    _scratch: Scratch,
    git_dir: PathBuf,
    work: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let scratch = Scratch::new();
        let git_dir = scratch.0.join("repo.git");
        let work = scratch.0.join("work");
        git::init_bare(&git_dir, "main").unwrap();
        run(
            &scratch.0,
            &["clone", "-q", git_dir.to_str().unwrap(), "work"],
        );
        let repo = Self {
            _scratch: scratch,
            git_dir,
            work,
        };
        repo.commit("main", &[("a.txt", "one")], "Add a.txt");
        run(&repo.work, &["checkout", "-q", "-b", "feature"]);
        repo.commit(
            "feature",
            &[("a.txt", "two"), ("b.txt", "b")],
            "Change a and add b",
        );
        repo
    }

    /// Commits `files` on `branch` of the clone and pushes it.
    fn commit(&self, branch: &str, files: &[(&str, &str)], message: &str) -> String {
        let current = cli(&self.work)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap()
            .stdout;
        if String::from_utf8_lossy(&current).trim() != branch
            && !cli(&self.work)
                .args(["checkout", "-q", branch])
                .status()
                .unwrap()
                .success()
        {
            run(&self.work, &["checkout", "-q", "-b", branch]);
        }
        for (path, content) in files {
            let target = self.work.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, content).unwrap();
            run(&self.work, &["add", path]);
        }
        run(
            &self.work,
            &["commit", "-q", "--allow-empty", "-m", message],
        );
        run(&self.work, &["push", "-q", "-f", "origin", branch]);
        run(&self.work, &["rev-parse", "HEAD"])
    }

    fn sha(&self, branch: &str) -> Option<String> {
        git::resolve_commit(&self.git_dir, &format!("refs/heads/{branch}")).unwrap()
    }

    fn file(&self, commit: &str, path: &str) -> Option<Vec<u8>> {
        match git::path_content(&self.git_dir, commit, path).unwrap()? {
            PathContent::File(entry) => Some(git::blob(&self.git_dir, &entry.sha).unwrap()),
            PathContent::Directory(_) => panic!("{path} is a directory"),
        }
    }

    /// Makes `main` change the line of `a.txt` that `feature` changes.
    fn diverge(&self) -> String {
        self.commit("main", &[("a.txt", "three")], "Change a")
    }

    /// Installs a hook that fails, so that any git command that runs it fails.
    fn failing_hooks(&self) {
        for hook in ["reference-transaction", "pre-receive", "post-checkout"] {
            let path = self.git_dir.join("hooks").join(hook);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }
}

#[test]
fn ref_names_are_validated_as_git_check_ref_format_does() {
    let scratch = Scratch::new();
    for name in [
        "refs/heads/main",
        "refs/heads/feature/one",
        "refs/tags/v1.0",
        "refs/heads/a..b",
        "refs/heads/x.lock",
        "refs/heads/a@{b",
        "refs/heads/trailing/",
        "refs/heads/with space",
        "refs/heads/.hidden",
        "refs/heads/-dash",
        "refs/heads/tilde~1",
        "refs/heads/colon:x",
        "refs/heads/back\\slash",
        "refs//double",
        "refs/heads/end.",
        "main",
        "HEAD",
        "refs",
        "@",
        "",
    ] {
        let git_says = cli(&scratch.0)
            .args(["check-ref-format", name])
            .status()
            .unwrap()
            .success();
        assert_eq!(git::is_valid_ref_name(name), git_says, "{name:?}");
    }
}

#[test]
fn a_deleted_ref_is_gone_even_when_packed() {
    let repo = Repo::new();
    run(&repo.git_dir, &["pack-refs", "--all"]);
    assert_eq!(
        git::delete_ref(&repo.git_dir, "refs/heads/feature").unwrap(),
        Ok(())
    );
    assert_eq!(repo.sha("feature"), None);
    assert!(repo.sha("main").is_some());
}

#[test]
fn deleting_a_missing_or_invalid_ref_is_refused() {
    let repo = Repo::new();
    assert_eq!(
        git::delete_ref(&repo.git_dir, "refs/heads/nowhere").unwrap(),
        Err(Refusal::Missing("refs/heads/nowhere".to_owned()))
    );
    assert_eq!(
        git::delete_ref(&repo.git_dir, "refs/heads/a..b").unwrap(),
        Err(Refusal::InvalidRefName("refs/heads/a..b".to_owned()))
    );
}

#[test]
fn a_ref_moves_only_from_where_it_is_expected() {
    let repo = Repo::new();
    let main = repo.sha("main").unwrap();
    let feature = repo.sha("feature").unwrap();
    assert_eq!(
        git::move_ref(&repo.git_dir, "refs/heads/main", &feature, &feature).unwrap(),
        Err(Refusal::Moved("refs/heads/main".to_owned()))
    );
    assert_eq!(repo.sha("main").unwrap(), main);
    assert_eq!(
        git::move_ref(&repo.git_dir, "refs/heads/nowhere", &feature, &main).unwrap(),
        Err(Refusal::Moved("refs/heads/nowhere".to_owned()))
    );
    assert_eq!(
        git::move_ref(&repo.git_dir, "refs/heads/main", &feature, &main).unwrap(),
        Ok(())
    );
    assert_eq!(repo.sha("main").unwrap(), feature);
}

#[test]
fn a_ref_does_not_move_to_an_object_the_repository_lacks() {
    let repo = Repo::new();
    let main = repo.sha("main").unwrap();
    let absent = "0123456789012345678901234567890123456789";
    assert!(git::move_ref(&repo.git_dir, "refs/heads/main", absent, &main).is_err());
    assert_eq!(repo.sha("main").unwrap(), main);
}

#[test]
fn every_merge_style_brings_the_head_into_the_base_its_own_way() {
    for (style, parents) in [
        (MergeStyle::Merge, 2),
        (MergeStyle::Squash, 1),
        (MergeStyle::FastForward, 1),
        (MergeStyle::Rebase, 1),
        (MergeStyle::RebaseMerge, 2),
    ] {
        let repo = Repo::new();
        let feature = repo.sha("feature").unwrap();
        let merged = git::merge(&repo.git_dir, "main", "feature", style, "Merged", &MERGER)
            .unwrap()
            .unwrap();
        assert_eq!(
            repo.sha("main").unwrap(),
            merged,
            "{style:?} moves the base"
        );
        let commit = git::commit(&repo.git_dir, &merged).unwrap();
        assert_eq!(commit.parents.len(), parents, "{style:?}");
        assert_eq!(repo.file(&merged, "a.txt").unwrap(), b"two", "{style:?}");
        assert_eq!(repo.file(&merged, "b.txt").unwrap(), b"b", "{style:?}");
        assert_eq!(
            repo.sha("feature").unwrap(),
            feature,
            "{style:?} keeps the head"
        );
    }
}

#[test]
fn a_merge_commit_carries_the_message_and_identity_given() {
    let repo = Repo::new();
    let main = repo.sha("main").unwrap();
    let feature = repo.sha("feature").unwrap();
    let merged = git::merge(
        &repo.git_dir,
        "main",
        "feature",
        MergeStyle::Merge,
        "Merge feature",
        &MERGER,
    )
    .unwrap()
    .unwrap();
    let commit = git::commit(&repo.git_dir, &merged).unwrap();
    assert_eq!(commit.parents, [main, feature]);
    assert_eq!(commit.message, "Merge feature");
    for signature in [&commit.author, &commit.committer] {
        assert_eq!(signature.name, MERGER.name);
        assert_eq!(signature.email, MERGER.email);
        assert_eq!(signature.date, MERGER.date.unwrap());
    }
}

#[test]
fn a_diverged_merge_replays_and_squashes_onto_the_new_base() {
    for style in [
        MergeStyle::Merge,
        MergeStyle::Squash,
        MergeStyle::Rebase,
        MergeStyle::RebaseMerge,
    ] {
        let repo = Repo::new();
        let main = repo.commit("main", &[("c.txt", "c")], "Add c");
        let merged = git::merge(&repo.git_dir, "main", "feature", style, "Merged", &MERGER)
            .unwrap()
            .unwrap();
        assert_ne!(merged, main, "{style:?}");
        assert_eq!(repo.file(&merged, "a.txt").unwrap(), b"two", "{style:?}");
        assert_eq!(repo.file(&merged, "c.txt").unwrap(), b"c", "{style:?}");
        let first = git::commit(&repo.git_dir, &merged).unwrap().parents[0].clone();
        if matches!(style, MergeStyle::Rebase) {
            // The replayed commit keeps its author and gets the merger as committer.
            let commit = git::commit(&repo.git_dir, &merged).unwrap();
            assert_eq!(first, main);
            assert_eq!(commit.author.name, TESTER.name);
            assert_eq!(commit.committer.name, MERGER.name);
            assert_eq!(commit.message, "Change a and add b");
        } else {
            assert_eq!(first, main, "{style:?} builds on the base");
        }
    }
}

#[test]
fn a_fast_forward_is_refused_once_the_base_moved_on() {
    let repo = Repo::new();
    let main = repo.commit("main", &[("c.txt", "c")], "Add c");
    assert_eq!(
        git::merge(
            &repo.git_dir,
            "main",
            "feature",
            MergeStyle::FastForward,
            "Merged",
            &MERGER
        )
        .unwrap(),
        Err(Refusal::NotFastForward)
    );
    assert_eq!(repo.sha("main").unwrap(), main);
}

#[test]
fn a_conflicting_merge_names_the_paths_and_moves_nothing() {
    for style in [
        MergeStyle::Merge,
        MergeStyle::Squash,
        MergeStyle::Rebase,
        MergeStyle::RebaseMerge,
    ] {
        let repo = Repo::new();
        let main = repo.diverge();
        repo.failing_hooks();
        assert_eq!(
            git::merge(&repo.git_dir, "main", "feature", style, "Merged", &MERGER).unwrap(),
            Err(Refusal::Conflict(vec!["a.txt".to_owned()])),
            "{style:?}"
        );
        assert_eq!(repo.sha("main").unwrap(), main, "{style:?}");
        assert!(!repo.git_dir.join("worktrees").exists(), "{style:?}");
    }
}

#[test]
fn merging_a_missing_branch_is_refused() {
    let repo = Repo::new();
    assert_eq!(
        git::merge(
            &repo.git_dir,
            "main",
            "nowhere",
            MergeStyle::Merge,
            "Merged",
            &MERGER
        )
        .unwrap(),
        Err(Refusal::Missing("nowhere".to_owned()))
    );
    assert_eq!(
        git::merge(
            &repo.git_dir,
            "nowhere",
            "feature",
            MergeStyle::Merge,
            "Merged",
            &MERGER
        )
        .unwrap(),
        Err(Refusal::Missing("refs/heads/nowhere".to_owned()))
    );
}

#[test]
fn merging_what_the_base_already_has_changes_nothing() {
    let repo = Repo::new();
    let main = repo.sha("main").unwrap();
    for style in [
        MergeStyle::Merge,
        MergeStyle::FastForward,
        MergeStyle::Rebase,
        MergeStyle::RebaseMerge,
    ] {
        assert_eq!(
            git::merge(&repo.git_dir, "feature", "main", style, "Merged", &MERGER)
                .unwrap()
                .unwrap(),
            repo.sha("feature").unwrap(),
            "{style:?}"
        );
    }
    assert_eq!(repo.sha("main").unwrap(), main);
}

#[test]
fn a_branch_is_brought_up_to_date_by_merging_its_base_into_it() {
    let repo = Repo::new();
    let main = repo.commit("main", &[("c.txt", "c")], "Add c");
    let feature = repo.sha("feature").unwrap();
    let updated = git::merge(
        &repo.git_dir,
        "feature",
        &main,
        MergeStyle::Merge,
        "Merge branch 'main' into feature",
        &MERGER,
    )
    .unwrap()
    .unwrap();
    assert_eq!(repo.sha("feature").unwrap(), updated);
    assert_eq!(repo.sha("main").unwrap(), main);
    assert_eq!(
        git::commit(&repo.git_dir, &updated).unwrap().parents,
        [feature, main]
    );
}

#[test]
fn mergeability_reports_the_conflicting_paths() {
    let repo = Repo::new();
    assert_eq!(
        git::mergeable(&repo.git_dir, "main", "feature").unwrap(),
        Ok(())
    );
    assert_eq!(
        git::mergeable(&repo.git_dir, "feature", "main").unwrap(),
        Ok(())
    );
    repo.diverge();
    assert_eq!(
        git::mergeable(&repo.git_dir, "main", "feature").unwrap(),
        Err(Refusal::Conflict(vec!["a.txt".to_owned()]))
    );
    assert_eq!(
        git::mergeable(&repo.git_dir, "main", "nowhere").unwrap(),
        Err(Refusal::Missing("nowhere".to_owned()))
    );
}

#[test]
fn unrelated_histories_are_not_mergeable() {
    let repo = Repo::new();
    run(&repo.work, &["checkout", "-q", "--orphan", "other"]);
    repo.commit("other", &[("z.txt", "z")], "Unrelated");
    assert_eq!(
        git::mergeable(&repo.git_dir, "main", "other").unwrap(),
        Err(Refusal::Unrelated)
    );
    assert_eq!(
        git::merge(
            &repo.git_dir,
            "main",
            "other",
            MergeStyle::Merge,
            "Merged",
            &MERGER
        )
        .unwrap(),
        Err(Refusal::Unrelated)
    );
}

#[test]
fn committed_files_give_the_sha_the_git_cli_gives() {
    let repo = Repo::new();
    let expected = repo.commit("main", &[("dir/new.txt", "new")], "Add new");
    run(&repo.work, &["push", "-q", "-f", "origin", "HEAD~1:main"]);
    let sha = git::commit_files(
        &repo.git_dir,
        "main",
        None,
        &[("dir/new.txt", b"new")],
        "Add new",
        &TESTER,
        &TESTER,
    )
    .unwrap();
    assert_eq!(sha, expected);
    assert_eq!(repo.sha("main").unwrap(), sha);
}

#[test]
fn committed_files_start_a_new_branch_from_another() {
    let repo = Repo::new();
    let main = repo.sha("main").unwrap();
    let sha = git::commit_files(
        &repo.git_dir,
        "topic",
        Some("main"),
        &[("a.txt", b"replaced"), ("deep/er/c.txt", b"c")],
        "Topic",
        &TESTER,
        &MERGER,
    )
    .unwrap();
    assert_eq!(repo.sha("topic").unwrap(), sha);
    let commit = git::commit(&repo.git_dir, &sha).unwrap();
    assert_eq!(commit.parents, [main]);
    assert_eq!(commit.author.date, TESTER.date.unwrap());
    assert_eq!(commit.committer.name, MERGER.name);
    assert_eq!(commit.committer.date, MERGER.date.unwrap());
    assert_eq!(repo.file(&sha, "a.txt").unwrap(), b"replaced");
    assert_eq!(repo.file(&sha, "deep/er/c.txt").unwrap(), b"c");
}

#[test]
fn committed_files_on_a_new_branch_with_nothing_to_start_from_are_a_root() {
    let repo = Repo::new();
    let sha = git::commit_files(
        &repo.git_dir,
        "orphan",
        Some("nowhere"),
        &[("only.txt", b"only")],
        "Root",
        &TESTER,
        &TESTER,
    )
    .unwrap();
    assert!(git::commit(&repo.git_dir, &sha).unwrap().parents.is_empty());
    assert_eq!(repo.file(&sha, "a.txt"), None);
}

#[test]
fn committed_files_are_checked_against_the_whole_tree() {
    let repo = Repo::new();
    repo.commit("main", &[("x/y/z/f", "deep")], "Add deep");
    let main = repo.sha("main").unwrap();
    for files in [
        &[("x/y/z", &b"blob over a directory"[..])][..],
        &[
            ("x/y/z/g", &b"fine"[..]),
            ("a.txt/under", &b"under a file"[..]),
        ][..],
        &[
            ("x/y/z/g", &b"fine"[..]),
            ("x", &b"blob over a directory"[..]),
        ][..],
    ] {
        let error = git::commit_files(&repo.git_dir, "main", None, files, "Bad", &TESTER, &TESTER)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{files:?}");
    }
    assert_eq!(repo.sha("main").unwrap(), main);
}

#[test]
fn an_executable_file_stays_executable_when_committed_over() {
    let repo = Repo::new();
    run(&repo.work, &["checkout", "-q", "main"]);
    fs::write(repo.work.join("run.sh"), "one").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(repo.work.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    run(&repo.work, &["add", "--chmod=+x", "run.sh"]);
    run(&repo.work, &["commit", "-q", "-m", "Add run.sh"]);
    run(&repo.work, &["push", "-q", "origin", "main"]);
    let expected = repo.commit("main", &[("run.sh", "two")], "Change run.sh");
    run(&repo.work, &["push", "-q", "-f", "origin", "HEAD~1:main"]);
    let sha = git::commit_files(
        &repo.git_dir,
        "main",
        None,
        &[("run.sh", b"two")],
        "Change run.sh",
        &TESTER,
        &TESTER,
    )
    .unwrap();
    assert_eq!(sha, expected);
}

#[test]
fn committed_files_must_stay_inside_the_repository() {
    let repo = Repo::new();
    let main = repo.sha("main").unwrap();
    for path in [
        "",
        "../x",
        "a/../b",
        ".git/config",
        "dir/.GIT/x",
        "/abs",
        "a.txt/under",
    ] {
        let error = git::commit_files(
            &repo.git_dir,
            "main",
            None,
            &[(path, b"x")],
            "Bad",
            &TESTER,
            &TESTER,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{path:?}");
    }
    assert_eq!(repo.sha("main").unwrap(), main);
}

#[test]
fn a_diff_and_a_patch_are_what_the_git_cli_prints() {
    let repo = Repo::new();
    let png = [0x89, b'P', b'N', b'G', 0, 1, 2, 3];
    fs::write(repo.work.join("image.png"), png).unwrap();
    run(&repo.work, &["add", "image.png"]);
    run(&repo.work, &["commit", "-q", "-m", "Add image"]);
    run(&repo.work, &["push", "-q", "origin", "feature"]);
    let (main, feature) = (repo.sha("main").unwrap(), repo.sha("feature").unwrap());

    let diff = git::diff(&repo.git_dir, &main, &feature).unwrap();
    let cli_diff = cli(&repo.git_dir)
        .args(["diff", "--binary", &main, &feature])
        .output()
        .unwrap()
        .stdout;
    assert_eq!(diff, cli_diff);
    assert!(String::from_utf8_lossy(&diff).contains("GIT binary patch"));

    let patch = git::format_patch(&repo.git_dir, &main, &feature).unwrap();
    let cli_patch = cli(&repo.git_dir)
        .args([
            "format-patch",
            "--stdout",
            "--no-signature",
            &format!("{main}..{feature}"),
        ])
        .output()
        .unwrap()
        .stdout;
    assert_eq!(patch, cli_patch);
    assert_eq!(
        String::from_utf8_lossy(&patch)
            .matches("Subject: [PATCH")
            .count(),
        2
    );
}

#[test]
fn branches_are_copied_into_another_repository_whatever_its_hooks() {
    let repo = Repo::new();
    let scratch = Scratch::new();
    let fork = scratch.0.join("fork.git");
    git::init_bare(&fork, "main").unwrap();
    for hook in ["reference-transaction", "post-checkout"] {
        let path = fork.join("hooks").join(hook);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    git::copy_branches(&fork, &repo.git_dir).unwrap();
    let copied: Vec<_> = git::refs(&fork, "refs/")
        .unwrap()
        .into_iter()
        .map(|reference| (reference.name, reference.sha))
        .collect();
    assert_eq!(
        copied,
        [
            (
                "refs/heads/feature".to_owned(),
                repo.sha("feature").unwrap()
            ),
            ("refs/heads/main".to_owned(), repo.sha("main").unwrap()),
        ]
    );
}
