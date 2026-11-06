//! Matches request paths against a generated operation table with `matchit`.

use std::collections::HashMap;

use percent_encoding::percent_decode_str;

use crate::operation::{Method, OperationInfo};

/// Trailing path parameters that may contain `/`, such as a file path or a branch name.
const CATCH_ALL: [&str; 5] = ["path", "ref", "branch", "tag", "filepath"];

/// One piece of a path template's segment: literal text, or a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part<'a> {
    Literal(&'a str),
    Param(&'a str),
}

/// One operation matched by a request, with its path parameters by spec name.
pub struct Match {
    pub info: &'static OperationInfo,
    pub params: HashMap<String, String>,
}

/// The routes of one method.
///
/// `matchit` rejects a catch-all at a position other routes use for a parameter, such as
/// `commits/{*ref}` beside `commits/{ref}/status`. So every operation is routed with plain
/// parameters, and those ending in a [`CATCH_ALL`] parameter are also routed, with a
/// catch-all, in a second router that is consulted only when the first finds nothing. A path
/// such as `commits/main/status` therefore matches the more specific operation.
///
/// `matchit` also rejects a segment that mixes parameters with literal text, such as
/// `{sha}.{diffType}`, beside a plain `{sha}`. Such an operation is matched segment by
/// segment by [`match_template`], once `matchit` finds nothing static.
#[derive(Default)]
struct MethodRoutes {
    exact: matchit::Router<usize>,
    catch_all: matchit::Router<usize>,
}

/// An operation `matchit` cannot route, and the template [`match_template`] matches.
type Mixed = (usize, Vec<Vec<Part<'static>>>);

/// Routes every operation of a generated table.
pub struct Router {
    operations: &'static [&'static OperationInfo],
    methods: HashMap<Method, MethodRoutes>,
    /// The templates `matchit` cannot route, by method.
    mixed: HashMap<Method, Vec<Mixed>>,
    /// Spec parameter names of each operation, in the order its segments hold them.
    names: Vec<Vec<&'static str>>,
}

impl Router {
    /// Routes `operations`, the table a spec generated.
    pub fn new(operations: &'static [&'static OperationInfo]) -> Self {
        let mut methods: HashMap<Method, MethodRoutes> = HashMap::new();
        let mut mixed: HashMap<Method, Vec<Mixed>> = HashMap::new();
        let mut names = Vec::with_capacity(operations.len());
        for (index, info) in operations.iter().enumerate() {
            let template = split_path(info.path);
            names.push(
                template
                    .iter()
                    .flatten()
                    .filter_map(|part| match part {
                        Part::Literal(_) => None,
                        Part::Param(name) => Some(*name),
                    })
                    .collect(),
            );
            let plain = template.iter().all(|segment| segment.len() <= 1);
            if plain {
                let routes = methods.entry(info.method).or_default();
                routes
                    .exact
                    .insert(pattern(&template, false), index)
                    .unwrap_or_else(|error| panic!("cannot route {}: {error}", info.id));
                if template.last().is_some_and(|last| is_catch_all(last)) {
                    routes
                        .catch_all
                        .insert(pattern(&template, true), index)
                        .unwrap_or_else(|error| {
                            panic!("cannot route {} with a catch-all: {error}", info.id)
                        });
                }
            } else {
                mixed
                    .entry(info.method)
                    .or_default()
                    .push((index, template));
            }
        }
        Self {
            operations,
            methods,
            mixed,
            names,
        }
    }

    /// Finds the operation for `method` and `path`. Every parameter must be non-empty, and a
    /// catch-all must not start with an empty segment.
    pub fn find(&self, method: Method, path: &str) -> Option<Match> {
        let routes = self.methods.get(&method)?;
        let exact = routes.exact.at(path).ok().filter(filled);
        // A template mixing parameters with literal text is narrower than a plain parameter,
        // so it wins over one, but not over a route that matched statically.
        if !exact.as_ref().is_some_and(|found| found.params.is_empty())
            && let Some((index, params)) = self.find_mixed(method, path)
        {
            return Some(Match {
                info: self.operations[index],
                params,
            });
        }
        let found = exact.or_else(|| routes.catch_all.at(path).ok().filter(filled))?;
        let index = *found.value;
        let names = &self.names[index];
        let params = found
            .params
            .iter()
            .map(|(key, value)| {
                let position: usize = key[1..].parse().expect("parameters are named by position");
                (names[position - 1].to_owned(), decode(value))
            })
            .collect();
        Some(Match {
            info: self.operations[index],
            params,
        })
    }

    /// Matches `path` against the templates of `method` that `matchit` cannot route.
    fn find_mixed(&self, method: Method, path: &str) -> Option<(usize, HashMap<String, String>)> {
        let templates = self.mixed.get(&method)?;
        let segments: Vec<&str> = path.split('/').collect();
        templates
            .iter()
            .find_map(|(index, template)| Some((*index, match_template(template, &segments)?)))
    }
}

