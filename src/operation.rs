//! The types of the operations a fake serves, as its spec describes them.

/// HTTP method of an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    /// The method as it appears on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// Static description of one operation.
#[derive(Debug, PartialEq, Eq)]
pub struct OperationInfo {
    /// The spec's `operationId`, such as `repos/get`.
    pub id: &'static str,
    pub method: Method,
    /// Path template, such as `/repos/{owner}/{repo}`.
    pub path: &'static str,
}

/// Implemented by the generated type of every operation.
pub trait Operation {
    const INFO: &'static OperationInfo;
}
