//! `pg_hba.conf`-style host-based authentication rules.
//!
//! When `PGRS_HBA` (a file path) or `PGRS_HBA_RULES` (inline rules) is set, the
//! server matches each incoming connection against an ordered list of rules and
//! applies the first one that matches by database, user, and client address.
//! When neither env var is set, the server keeps its prior behavior (selected by
//! `PGRS_AUTH_METHOD` / SCRAM-if-`PGRS_PASSWORD` / trust).
//!
//! Each rule line is `<type> <database> <user> <address> <method>`, e.g.
//! `host all all 127.0.0.1 scram-sha-256`. The address column is omitted for
//! `local` lines. Blank lines and `#` comments are ignored.
//!
//! Matching support is intentionally minimal but standard-ish: `all` wildcards,
//! exact database/user/IP matches, the `samehost`/`localhost` keywords, and an
//! `<ip>/<prefixlen>` CIDR form (IPv4). This is enough to express the common
//! "trust localhost, require a password elsewhere" policy.

/// The connection type column of an HBA rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HbaType {
    /// `local` — a Unix-domain socket connection (no address column).
    Local,
    /// `host` — a TCP connection (TLS optional).
    Host,
    /// `hostssl` — a TCP connection that must use TLS.
    HostSsl,
    /// `hostnossl` — a TCP connection that must not use TLS.
    HostNoSsl,
}

/// The authentication method an HBA rule selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HbaMethod {
    Trust,
    Reject,
    Password,
    Md5,
    ScramSha256,
}

impl HbaMethod {
    fn parse(s: &str) -> Option<HbaMethod> {
        match s.to_ascii_lowercase().as_str() {
            "trust" => Some(HbaMethod::Trust),
            "reject" => Some(HbaMethod::Reject),
            "password" => Some(HbaMethod::Password),
            "md5" => Some(HbaMethod::Md5),
            "scram-sha-256" => Some(HbaMethod::ScramSha256),
            _ => None,
        }
    }
}

/// The address column of an HBA rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HbaAddress {
    /// No address (a `local` rule), or `all` — matches any peer.
    Any,
    /// An exact IP address (text form, e.g. `127.0.0.1`).
    Exact(String),
    /// `samehost`/`localhost` — matches loopback peers.
    Loopback,
    /// IPv4 CIDR `base/prefixlen` (e.g. `10.0.0.0/8`).
    Cidr { base: [u8; 4], prefix: u8 },
}

impl HbaAddress {
    fn parse(s: &str) -> Option<HbaAddress> {
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "all" => return Some(HbaAddress::Any),
            "samehost" | "localhost" | "samenet" => return Some(HbaAddress::Loopback),
            _ => {}
        }
        if let Some((base, prefix)) = s.split_once('/') {
            let octets = parse_ipv4(base)?;
            let prefix: u8 = prefix.parse().ok()?;
            if prefix > 32 {
                return None;
            }
            return Some(HbaAddress::Cidr {
                base: octets,
                prefix,
            });
        }
        // Treat anything else as an exact (textual) address.
        Some(HbaAddress::Exact(s.to_string()))
    }

    /// Whether this address column matches `peer_ip` (the textual client IP).
    fn matches(&self, peer_ip: &str) -> bool {
        match self {
            HbaAddress::Any => true,
            HbaAddress::Exact(addr) => addr == peer_ip,
            HbaAddress::Loopback => is_loopback(peer_ip),
            HbaAddress::Cidr { base, prefix } => match parse_ipv4(peer_ip) {
                Some(ip) => ipv4_in_cidr(ip, *base, *prefix),
                None => false,
            },
        }
    }
}

/// A single parsed HBA rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbaRule {
    pub conn_type: HbaType,
    /// Database column: `None` means `all`; otherwise an exact database name.
    pub database: Option<String>,
    /// User column: `None` means `all`; otherwise an exact role name.
    pub user: Option<String>,
    pub address: HbaAddress,
    pub method: HbaMethod,
}

impl HbaRule {
    /// Whether this rule matches a connection from `user`/`database` at
    /// `peer_ip`. A `local` rule only matches local (no-TCP) connections, which
    /// we approximate by matching loopback peers as well.
    pub fn matches(&self, database: &str, user: &str, peer_ip: &str) -> bool {
        let db_ok = self.database.as_deref().is_none_or(|d| d == database);
        let user_ok = self.user.as_deref().is_none_or(|u| u == user);
        if !db_ok || !user_ok {
            return false;
        }
        match self.conn_type {
            HbaType::Local => is_loopback(peer_ip),
            HbaType::Host | HbaType::HostSsl | HbaType::HostNoSsl => self.address.matches(peer_ip),
        }
    }
}

/// An ordered set of HBA rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HbaConfig {
    pub rules: Vec<HbaRule>,
}

impl HbaConfig {
    /// Parse rules from text (the contents of a `pg_hba.conf`-style file or the
    /// inline `PGRS_HBA_RULES` value). Blank lines and `#` comments are skipped.
    /// Lines that don't parse are skipped (lenient, like a tolerant loader).
    pub fn parse(text: &str) -> HbaConfig {
        let mut rules = Vec::new();
        for raw in text.lines() {
            // Strip comments and surrounding whitespace.
            let line = match raw.find('#') {
                Some(i) => &raw[..i],
                None => raw,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rule) = parse_rule_line(line) {
                rules.push(rule);
            }
        }
        HbaConfig { rules }
    }

    /// The method of the first rule that matches the connection, or `None` when
    /// no rule matches (the caller decides the fallthrough behavior — PostgreSQL
    /// rejects, but see the server for the policy chosen here).
    pub fn match_method(
        &self,
        database: &str,
        user: &str,
        peer_ip: &str,
    ) -> Option<&HbaMethod> {
        self.rules
            .iter()
            .find(|r| r.matches(database, user, peer_ip))
            .map(|r| &r.method)
    }
}

