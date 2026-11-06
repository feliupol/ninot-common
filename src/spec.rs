//! Turning a pinned API description into the operation table a fake routes with.
//!
//! Both OpenAPI 3, which GitHub publishes, and Swagger 2.0, which Forgejo publishes, list
//! their operations as `paths` -> method -> `operationId` and `summary`. Swagger 2.0 also has
//! a `basePath`, such as `/api/v1`, that prefixes every path; OpenAPI 3 has none.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use serde::Deserialize;
use serde_json::Value;

/// The parts of an OpenAPI 3 or Swagger 2.0 description the operation table needs.
#[derive(Deserialize)]
struct Spec {
    /// Swagger 2.0's prefix for every path, such as `/api/v1`; absent in OpenAPI 3.
    #[serde(default, rename = "basePath")]
    base_path: String,
    paths: BTreeMap<String, PathItem>,
}

/// One path's operations, in the order the table lists methods.
#[derive(Deserialize)]
struct PathItem {
    get: Option<Operation>,
    post: Option<Operation>,
    put: Option<Operation>,
    patch: Option<Operation>,
    delete: Option<Operation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Operation {
    operation_id: String,
    summary: Option<String>,
}

/// The Rust source of `spec`'s operation table: one type per operation and the table itself,
/// to be included where [`crate::operation`]'s types are in scope.
///
/// # Panics
///
/// Panics when an `operationId` maps to a type that already exists, as two operations would
/// then be indistinguishable.
pub fn operations(spec: &Value) -> String {
    let spec: Spec = serde_json::from_value(spec.clone())
        .expect("the pinned spec is not a valid API description");
    let base = prefix(&spec.base_path);

    let mut names = BTreeSet::new();
    let mut types = Vec::new();
    let mut table = Vec::new();
    for (path, item) in &spec.paths {
        let methods = [
            ("Get", &item.get),
            ("Post", &item.post),
            ("Put", &item.put),
            ("Patch", &item.patch),
            ("Delete", &item.delete),
        ];
        for (method, operation) in methods {
            let Some(operation) = operation else {
                continue;
            };
            let id = &operation.operation_id;
            let name = format_ident!("{}", type_name(id));
            assert!(
                names.insert(name.to_string()),
                "{id} maps to a duplicate type {name}"
            );
            let summary = operation.summary.as_deref().unwrap_or(id);
            let method = format_ident!("{method}");
            let path = format!("{base}{path}");
            types.push(quote! {
                #[doc = #summary]
                pub struct #name;
                impl Operation for #name {
                    const INFO: &'static OperationInfo = &OperationInfo {
                        id: #id,
                        method: Method::#method,
                        path: #path,
                    };
                }
            });
            table.push(quote! { ops::#name::INFO });
        }
    }

    let generated: TokenStream = quote! {
        /// Every operation in the pinned spec, ordered by path.
        pub static OPERATIONS: &[&OperationInfo] = &[#(#table),*];

        /// One type per operation in the pinned spec.
        pub mod ops {
            use super::{Method, Operation, OperationInfo};
            #(#types)*
        }
    };
    let file = syn::parse2(generated).expect("the generated operations are not valid Rust");
    prettyplease::unparse(&file)
}

/// The `basePath` as a path prefix: empty, or `/api/v1` without a trailing slash.
fn prefix(base_path: &str) -> String {
    let trimmed = base_path.trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    assert!(
        trimmed.starts_with('/'),
        "basePath {base_path} does not start with a slash"
    );
    trimmed.to_owned()
}

/// Maps `repos/get-content` to `ReposGetContent`.
fn type_name(id: &str) -> String {
    id.split(['/', '-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let first = chars.next().unwrap().to_ascii_uppercase();
            std::iter::once(first).chain(chars).collect::<String>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openapi_3_paths_are_used_as_they_are() {
        let spec = json!({
            "openapi": "3.0.3",
            "paths": {
                "/repos/{owner}/{repo}": {
                    "get": { "operationId": "repos/get", "summary": "Get a repository" }
                }
            }
        });
        let generated = operations(&spec);
        assert!(generated.contains(r#"id: "repos/get""#), "{generated}");
        assert!(
            generated.contains(r#"path: "/repos/{owner}/{repo}""#),
            "{generated}"
        );
        assert!(generated.contains("///Get a repository"), "{generated}");
        assert!(generated.contains("pub struct ReposGet;"), "{generated}");
        assert!(generated.contains("ops::ReposGet::INFO"), "{generated}");
    }

    #[test]
    fn swagger_2_base_path_prefixes_every_path() {
        let spec = json!({
            "swagger": "2.0",
            "basePath": "/api/v1",
            "paths": {
                "/user/repos": {
                    "post": { "operationId": "user/create-repo", "summary": "Create a repository" }
                },
                "/version": {
                    "get": { "operationId": "version/get", "summary": "Get the version" }
                }
            }
        });
        let generated = operations(&spec);
        assert!(
            generated.contains(r#"path: "/api/v1/user/repos""#),
            "{generated}"
        );
        assert!(
            generated.contains(r#"path: "/api/v1/version""#),
            "{generated}"
        );
        assert!(generated.contains("///Create a repository"), "{generated}");
    }

    #[test]
    fn a_missing_summary_falls_back_to_the_operation_id() {
        let spec = json!({
            "paths": { "/zen": { "get": { "operationId": "meta/get-zen" } } }
        });
        assert!(operations(&spec).contains("///meta/get-zen"));
    }
}
