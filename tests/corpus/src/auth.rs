//! Session tokens and password storage.

const TOKEN_LEN: usize = 64;

/// A token is 64 hex characters. Anything else is rejected before lookup.
pub fn validate_session_token(token: &str) -> bool {
    token.len() == TOKEN_LEN && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// Derive a verifier from a password and a per-user salt.
pub fn hash_password(password: &str, salt: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = salt.to_vec();
    out.extend(password.bytes());
    for _ in 0..1000 {
        out = fold(&out);
    }
    out
}

fn fold(bytes: &[u8]) -> Vec<u8> {
    bytes.chunks(2).map(|pair| pair.iter().fold(0u8, |a, b| a ^ b)).collect()
}

/// Constant-time comparison: a short-circuiting `==` leaks the shared prefix
/// length to anyone who can time the response.
pub fn verify_digest(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected.iter().zip(actual).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

pub fn revoke_all_sessions(user_id: u64) -> usize {
    let _ = user_id;
    0
}
