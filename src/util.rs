//! Pure string helpers shared across the plugin.

/// Replace characters that are unsafe in a path with `-`.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Expand `{{ name }}` and `{{ name | sanitize }}` template variables.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, val) in vars {
        out = out.replace(&format!("{{{{ {name} }}}}"), val);
        out = out.replace(&format!("{{{{ {name} | sanitize }}}}"), &sanitize(val));
    }
    out
}

/// Drop `refs/heads/`, `refs/remotes/`, then `origin/` prefixes (each at most once).
pub fn strip_remote(r: &str) -> String {
    let mut r = r;
    for p in ["refs/heads/", "refs/remotes/", "origin/"] {
        r = r.strip_prefix(p).unwrap_or(r);
    }
    r.to_string()
}

/// Single-quote a string for safe embedding in a POSIX shell command.
pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
