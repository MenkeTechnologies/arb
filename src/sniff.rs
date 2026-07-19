//! Zero-config input sniffing (SPEC §1/§19): `cmd | arb` with no spec peeks the
//! first stream lines and picks the stdlib preset whose `source` pipeline matches
//! the data shape, instead of the generic tail. Pure and unit-testable; every
//! returned name is guarded against [`crate::spec::STDLIB_NAMES`] so a stdlib
//! rename can't route to a missing module.

/// Pick a preset name for the peeked lines, or `None` to keep the default tail.
/// First match wins; rules are keyed to what each preset's pipeline consumes.
pub fn sniff(first: &[&str]) -> Option<&'static str> {
    let lines: Vec<&str> = first
        .iter()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .collect();
    let l0 = *lines.first()?;
    let guard = |name: &'static str| crate::spec::STDLIB_NAMES.contains(&name).then_some(name);

    // 1. JSON object stream.
    if l0.trim_start().starts_with('{') {
        let has = |k: &str| l0.contains(&format!("\"{k}\""));
        if has("status") && (has("request") || has("method")) {
            return guard("nginx");
        }
        if has("level") || has("lvl") || has("severity") {
            return guard("logs");
        }
        return guard("json");
    }

    // 2. Header-line shape (a tool's first line is its column header).
    let toks: Vec<&str> = l0.split_whitespace().collect();
    let has_tok = |t: &str| toks.contains(&t);
    if l0.starts_with("CONTAINER") {
        return guard("docker");
    }
    if has_tok("USER") && has_tok("PID") && has_tok("%CPU") {
        return guard("top");
    }
    if has_tok("NAME") && has_tok("READY") && has_tok("STATUS") {
        return guard("k8s");
    }

    // 3. git log --oneline: most lines start with a short hex sha + space.
    let hexish = |l: &str| {
        let sha = l.split_whitespace().next().unwrap_or("");
        (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
    };
    if lines.len() >= 2 && lines.iter().filter(|l| hexish(l)).count() * 5 >= lines.len() * 3 {
        return guard("git");
    }
    if lines.len() == 1 && hexish(l0) {
        return guard("git");
    }

    // 4. Tab- or comma-separated columns -> a table.
    if l0.contains('\t') || l0.contains(',') {
        return guard("table");
    }

    // logfmt (key=value) is intentionally not mapped: no preset consumes it, so
    // routing it to `logs` (JSON-only) would mis-parse — keep the default tail.
    None
}

#[cfg(test)]
mod tests {
    use super::sniff;

    #[test]
    fn json_streams() {
        assert_eq!(sniff(&[r#"{"a":1}"#, r#"{"a":2}"#]), Some("json"));
        assert_eq!(sniff(&[r#"{"level":"info","m":"x"}"#]), Some("logs"));
        assert_eq!(
            sniff(&[r#"{"status":200,"request":"GET /"}"#]),
            Some("nginx")
        );
        assert_eq!(sniff(&[r#"{"status":200}"#]), Some("json")); // status alone -> json
    }

    #[test]
    fn tool_headers() {
        assert_eq!(sniff(&["CONTAINER ID   NAME   CPU %"]), Some("docker"));
        assert_eq!(sniff(&["USER  PID  %CPU  %MEM  COMMAND"]), Some("top"));
        assert_eq!(sniff(&["NAME   READY   STATUS   RESTARTS"]), Some("k8s"));
    }

    #[test]
    fn git_and_columns() {
        assert_eq!(
            sniff(&["a1b2c3d fix thing", "e4f5a6b add thing"]),
            Some("git")
        );
        assert_eq!(sniff(&["a,b,c", "1,2,3"]), Some("table"));
        assert_eq!(sniff(&["a\tb\tc"]), Some("table"));
    }

    #[test]
    fn no_match_keeps_default() {
        assert_eq!(sniff(&["just some prose here"]), None);
        assert_eq!(sniff(&["key=value dur=5"]), None); // logfmt -> default tail
        assert_eq!(sniff(&[]), None);
        assert_eq!(sniff(&["", "  "]), None);
    }
}
