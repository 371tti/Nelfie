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
    let added_line = if current_line < now {
        now.saturating_add(add_line)
    } else {
        current_line.saturating_add(add_line)
    };

    if added_line > limit_line {
        let wait_sec = added_line - limit_line;
        Err(now.saturating_add(wait_sec))
    } else {
        Ok(added_line)
    }
}
