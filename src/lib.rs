//! The optional memory service shares a protocol, not the agent's tool authority.
//! The cloud launcher (`rusty-cloud`) reaches Daytona over its REST API.
pub mod advisor;
pub mod cloud;
pub mod locate;
pub mod privacy;
pub mod recall;
pub mod workspace_query;

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// FNV-1a; stable across Rust versions, unlike `DefaultHasher`.
pub fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}
