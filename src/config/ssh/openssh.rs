use crate::i18n::messages as i18n_key;

use super::{AuthMethod, PortForward, SshConfig, SshJumpHost, parse_endpoint, parse_forwards};
use crate::config::{Result, invalid, validate_text};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

enum Directive {
    Setting(Vec<String>),
    Include(Vec<Directive>),
}

/// Imports the effective, non-executable OpenSSH settings for one alias.
/// Unsupported matching directives are rejected instead of silently changing security or routing.
pub fn import_openssh(path: &Path, alias: &str) -> Result<SshConfig> {
    validate_text(alias, 256, i18n_key::CONNECTIONS_SSH_HOST_ALIAS)?;
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| invalid(i18n_key::CONNECTIONS_HOME_IS_NOT_CONFIGURED))?;
    let mut source = Vec::new();
    read_config(path, &home, 0, &mut 0, &mut source)?;
    resolve(&source, alias, &home, &mut Vec::new())
}
fn read_config(
    path: &Path,
    home: &Path,
    depth: usize,
    total: &mut usize,
    lines: &mut Vec<Directive>,
) -> Result<()> {
    if depth > 8 {
        return Err(invalid(
            i18n_key::CONNECTIONS_SSH_CONFIGURATION_INCLUDES_ARE_NESTED_TOO_DEEPLY,
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid(
            i18n_key::CONNECTIONS_SSH_CONFIGURATION_MUST_BE_A_REGULAR_FILE,
        ));
    }
    let mut text = String::new();
    file.by_ref()
        .take((1024 * 1024 + 1) as u64)
        .read_to_string(&mut text)?;
    *total += text.len();
    if *total > 1024 * 1024 {
        return Err(invalid(
            i18n_key::CONNECTIONS_SSH_CONFIGURATION_EXCEEDS_1_MIB,
        ));
    }
    for line in text.lines() {
        let words = words(line)?;
        if words.is_empty() {
            continue;
        }
        if words[0].eq_ignore_ascii_case("include") {
            if words.len() < 2 {
                return Err(invalid(
                    i18n_key::CONNECTIONS_AN_SSH_CONFIGURATION_DIRECTIVE_REQUIRES_ONE_VALUE,
                ));
            }
            for include in &words[1..] {
                if include.contains(['%', '$']) {
                    return Err(invalid(
                        i18n_key::CONNECTIONS_USE_A_LITERAL_PATH_OR_IN_SSH_INCLUDE,
                    ));
                }
                let expanded = expand_path(include, home, "", "", 22)?;
                let expanded = if expanded.is_absolute() {
                    expanded
                } else {
                    home.join(".ssh").join(expanded)
                };
                for path in include_paths(&expanded)? {
                    let mut included = Vec::new();
                    read_config(&path, home, depth + 1, total, &mut included)?;
                    lines.push(Directive::Include(included));
                }
            }
        } else {
            lines.push(Directive::Setting(words));
        }
    }
    Ok(())
}
fn include_paths(pattern: &Path) -> Result<Vec<PathBuf>> {
    let Some(name) = pattern.file_name().and_then(|name| name.to_str()) else {
        return Err(invalid(i18n_key::CONNECTIONS_INVALID_SSH_INCLUDE_PATH));
    };
    if !name.contains(['*', '?']) {
        return Ok(if pattern.exists() {
            vec![pattern.into()]
        } else {
            Vec::new()
        });
    }
    let directory = pattern.parent().unwrap_or(Path::new("."));
    if directory.to_string_lossy().contains(['*', '?']) {
        return Err(invalid(
            i18n_key::CONNECTIONS_SSH_INCLUDE_WILDCARDS_ARE_SUPPORTED_ONLY_IN_THE_FILENAME,
        ));
    }
    let mut paths = Vec::new();
    for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
        let entry = entry?;
        if index >= 4096 {
            return Err(invalid(
                i18n_key::CONNECTIONS_TOO_MANY_SSH_CONFIGURATION_INCLUDES,
            ));
        }
        if wildcard(name, &entry.file_name().to_string_lossy()) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}
fn words(line: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '#' {
            break;
        } else if c.is_whitespace() || (c == '=' && words.is_empty()) {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if escaped || quote.is_some() {
        return Err(invalid(
            i18n_key::CONNECTIONS_UNCLOSED_QUOTE_OR_ESCAPE_IN_SSH_CONFIGURATION,
        ));
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}
fn wildcard(pattern: &str, value: &str) -> bool {
    let (p, v) = (pattern.as_bytes(), value.as_bytes());
    let (mut i, mut j, mut star, mut restart) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && (p[i] == b'?' || p[i].eq_ignore_ascii_case(&v[j])) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            i += 1;
            restart = j;
        } else if let Some(s) = star {
            restart += 1;
            j = restart;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}
fn host_matches(patterns: &[String], alias: &str) -> bool {
    let mut found = false;
    for p in patterns {
        if let Some(p) = p.strip_prefix('!') {
            if wildcard(p, alias) {
                return false;
            }
        } else {
            found |= wildcard(p, alias);
        }
    }
    found
}
fn collect(
    lines: &[Directive],
    alias: &str,
    values: &mut BTreeMap<String, String>,
    forwards: &mut Vec<PortForward>,
) -> Result<()> {
    let mut selected = true;
    for directive in lines {
        let words = match directive {
            Directive::Setting(words) => words,
            Directive::Include(included) => {
                if selected {
                    collect(included, alias, values, forwards)?;
                }
                continue;
            }
        };
        let key = words[0].to_ascii_lowercase();
        if key == "host" {
            selected = host_matches(&words[1..], alias);
            continue;
        }
        if key == "match" {
            return Err(invalid(
                i18n_key::CONNECTIONS_OPENSSH_MATCH_BLOCKS_ARE_NOT_SUPPORTED_BY_THE_IMPORTER_CREATE,
            ));
        }
        if !selected {
            continue;
        }
        if ["localforward", "remoteforward", "dynamicforward"].contains(&key.as_str()) {
            let prefix = match key.as_str() {
                "localforward" => "L",
                "remoteforward" => "R",
                _ => "D",
            };
            forwards.extend(parse_forwards(&format!(
                "{prefix} {}",
                words[1..].join(" ")
            ))?);
            continue;
        }
        if ![
            "hostname",
            "user",
            "port",
            "identityfile",
            "identityagent",
            "proxyjump",
            "connecttimeout",
            "serveraliveinterval",
            "serveralivecountmax",
            "preferredauthentications",
        ]
        .contains(&key.as_str())
        {
            return Err(invalid(crate::msg!(
                i18n_key::CONNECTIONS_UNSUPPORTED_SSH_CONFIGURATION_DIRECTIVE_DIRECTIVE,
                directive = words[0]
            )));
        }
        if words.len() != 2 {
            return Err(invalid(
                i18n_key::CONNECTIONS_AN_SSH_CONFIGURATION_DIRECTIVE_REQUIRES_ONE_VALUE,
            ));
        }
        if key == "identityfile" && values.get(&key).is_some_and(|prior| prior != &words[1]) {
            return Err(invalid(i18n_key::CONNECTIONS_MULTIPLE_SSH_IDENTITYFILE_ENTRIES_REQUIRE_SEPARATE_CONNECTION_PROFILES));
        }
        values.entry(key).or_insert_with(|| words[1].clone());
    }
    Ok(())
}
fn resolve(
    lines: &[Directive],
    alias: &str,
    home: &Path,
    visited: &mut Vec<String>,
) -> Result<SshConfig> {
    if visited.len() >= 8 || visited.iter().any(|a| a == alias) {
        return Err(invalid(
            i18n_key::CONNECTIONS_SSH_PROXYJUMP_CONTAINS_A_CYCLE_OR_TOO_MANY_HOPS,
        ));
    }
    visited.push(alias.into());
    let mut values = BTreeMap::new();
    let mut forwards = Vec::new();
    collect(lines, alias, &mut values, &mut forwards)?;
    let username = values
        .get("user")
        .cloned()
        .or_else(|| std::env::var("USER").ok())
        .ok_or_else(|| invalid(i18n_key::CONNECTIONS_SSH_USERNAME_IS_REQUIRED))?;
    let port = values
        .get("port")
        .map(|v| v.parse::<u16>())
        .transpose()
        .map_err(|_| invalid(i18n_key::CONNECTIONS_INVALID_SSH_ENDPOINT))?
        .unwrap_or(22);
    let host = values
        .get("hostname")
        .cloned()
        .unwrap_or_else(|| alias.into())
        .replace("%h", alias);
    let agent_value = values.get("identityagent").map(String::as_str);
    let agent_socket = agent_value
        .filter(|value| !matches!(*value, "none" | "SSH_AUTH_SOCK"))
        .map(|value| expand_path(value, home, &host, &username, port))
        .transpose()?;
    let auth = match values.get("preferredauthentications").map(String::as_str) {
        Some("password") => AuthMethod::Password { credential: None },
        Some("keyboard-interactive") => AuthMethod::KeyboardInteractive,
        Some("publickey") | None => {
            if let Some(path) = values
                .get("identityfile")
                .filter(|path| path.as_str() != "none")
            {
                AuthMethod::PrivateKey {
                    path: expand_path(path, home, &host, &username, port)?,
                    passphrase: None,
                }
            } else if agent_value == Some("none") {
                return Err(invalid(
                    i18n_key::CONNECTIONS_SSH_AGENT_IS_DISABLED_SELECT_A_PRIVATE_KEY_OR_ANOTHER,
                ));
            } else {
                AuthMethod::Agent {
                    socket: agent_socket,
                }
            }
        }
        Some(_) => {
            return Err(invalid(
                i18n_key::CONNECTIONS_IMPORT_ONE_PREFERRED_SSH_AUTHENTICATION_METHOD_PER_PROFILE,
            ));
        }
    };
    let mut config = SshConfig {
        host,
        port,
        username,
        auth,
        forwards,
        ..SshConfig::default()
    };
    for (name, target) in [
        ("connecttimeout", &mut config.timeout_secs),
        ("serveraliveinterval", &mut config.keepalive_secs),
    ] {
        if let Some(v) = values.get(name) {
            *target = v
                .parse()
                .map_err(|_| invalid(i18n_key::CONNECTIONS_INVALID_SSH_TIMEOUT))?;
        }
    }
    if let Some(v) = values.get("serveralivecountmax") {
        config.keepalive_max = v
            .parse()
            .map_err(|_| invalid(i18n_key::CONNECTIONS_INVALID_SSH_TIMEOUT))?;
    }
    if let Some(jumps) = values.get("proxyjump").filter(|v| v.as_str() != "none") {
        for value in jumps.split(',') {
            let (user, address) = value
                .rsplit_once('@')
                .map(|(u, a)| (Some(u), a))
                .unwrap_or((None, value));
            let (hop_alias, _) = parse_endpoint(address, None, Some(22))?;
            let mut hop = resolve(lines, &hop_alias, home, visited)?;
            if let Some(user) = user {
                hop.username = user.into();
            }
            if address.contains(':') {
                hop.port = parse_endpoint(address, None, Some(22))?.1;
            }
            config.jump_hosts.extend(hop.jump_hosts);
            let (identity_file, agent_socket) = match hop.auth {
                AuthMethod::PrivateKey { path, .. } => (Some(path), None),
                AuthMethod::Agent { socket } => (None, socket),
                _ => {
                    return Err(invalid(
                        i18n_key::CONNECTIONS_JUMP_HOSTS_USE_AN_SSH_AGENT_OR_A_PRIVATE_KEY,
                    ));
                }
            };
            config.jump_hosts.push(SshJumpHost {
                host: hop.host,
                port: hop.port,
                username: hop.username,
                identity_file,
                agent_socket,
            });
        }
    }
    visited.pop();
    config.validate()?;
    Ok(config)
}
fn expand_path(value: &str, home: &Path, host: &str, user: &str, port: u16) -> Result<PathBuf> {
    let value = if let Some(var) = value.strip_prefix('$') {
        std::env::var(var).map_err(|_| {
            invalid(crate::msg!(
                i18n_key::CONNECTIONS_SSH_ENVIRONMENT_VARIABLE_IS_UNAVAILABLE_NAME,
                name = var
            ))
        })?
    } else {
        value.into()
    };
    let mut expanded = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            expanded.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => expanded.push('%'),
            Some('d') => expanded.push_str(&home.to_string_lossy()),
            Some('h') => expanded.push_str(host),
            Some('r') => expanded.push_str(user),
            Some('p') => expanded.push_str(&port.to_string()),
            _ => {
                return Err(invalid(
                    i18n_key::CONNECTIONS_UNSUPPORTED_TOKEN_IN_SSH_CONFIGURATION_PATH,
                ));
            }
        }
    }
    let value = expanded;
    if let Some(rest) = value.strip_prefix("~/") {
        Ok(home.join(rest))
    } else if value.starts_with('~') {
        Err(invalid(
            i18n_key::CONNECTIONS_ONLY_THE_CURRENT_USER_S_HOME_DIRECTORY_IS_SUPPORTED,
        ))
    } else {
        Ok(value.into())
    }
}

#[cfg(test)]
mod tests {
    use super::{Directive, expand_path, import_openssh, resolve, words};
    use crate::config::{AuthMethod, Result};
    use std::path::Path;

    #[test]
    fn first_matching_host_value_wins_and_negation_applies() {
        let lines="Host prod\n HostName 127.0.0.2\n User operator\nHost * !prod\n Port 2222\nHost *\n Port 22\n IdentityAgent /tmp/agent".lines().map(|line| words(line).map(Directive::Setting)).collect::<Result<Vec<_>>>().unwrap();
        let config = resolve(&lines, "prod", Path::new("/tmp"), &mut Vec::new()).unwrap();
        assert_eq!(config.port, 22);
        assert_eq!(config.host, "127.0.0.2");
        assert_eq!(config.username, "operator");
        assert!(matches!(config.auth, AuthMethod::Agent { .. }));
    }

    #[test]
    fn unsupported_execution_and_cycles_are_rejected() {
        let lines = "Host target\n ProxyJump target\n User user"
            .lines()
            .map(|line| words(line).map(Directive::Setting))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(resolve(&lines, "target", Path::new("/tmp"), &mut Vec::new()).is_err());
        let lines = "Host *\n ProxyCommand command"
            .lines()
            .map(|line| words(line).map(Directive::Setting))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(resolve(&lines, "target", Path::new("/tmp"), &mut Vec::new()).is_err());
    }

    #[test]
    fn conditional_include_does_not_escape_host_scope() {
        let directory = tempfile::tempdir().unwrap();
        let included = directory.path().join("included");
        std::fs::write(
            &included,
            "Host *\n User included\nHost elsewhere\n Port 2200\n",
        )
        .unwrap();
        let root = directory.path().join("config");
        std::fs::write(
            &root,
            format!(
                "Host other\n Include {}\nHost target\n User actual\n Include {}\n Port 2222\n",
                included.display(),
                included.display()
            ),
        )
        .unwrap();
        let config = import_openssh(&root, "target").unwrap();
        assert_eq!(config.username, "actual");
        assert_eq!(config.port, 2222);
    }

    #[test]
    fn path_tokens_preserve_literal_percent_and_disabled_agent_is_explicit() {
        assert_eq!(
            expand_path(
                "~/key%%-%h-%r-%p",
                Path::new("/home/user"),
                "host",
                "alice",
                22
            )
            .unwrap(),
            Path::new("/home/user/key%-host-alice-22")
        );
        let lines = "Host *\n User alice\n IdentityAgent none"
            .lines()
            .map(|line| words(line).map(Directive::Setting))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(resolve(&lines, "host", Path::new("/home/user"), &mut Vec::new()).is_err());
    }
}
