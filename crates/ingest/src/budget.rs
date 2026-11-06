//! M0 memory budget: past it, batches that would add new runs are refused.

pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_RUNS: usize = 500_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub max_bytes: u64,
    pub max_runs: usize,
}

impl Default for Budget {
    fn default() -> Budget {
        Budget {
            max_bytes: DEFAULT_MAX_BYTES,
            max_runs: DEFAULT_MAX_RUNS,
        }
    }
}

impl Budget {
    pub fn is_full(&self, bytes: u64, runs: usize) -> bool {
        bytes >= self.max_bytes || runs >= self.max_runs
    }
}

/// Parses sizes like `2GB`, `512MB`, `1048576`.
pub fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KB" | "K" => 1024,
        "MB" | "M" => 1024 * 1024,
        "GB" | "G" => 1024 * 1024 * 1024,
        _ => return None,
    };
    n.checked_mul(mult)
}