/// Parse one non-empty, comment-free rule line into an [`HbaRule`].
fn parse_rule_line(line: &str) -> Option<HbaRule> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let conn_type = match fields.first()?.to_ascii_lowercase().as_str() {
        "local" => HbaType::Local,
        "host" => HbaType::Host,
        "hostssl" => HbaType::HostSsl,
        "hostnossl" => HbaType::HostNoSsl,
        _ => return None,
    };

    let wildcard = |s: &str| -> Option<String> {
        if s.eq_ignore_ascii_case("all") {
            None
        } else {
            Some(s.to_string())
        }
    };

    if conn_type == HbaType::Local {
        // `local <database> <user> <method>` (no address column).
        if fields.len() < 4 {
            return None;
        }
        let database = wildcard(fields[1]);
        let user = wildcard(fields[2]);
        let method = HbaMethod::parse(fields[3])?;
        return Some(HbaRule {
            conn_type,
            database,
            user,
            address: HbaAddress::Any,
            method,
        });
    }

    // `host <database> <user> <address> <method>`.
    if fields.len() < 5 {
        return None;
    }
    let database = wildcard(fields[1]);
    let user = wildcard(fields[2]);
    let address = HbaAddress::parse(fields[3])?;
    let method = HbaMethod::parse(fields[4])?;
    Some(HbaRule {
        conn_type,
        database,
        user,
        address,
        method,
    })
}

/// Parse a dotted-quad IPv4 address into its four octets.
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        octets[i] = part.parse().ok()?;
    }
    Some(octets)
}

/// Whether `ip` (octets) falls within the CIDR `base/prefix`.
fn ipv4_in_cidr(ip: [u8; 4], base: [u8; 4], prefix: u8) -> bool {
    let ip = u32::from_be_bytes(ip);
    let base = u32::from_be_bytes(base);
    if prefix == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - prefix as u32);
    (ip & mask) == (base & mask)
}

/// Whether a textual peer address is an IPv4/IPv6 loopback (or empty, which we
/// treat as a local connection).
fn is_loopback(peer_ip: &str) -> bool {
    peer_ip.is_empty()
        || peer_ip == "127.0.0.1"
        || peer_ip == "::1"
        || peer_ip.starts_with("127.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_lines() {
        let cfg = HbaConfig::parse(
            "# comment line\n\
             local all all trust\n\
             host all all 127.0.0.1 scram-sha-256\n\
             host mydb alice 10.0.0.0/8 md5\n\
             host all all all reject  # trailing comment\n",
        );
        assert_eq!(cfg.rules.len(), 4);
        assert_eq!(cfg.rules[0].conn_type, HbaType::Local);
        assert_eq!(cfg.rules[0].method, HbaMethod::Trust);
        assert_eq!(cfg.rules[1].address, HbaAddress::Exact("127.0.0.1".into()));
        assert_eq!(cfg.rules[1].method, HbaMethod::ScramSha256);
        assert_eq!(cfg.rules[2].database.as_deref(), Some("mydb"));
        assert_eq!(cfg.rules[2].user.as_deref(), Some("alice"));
        assert_eq!(cfg.rules[3].method, HbaMethod::Reject);
    }

    #[test]
    fn matches_by_user_db_address() {
        let cfg = HbaConfig::parse(
            "host mydb alice 127.0.0.1 md5\n\
             host all all 127.0.0.1 scram-sha-256\n",
        );
        // Specific rule wins for alice@mydb.
        assert_eq!(
            cfg.match_method("mydb", "alice", "127.0.0.1"),
            Some(&HbaMethod::Md5)
        );
        // Falls through to the catch-all for a different user.
        assert_eq!(
            cfg.match_method("mydb", "bob", "127.0.0.1"),
            Some(&HbaMethod::ScramSha256)
        );
        // Falls through to the catch-all for a different db.
        assert_eq!(
            cfg.match_method("otherdb", "alice", "127.0.0.1"),
            Some(&HbaMethod::ScramSha256)
        );
    }

    #[test]
    fn reject_rule_and_fallthrough() {
        let cfg = HbaConfig::parse("host all all 192.168.1.0/24 reject\n");
        // In-range address matches the reject rule.
        assert_eq!(
            cfg.match_method("db", "u", "192.168.1.50"),
            Some(&HbaMethod::Reject)
        );
        // Out-of-range address matches no rule (fallthrough → None).
        assert_eq!(cfg.match_method("db", "u", "10.0.0.1"), None);
    }

    #[test]
    fn cidr_matching() {
        let cfg = HbaConfig::parse("host all all 10.0.0.0/8 trust\n");
        assert_eq!(
            cfg.match_method("db", "u", "10.255.1.2"),
            Some(&HbaMethod::Trust)
        );
        assert_eq!(cfg.match_method("db", "u", "11.0.0.1"), None);
    }

    #[test]
    fn local_rule_matches_loopback_only() {
        let cfg = HbaConfig::parse("local all all trust\n");
        assert_eq!(cfg.match_method("db", "u", "127.0.0.1"), Some(&HbaMethod::Trust));
        assert_eq!(cfg.match_method("db", "u", ""), Some(&HbaMethod::Trust));
        assert_eq!(cfg.match_method("db", "u", "8.8.8.8"), None);
    }

    #[test]
    fn samehost_keyword() {
        let cfg = HbaConfig::parse("host all all samehost password\n");
        assert_eq!(
            cfg.match_method("db", "u", "127.0.0.1"),
            Some(&HbaMethod::Password)
        );
        assert_eq!(cfg.match_method("db", "u", "203.0.113.7"), None);
    }
}
