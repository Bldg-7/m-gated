pub fn assert_cases(stdout: &str, expected: &[usize]) {
    assert!(!stdout.contains("U0_E2E global_error="), "고정 runner 오류");
    let mut observed = Vec::new();
    for row in stdout
        .lines()
        .filter_map(|line| line.strip_prefix("U0_E2E case="))
    {
        let mut fields = row.split_whitespace();
        let id = fields
            .next()
            .unwrap()
            .parse::<usize>()
            .expect("미등록 browser 사례는 성공으로 세지 않음");
        assert_eq!(
            fields.next(),
            Some("status=passed"),
            "browser 사례 {id} 미통과"
        );
        observed.push(id);
    }
    observed.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(
        observed, expected,
        "browser scope의 누락/중복/skip은 허용하지 않음"
    );
}
// 사례 결과를 먼저 보존한다. annotation은 별도 유한 예산이며 새 사례를 밀어내지 않는다.
pub fn forward_sanitized(stdout: &str) {
    let cases = stdout
        .lines()
        .filter(|line| line.starts_with("U0_E2E case="))
        .take(27);
    let annotations = stdout
        .lines()
        .filter(|line| line.starts_with("U0_E2E ") && !line.starts_with("U0_E2E case="))
        .take(32);
    for line in cases.chain(annotations) {
        if line.len() <= 400 && line.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
            eprintln!("{line}");
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_results_reject_missing_duplicate_skipped_and_unregistered_cases() {
        assert_cases(
            "U0_E2E case=26 status=passed assertion_lines=none\nU0_E2E case=27 status=passed assertion_lines=none",
            &[26, 27],
        );
        for stdout in [
            "",
            "U0_E2E case=26 status=skipped",
            "U0_E2E case=unregistered status=passed",
            "U0_E2E case=26 status=passed\nU0_E2E case=26 status=passed",
        ] {
            assert!(std::panic::catch_unwind(|| assert_cases(stdout, &[26, 27])).is_err());
        }
    }
}
