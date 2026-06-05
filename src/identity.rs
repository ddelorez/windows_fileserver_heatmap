//! Principal classification.
//!
//! The durable contamination filter (per the design): heat should reflect human
//! demand, so automated accessors — machine accounts, service accounts, system
//! principals — are tagged here and can be excluded from scoring downstream.
//!
//! This module is pure and unit-tested; it has no ETW dependency.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalClass {
    Human,
    Machine, // computer accounts (SAM name ends in '$')
    Service, // LOCAL/NETWORK SERVICE, plus any configured service accounts
    System,  // SYSTEM / well-known system SIDs / ANONYMOUS
    Unknown, // couldn't resolve a name — treated as observed-but-unclassified
}

impl PrincipalClass {
    /// True for principals we'd normally exclude from heat scoring.
    /// `Unknown` is deliberately *not* automation: we'd rather over-count a
    /// genuine user than silently drop activity we failed to classify.
    pub fn is_automation(self) -> bool {
        matches!(self, PrincipalClass::Machine | PrincipalClass::Service | PrincipalClass::System)
    }
}

/// Classify an account string. Accepts `DOMAIN\name`, bare `name`, or a SID
/// string (`S-1-5-...`). Service-account detection beyond the built-in
/// well-knowns is intentionally config-driven in the real agent (a site has its
/// own backup/monitoring accounts that look human); the spike keeps just the
/// universal rules.
pub fn classify(account: &str) -> PrincipalClass {
    if account.is_empty() || account == "<unknown>" {
        return PrincipalClass::Unknown;
    }

    // SID-based well-knowns (most reliable when present).
    if account.starts_with("S-1-5-18") {
        return PrincipalClass::System; // Local System
    }
    if account.starts_with("S-1-5-19") || account.starts_with("S-1-5-20") {
        return PrincipalClass::Service; // Local Service / Network Service
    }
    if account.starts_with("S-1-5-7") {
        return PrincipalClass::System; // Anonymous Logon
    }

    let bare = account.rsplit('\\').next().unwrap_or(account);
    let upper = bare.to_ascii_uppercase();

    match upper.as_str() {
        "SYSTEM" | "ANONYMOUS LOGON" => return PrincipalClass::System,
        "LOCAL SERVICE" | "NETWORK SERVICE" => return PrincipalClass::Service,
        _ => {}
    }

    // Computer accounts present as DOMAIN\HOSTNAME$ over SMB.
    if bare.ends_with('$') {
        return PrincipalClass::Machine;
    }

    PrincipalClass::Human
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humans() {
        assert_eq!(classify("CONTOSO\\alice"), PrincipalClass::Human);
        assert_eq!(classify("bob"), PrincipalClass::Human);
    }

    #[test]
    fn machines() {
        assert_eq!(classify("CONTOSO\\SGIFS01$"), PrincipalClass::Machine);
        assert_eq!(classify("WORKSTATION12$"), PrincipalClass::Machine);
        assert!(classify("CONTOSO\\SGIFS01$").is_automation());
    }

    #[test]
    fn system_and_service() {
        assert_eq!(classify("NT AUTHORITY\\SYSTEM"), PrincipalClass::System);
        assert_eq!(classify("LOCAL SERVICE"), PrincipalClass::Service);
        assert_eq!(classify("S-1-5-18"), PrincipalClass::System);
        assert_eq!(classify("S-1-5-20"), PrincipalClass::Service);
    }

    #[test]
    fn unknown_is_not_automation() {
        assert_eq!(classify(""), PrincipalClass::Unknown);
        assert!(!classify("").is_automation());
    }
}
