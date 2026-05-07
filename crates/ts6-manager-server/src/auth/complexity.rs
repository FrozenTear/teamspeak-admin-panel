//! Spec §6.2.2 — password complexity rules. Each rule has a verbatim error
//! string the API surface returns on HTTP 400.

/// Single rule violation. The `Display` text is the spec-mandated message
/// returned on the HTTP 400 response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    TooShort,
    NoUppercase,
    NoLowercase,
    NoDigit,
    NoSpecial,
}

impl Rule {
    /// Spec-verbatim error string.
    pub fn message(self) -> &'static str {
        match self {
            Rule::TooShort => "Password must be at least 8 characters long",
            Rule::NoUppercase => "Password must contain at least one uppercase letter",
            Rule::NoLowercase => "Password must contain at least one lowercase letter",
            Rule::NoDigit => "Password must contain at least one digit",
            Rule::NoSpecial => "Password must contain at least one special character",
        }
    }
}

/// Spec §6.2.2: special-character set permitted by the rule.
const SPECIAL_CHARS: &str = "!@#$%^&*()_+-=[]{}|;':\",./<>?";

/// Validate `password` against every rule. Returns the full list of violations
/// in spec order — callers that want a single error message should use the
/// first element.
pub fn violations(password: &str) -> Vec<Rule> {
    let mut out = Vec::new();
    if password.chars().count() < 8 {
        out.push(Rule::TooShort);
    }
    if !password.chars().any(|c| c.is_ascii_uppercase()) {
        out.push(Rule::NoUppercase);
    }
    if !password.chars().any(|c| c.is_ascii_lowercase()) {
        out.push(Rule::NoLowercase);
    }
    if !password.chars().any(|c| c.is_ascii_digit()) {
        out.push(Rule::NoDigit);
    }
    if !password.chars().any(|c| SPECIAL_CHARS.contains(c)) {
        out.push(Rule::NoSpecial);
    }
    out
}

/// Convenience: `Ok(())` if `password` satisfies every rule, else `Err(first_violation)`.
pub fn validate(password: &str) -> Result<(), Rule> {
    match violations(password).first() {
        Some(r) => Err(*r),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_compliant_password() {
        assert_eq!(validate("Hunter2!ok"), Ok(()));
        assert_eq!(validate("Sw3et;Days"), Ok(()));
    }

    #[test]
    fn rejects_too_short() {
        assert_eq!(validate("Aa1!"), Err(Rule::TooShort));
        assert_eq!(violations("Aa1!"), vec![Rule::TooShort]);
    }

    #[test]
    fn rejects_no_uppercase() {
        // "all lowercase 1!" length is fine, has digit + special, missing
        // uppercase only.
        assert_eq!(validate("password1!"), Err(Rule::NoUppercase));
    }

    #[test]
    fn rejects_no_lowercase() {
        assert_eq!(validate("PASSWORD1!"), Err(Rule::NoLowercase));
    }

    #[test]
    fn rejects_no_digit() {
        assert_eq!(validate("Password!!"), Err(Rule::NoDigit));
    }

    #[test]
    fn rejects_no_special() {
        assert_eq!(validate("Password11"), Err(Rule::NoSpecial));
    }

    #[test]
    fn returns_violations_in_spec_order() {
        // 4-char password with no upper, no digit, no special — TooShort
        // appears first, then NoUppercase, then NoDigit, then NoSpecial.
        let v = violations("abcd");
        assert_eq!(v[0], Rule::TooShort);
        assert!(v.contains(&Rule::NoUppercase));
        assert!(v.contains(&Rule::NoDigit));
        assert!(v.contains(&Rule::NoSpecial));
    }

    #[test]
    fn special_character_set_matches_spec() {
        // Per §6.2.2 special set. Build a length-8 password with one of each
        // mandatory class plus the candidate special, padded with lowercase.
        for ch in "!@#$%^&*()_+-=[]{}|;':\",./<>?".chars() {
            let pw = format!("Aa1{ch}aaaa"); // 3 + 1 + 4 = 8 chars
            assert_eq!(
                validate(&pw),
                Ok(()),
                "special char {ch:?} should satisfy NoSpecial rule (pw={pw:?})"
            );
        }
    }

    #[test]
    fn whitespace_is_not_a_special_character() {
        // Space, tab, newline don't count as special per the spec set.
        assert_eq!(validate("Aa1 aaaa"), Err(Rule::NoSpecial));
        assert_eq!(validate("Aa1\taaaa"), Err(Rule::NoSpecial));
    }

    #[test]
    fn message_text_matches_spec() {
        // Spec §6.2.2 wording: "Password must contain at least one uppercase letter".
        assert_eq!(
            Rule::NoUppercase.message(),
            "Password must contain at least one uppercase letter"
        );
        assert_eq!(
            Rule::NoLowercase.message(),
            "Password must contain at least one lowercase letter"
        );
        assert_eq!(
            Rule::NoDigit.message(),
            "Password must contain at least one digit"
        );
    }
}
