//! Longest Common Subsequence over normalized word tokens.
//! Used by `reconcile.rs` to anchor authoritative text into ASR timing.

/// Normalize a word for comparison: lowercase + strip non-alphanumeric.
pub fn norm(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// LCS index pairs. Returns `Vec<(i_in_a, i_in_b)>` for matched positions
/// in order. Standard DP; O(n*m) time, O(n*m) space — fine for songs
/// (≤2000 words per track).
pub fn lcs_pairs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    if n == 0 || m == 0 {
        return Vec::new();
    }

    // dp[i][j] = LCS length of a[..i] / b[..j]
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            if a[i] == b[j] {
                dp[i + 1][j + 1] = dp[i][j] + 1;
            } else {
                dp[i + 1][j + 1] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    // Backtrack
    let mut pairs = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            pairs.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    pairs.reverse();
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norms(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| norm(w)).collect()
    }

    #[test]
    fn norm_strips_punctuation_and_lowercases() {
        assert_eq!(norm("Hello,"), "hello");
        assert_eq!(norm("It's"), "its");
        assert_eq!(norm("Praise!"), "praise");
    }

    #[test]
    fn lcs_identical_sequences() {
        let a = norms(&["hello", "world"]);
        let b = norms(&["hello", "world"]);
        assert_eq!(lcs_pairs(&a, &b), vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn lcs_finds_anchors_with_one_swap() {
        // "I got a God" (whisperX) vs "I've got a God" (spotify)
        let a = norms(&["i", "got", "a", "god"]);
        let b = norms(&["ive", "got", "a", "god"]);
        let pairs = lcs_pairs(&a, &b);
        // "got","a","god" match
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], (1, 1));
    }

    #[test]
    fn lcs_handles_empty() {
        let a: Vec<String> = vec![];
        let b = norms(&["hello"]);
        assert_eq!(lcs_pairs(&a, &b), vec![]);
    }
}
