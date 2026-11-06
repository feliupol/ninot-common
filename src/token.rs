//! The unique, unguessable tokens a fake hands out.

/// Generates a token that no other call returns, prefixed with `prefix`, such as `ghp_`.
///
/// The prefix is what clients recognise the token's kind by, so a fake passes the one its API
/// documents. The 18 random bytes come from the operating system's CSPRNG.
pub fn new_token(prefix: &str) -> String {
    let mut bytes = [0u8; 18];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    let mut token = String::from(prefix);
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}