/// Whether `found` fills every parameter with something that could be a name.
fn filled(found: &matchit::Match<'_, '_, &usize>) -> bool {
    found
        .params
        .iter()
        .all(|(_, value)| !value.is_empty() && !value.starts_with('/'))
}

/// Splits a path template into its segments, each the literal text and parameters it holds in
/// order, as `{sha}.{diffType}` holds a parameter, a literal and another parameter.
///
/// # Panics
///
/// When a parameter has no closing brace, as no spec has.
fn split_path(path: &'static str) -> Vec<Vec<Part<'static>>> {
    path.split('/')
        .map(|segment| {
            let mut parts = Vec::new();
            let mut literal = segment;
            while let Some(start) = literal.find('{') {
                let (before, rest) = literal.split_at(start);
                if !before.is_empty() {
                    parts.push(Part::Literal(before));
                }
                let (name, after) = rest[1..]
                    .split_once('}')
                    .unwrap_or_else(|| panic!("{path} has an unterminated parameter"));
                parts.push(Part::Param(name));
                literal = after;
            }
            if !literal.is_empty() {
                parts.push(Part::Literal(literal));
            }
            parts
        })
        .collect()
}

/// Whether `segment` is one parameter that may hold `/`, such as a trailing `{filepath}`.
fn is_catch_all(segment: &[Part<'static>]) -> bool {
    match segment {
        [Part::Param(name)] => CATCH_ALL.contains(name),
        _ => false,
    }
}

/// The `matchit` pattern of a template whose segments are single literals and parameters.
/// Parameters are renamed by position: `matchit` rejects different names at the same
/// position, as in `/orgs/{org}` and `/orgs/{org_id}/...`.
fn pattern(template: &[Vec<Part<'static>>], catch_all: bool) -> String {
    let params = template
        .iter()
        .filter(|segment| matches!(segment.as_slice(), [Part::Param(_)]))
        .count();
    let mut numbered = 0;
    template
        .iter()
        .map(|segment| match segment.as_slice() {
            [] => String::new(),
            [Part::Literal(text)] => (*text).to_owned(),
            [Part::Param(_)] => {
                numbered += 1;
                if catch_all && numbered == params {
                    format!("{{*p{numbered}}}")
                } else {
                    format!("{{p{numbered}}}")
                }
            }
            _ => panic!("only templates of single literals and parameters are routed"),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

/// Matches the segments of a request path against `template`, returning its parameters by
/// spec name. Every parameter must be non-empty, and a trailing catch-all must not start with
/// an empty segment, as `matchit` requires of the templates it routes.
fn match_template(
    template: &[Vec<Part<'static>>],
    segments: &[&str],
) -> Option<HashMap<String, String>> {
    let (template, catch_all) = split_catch_all(template);
    // A catch-all swallows what is left of the path, so it must have something to swallow.
    if template.len() != segments.len() && !(catch_all.is_some() && segments.len() > template.len())
    {
        return None;
    }
    let mut params = HashMap::new();
    for (segment, path) in template.iter().zip(segments) {
        match_segment(segment, path, &mut params)?;
    }
    if let Some(name) = catch_all {
        let rest = &segments[template.len()..];
        if rest.first().is_none_or(|first| first.is_empty()) {
            return None;
        }
        params.insert(name.to_owned(), decode(&rest.join("/")));
    }
    Some(params)
}

/// `template` without its trailing catch-all parameter, and the parameter's name.
fn split_catch_all<'a>(
    template: &'a [Vec<Part<'static>>],
) -> (&'a [Vec<Part<'static>>], Option<&'static str>) {
    match template.split_last() {
        Some((last, head)) => match last.as_slice() {
            [Part::Param(name)] if CATCH_ALL.contains(name) => (head, Some(*name)),
            _ => (template, None),
        },
        None => (template, None),
    }
}

/// Matches one segment of a request path against the literal text and parameters of a
/// template segment, recording the parameters.
fn match_segment(
    segment: &[Part<'static>],
    path: &str,
    params: &mut HashMap<String, String>,
) -> Option<()> {
    if segment.is_empty() {
        return path.is_empty().then_some(());
    }
    if path.is_empty() || path.starts_with('/') {
        return None;
    }
    let mut rest = path;
    for (position, part) in segment.iter().enumerate() {
        match part {
            Part::Literal(text) => rest = rest.strip_prefix(*text)?,
            Part::Param(name) => {
                // A parameter is the text up to the next literal, of which there may be none.
                let next = segment[position + 1..].iter().find_map(|part| match part {
                    Part::Literal(text) => Some(*text),
                    Part::Param(_) => None,
                });
                let value = match next {
                    Some(text) => {
                        let value = rest.split_once(text)?.0;
                        // `rest` keeps the literal, which the literal part strips.
                        rest = &rest[value.len()..];
                        value
                    }
                    None => std::mem::take(&mut rest),
                };
                if value.is_empty() {
                    return None;
                }
                params.insert((*name).to_owned(), decode(value));
            }
        }
    }
    rest.is_empty().then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table shaped like the ones the fakes generate: catch-alls beside longer static
    /// routes, and a static segment beside a parameter at the same position.
    static OPERATIONS: &[&OperationInfo] = &[
        &OperationInfo {
            id: "git/list-matching-refs",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/git/matching-refs/{ref}",
        },
        &OperationInfo {
            id: "repos/list-for-authenticated-user",
            method: Method::Get,
            path: "/user/repos",
        },
        &OperationInfo {
            id: "repos/get-content",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/contents/{path}",
        },
        &OperationInfo {
            id: "repos/get-commit",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/commits/{ref}",
        },
        &OperationInfo {
            id: "repos/get-combined-status-for-ref",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/commits/{ref}/status",
        },
        &OperationInfo {
            id: "repos/get-release-by-tag",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/releases/tags/{tag}",
        },
        &OperationInfo {
            id: "repos/get-latest-release",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/releases/latest",
        },
        &OperationInfo {
            id: "orgs/block-user",
            method: Method::Post,
            path: "/orgs/{org}/blocks/{username}",
        },
        &OperationInfo {
            id: "applications/create-installation-access-token",
            method: Method::Post,
            path: "/applications/{client_id}/token",
        },
    ];

    /// A table with the templates `matchit` rejects: parameters sharing a segment with
    /// literal text, beside plain parameters and a static route.
    static MIXED: &[&OperationInfo] = &[
        &OperationInfo {
            id: "repo/get-single-commit",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/git/commits/{sha}",
        },
        &OperationInfo {
            id: "repo/download-commit-diff-or-patch",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/git/commits/{sha}.{diffType}",
        },
        &OperationInfo {
            id: "repo/get-pinned-pull-request",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/pulls/pinned",
        },
        &OperationInfo {
            id: "repo/raw-file",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/raw/{filepath}",
        },
        &OperationInfo {
            id: "repo/serve-blob",
            method: Method::Get,
            path: "/repos/{owner}/{repo}/blobs/{sha}.{kind}/{path}",
        },
    ];

    fn find(table: &'static [&'static OperationInfo], path: &str) -> Option<Match> {
        Router::new(table).find(Method::Get, path)
    }

    fn found(
        table: &'static [&'static OperationInfo],
        path: &str,
    ) -> (&'static str, HashMap<String, String>) {
        let found = find(table, path).expect("the path matches an operation");
        (found.info.id, found.params)
    }

    #[test]
    fn prefers_static_segments() {
        let (id, _) = found(OPERATIONS, "/repos/o/r/git/matching-refs/heads/rpc/");
        assert_eq!(id, "git/list-matching-refs");
        let (id, params) = found(OPERATIONS, "/user/repos");
        assert_eq!(id, "repos/list-for-authenticated-user");
        assert!(params.is_empty());
    }

    #[test]
    fn catch_all_keeps_slashes() {
        let (id, params) = found(OPERATIONS, "/repos/o/r/contents/scripts/a%20b.sh");
        assert_eq!(id, "repos/get-content");
        assert_eq!(params["path"], "scripts/a b.sh");
        assert_eq!(params["owner"], "o");
    }

    #[test]
    fn longer_static_route_beats_catch_all() {
        let (id, params) = found(OPERATIONS, "/repos/o/r/commits/main/status");
        assert_eq!(id, "repos/get-combined-status-for-ref");
        assert_eq!(params["ref"], "main");
        let (id, params) = found(OPERATIONS, "/repos/o/r/commits/feature/x");
        assert_eq!(id, "repos/get-commit");
        assert_eq!(params["ref"], "feature/x");
    }

    #[test]
    fn empty_parameters_do_not_match() {
        assert!(find(OPERATIONS, "/applications//token").is_none());
        assert!(find(OPERATIONS, "/orgs/x//blocks").is_none());
        assert!(find(OPERATIONS, "/repos/o/r/contents/").is_none());
        assert!(find(OPERATIONS, "/repos/o/r/contents//a").is_none());
    }

    #[test]
    fn static_segment_beats_parameter() {
        let (id, params) = found(OPERATIONS, "/repos/o/r/releases/tags/assets");
        assert_eq!(id, "repos/get-release-by-tag");
        assert_eq!(params["tag"], "assets");
    }

    #[test]
    fn unknown_paths_and_methods_do_not_match() {
        assert!(find(OPERATIONS, "/no/such/route").is_none());
        assert!(
            Router::new(OPERATIONS)
                .find(Method::Put, "/user/repos")
                .is_none()
        );
    }

    #[test]
    fn every_operation_routes_to_itself() {
        for table in [OPERATIONS, MIXED] {
            let router = Router::new(table);
            for info in table {
                let path = info
                    .path
                    .split('/')
                    .map(|segment| {
                        // Every parameter, of every segment, gets a name of its own.
                        let mut filled = String::new();
                        let mut rest = segment;
                        while let Some(start) = rest.find('{') {
                            filled.push_str(&rest[..start]);
                            let (name, after) = rest[start + 1..].split_once('}').unwrap();
                            assert!(!name.is_empty(), "{} has an empty parameter", info.id);
                            filled.push('x');
                            rest = after;
                        }
                        filled.push_str(rest);
                        filled
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                let found = router.find(info.method, &path).unwrap();
                assert_eq!(
                    found.info.path,
                    info.path,
                    "{} {} routed to {}",
                    info.method.as_str(),
                    info.path,
                    found.info.id
                );
            }
        }
    }

    #[test]
    fn parameters_sharing_a_segment_are_matched_by_hand() {
        let (id, params) = found(MIXED, "/repos/o/r/git/commits/abc123.diff");
        assert_eq!(id, "repo/download-commit-diff-or-patch");
        assert_eq!(params["sha"], "abc123");
        assert_eq!(params["diffType"], "diff");
        let (id, _) = found(MIXED, "/repos/o/r/git/commits/abc123");
        assert_eq!(id, "repo/get-single-commit");
    }

    #[test]
    fn mixed_parameters_must_be_non_empty() {
        // What a mixed template refuses, a plain parameter still matches.
        let (id, _) = found(MIXED, "/repos/o/r/git/commits/.patch");
        assert_eq!(id, "repo/get-single-commit");
        let (id, _) = found(MIXED, "/repos/o/r/git/commits/abc123.");
        assert_eq!(id, "repo/get-single-commit");
        assert!(find(MIXED, "/repos/o/r/git/commits/abc123.diff/x").is_none());
    }

    #[test]
    fn a_static_route_beats_a_mixed_template() {
        let (id, params) = found(MIXED, "/repos/o/r/pulls/pinned");
        assert_eq!(id, "repo/get-pinned-pull-request");
        assert_eq!(params["owner"], "o");
        assert_eq!(params["repo"], "r");
    }

    #[test]
    fn mixed_templates_may_end_in_a_catch_all() {
        let (id, params) = found(MIXED, "/repos/o/r/blobs/abc.tree/src/main.rs");
        assert_eq!(id, "repo/serve-blob");
        assert_eq!(params["sha"], "abc");
        assert_eq!(params["kind"], "tree");
        assert_eq!(params["path"], "src/main.rs");
        assert!(find(MIXED, "/repos/o/r/blobs/abc.tree").is_none());
        assert!(find(MIXED, "/repos/o/r/blobs/abc.tree/").is_none());
    }

    #[test]
    fn catch_all_routes_keep_slashes() {
        let (id, params) = found(MIXED, "/repos/o/r/raw/dir/a%20b.txt");
        assert_eq!(id, "repo/raw-file");
        assert_eq!(params["filepath"], "dir/a b.txt");
    }
}
