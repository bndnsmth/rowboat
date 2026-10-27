//! Credential-free connection guidance. Raw driver/SSH text stays out of UI reports.
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStage {
    Ssh,
    Database,
    File,
    Catalog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionIssue {
    Authentication,
    Refused,
    Dns,
    Timeout,
    Tls,
    DatabaseUnavailable,
    SshTrust,
    SshAuthentication,
    SshForwarding,
    SshUnavailable,
    Ssh,
    File,
    Catalog,
    Unknown,
}

impl fmt::Display for ConnectionIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Authentication => "Authentication failed. Check the database username/password and whether that account is allowed to connect.",
            Self::Refused => "Connection refused. Check the host and port, and that the database server is listening and reachable from this machine (or the SSH host).",
            Self::Dns => "Host could not be resolved. Check the hostname and your DNS/VPN connection.",
            Self::Timeout => "Connection timed out. Check reachability, firewall/VPN settings and the database or SSH port.",
            Self::Tls => "TLS negotiation or certificate verification failed. Check the server hostname, CA certificate, certificate expiry and any required client certificate/key. Keep certificate verification enabled.",
            Self::DatabaseUnavailable => "The database is unavailable to this account. Check its name, CONNECT permission and whether it accepts connections.",
            Self::SshTrust => "SSH host verification failed. Verify the server's host-key fingerprint through a trusted channel and review your known_hosts entry. Rowboat will not bypass host verification.",
            Self::SshAuthentication => "SSH authentication failed. Check the SSH username, loaded agent keys and configured identity file. Encrypted keys must be available through your SSH agent.",
            Self::SshForwarding => "SSH forwarding failed. Check that the SSH server permits forwarding and can reach the database host and port.",
            Self::SshUnavailable => "The SSH client or identity file is unavailable. Check that OpenSSH is installed and the configured identity file exists and is readable.",
            Self::Ssh => "The SSH tunnel could not be established. Check the SSH host/port, trusted host key and agent or identity file.",
            Self::File => "The SQLite file could not be opened. Check the path, file permissions and that it is a valid SQLite database.",
            Self::Catalog => "Connected, but the database/schema catalog could not be loaded. Check metadata permissions and server compatibility; refresh or retry after correcting access.",
            Self::Unknown => "The database connection failed. Check the endpoint, account access and TLS settings. Server-side connection logs may provide more detail; avoid sharing credentials or raw connection URLs.",
        })
    }
}
impl std::error::Error for ConnectionIssue {}

pub fn connection_issue(stage: ConnectionStage, error: &anyhow::Error) -> ConnectionIssue {
    if let Some(issue) = error.downcast_ref::<ConnectionIssue>() {
        return *issue;
    }
    let text = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let contains = |patterns: &[&str]| patterns.iter().any(|p| text.contains(p));
    if stage == ConnectionStage::Catalog {
        return ConnectionIssue::Catalog;
    }
    if stage == ConnectionStage::File {
        return ConnectionIssue::File;
    }
    if stage == ConnectionStage::Ssh {
        if contains(&[
            "cannot start ssh",
            "identity is unavailable",
            "identity must be",
        ]) {
            return ConnectionIssue::SshUnavailable;
        }
        if contains(&[
            "host key verification failed",
            "host identification has changed",
            "open configured ssh known-hosts file",
        ]) {
            return ConnectionIssue::SshTrust;
        }
        if contains(&[
            "permission denied",
            "authentication failed",
            "no identities",
            "sign_and_send_pubkey",
        ]) {
            return ConnectionIssue::SshAuthentication;
        }
        if contains(&["forward", "administratively prohibited"]) {
            return ConnectionIssue::SshForwarding;
        }
    } else {
        if contains(&[
            "password authentication failed",
            "authentication failed",
            "access denied for user",
            "no pg_hba.conf entry",
        ]) {
            return ConnectionIssue::Authentication;
        }
        if contains(&[
            "certificate",
            "tls error",
            "tls handshake",
            "tls negotiation",
            "ssl error",
            "ssl connection",
            "server does not support ssl",
        ]) {
            return ConnectionIssue::Tls;
        }
        if contains(&[
            "unknown database",
            "does not exist",
            "permission denied for database",
            "not currently accepting connections",
        ]) {
            return ConnectionIssue::DatabaseUnavailable;
        }
    }
    if contains(&["connection refused"]) {
        ConnectionIssue::Refused
    } else if contains(&[
        "failed to lookup address",
        "name or service not known",
        "nodename nor servname",
        "could not resolve",
        "temporary failure in name resolution",
    ]) {
        ConnectionIssue::Dns
    } else if contains(&["timed out", "timeout"]) {
        ConnectionIssue::Timeout
    } else if stage == ConnectionStage::Ssh {
        ConnectionIssue::Ssh
    } else {
        ConnectionIssue::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_guidance_classifies_without_echoing_untrusted_details() {
        for (stage, text, expected) in [
            (
                ConnectionStage::Database,
                "password authentication failed for user",
                ConnectionIssue::Authentication,
            ),
            (
                ConnectionStage::Database,
                "Access denied for user",
                ConnectionIssue::Authentication,
            ),
            (
                ConnectionStage::Database,
                "invalid peer certificate: UnknownIssuer",
                ConnectionIssue::Tls,
            ),
            (
                ConnectionStage::Database,
                "Connection refused (os error 111)",
                ConnectionIssue::Refused,
            ),
            (
                ConnectionStage::Database,
                "failed to lookup address",
                ConnectionIssue::Dns,
            ),
            (
                ConnectionStage::Database,
                "PostgreSQL connection timed out",
                ConnectionIssue::Timeout,
            ),
            (
                ConnectionStage::Database,
                "Unknown database",
                ConnectionIssue::DatabaseUnavailable,
            ),
            (
                ConnectionStage::Ssh,
                "Host key verification failed",
                ConnectionIssue::SshTrust,
            ),
            (
                ConnectionStage::Ssh,
                "Permission denied (publickey)",
                ConnectionIssue::SshAuthentication,
            ),
            (
                ConnectionStage::Ssh,
                "SSH exited; verify the existing known-hosts entry, username, and agent/identity. Permission denied (publickey)",
                ConnectionIssue::SshAuthentication,
            ),
            (
                ConnectionStage::Ssh,
                "Configured SSH identity is unavailable: permission denied",
                ConnectionIssue::SshUnavailable,
            ),
            (
                ConnectionStage::Ssh,
                "SSH could not establish forwarding socket",
                ConnectionIssue::SshForwarding,
            ),
            (
                ConnectionStage::Ssh,
                "Cannot start ssh",
                ConnectionIssue::SshUnavailable,
            ),
            (
                ConnectionStage::File,
                "unable to open database",
                ConnectionIssue::File,
            ),
            (
                ConnectionStage::Catalog,
                "permission denied",
                ConnectionIssue::Catalog,
            ),
            (
                ConnectionStage::Database,
                "unclassified failure",
                ConnectionIssue::Unknown,
            ),
        ] {
            let error =
                anyhow::anyhow!("{text}: private-password postgresql://user:secret@host/db");
            let issue = connection_issue(stage, &error);
            assert_eq!(issue, expected);
            let message = issue.to_string();
            assert!(!message.contains("private-password") && !message.contains("secret@"));
        }
    }
}
