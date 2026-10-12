pub(super) fn advance_rate_limit_bucket(
    current_line: u64,
    now: u64,
    add_line: u64,
    window_size: u64,
) -> Result<u64, u64> {
    if current_line == 0 {
        return Ok(0);
    }

    let limit_line = now.saturating_add(window_size);
    let added_line = add_rate_usage(current_line, now, add_line);

    if added_line > limit_line {
        let wait_sec = added_line - limit_line;
        Err(now.saturating_add(wait_sec))
    } else {
        Ok(added_line)
    }
}

pub(super) fn add_rate_usage(current_line: u64, now: u64, add_line: u64) -> u64 {
    if current_line == 0 {
        0
    } else if current_line < now {
        now.saturating_add(add_line)
    } else {
        current_line.saturating_add(add_line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_blocks_usage_that_would_cross_its_window() {
        let first = advance_rate_limit_bucket(1, 1_000, 3_600, 3_600).unwrap();
        assert_eq!(first, 4_600);
        assert_eq!(
            advance_rate_limit_bucket(first, 1_000, 3_600, 3_600),
            Err(4_600)
        );
        assert_eq!(
            advance_rate_limit_bucket(first, 4_600, 3_600, 3_600),
            Ok(8_200)
        );
    }
}
