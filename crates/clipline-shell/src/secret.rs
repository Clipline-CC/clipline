//! Move-only, redacted, zeroizing secret text ownership.

use std::fmt;

use zeroize::Zeroizing;

/// A UTF-8 secret whose allocation is zeroized when ownership ends.
///
/// This type deliberately implements neither `Clone` nor Serde traits. Domain
/// crates wrap it in purpose-specific owners and expose borrowed text only at
/// audited credential/HTTP boundaries.
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub(crate) fn from_zeroizing(value: Zeroizing<String>) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_text_is_borrowable_but_redacted() {
        let secret = SecretString::new("do-not-print".into());
        assert_eq!(secret.expose_secret(), "do-not-print");
        assert_eq!(format!("{secret:?}"), "SecretString([REDACTED])");
    }
}
