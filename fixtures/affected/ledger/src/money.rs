use std::fmt;

pub struct Money(pub u64);

impl Money {
    pub fn with_vat(&self) -> Money {
        Money(self.0 + crate::tax::vat(self.0))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{:02}", self.0 / 100, self.0 % 100)
    }
}

#[cfg(test)]
mod tests {
    use super::Money;

    #[test]
    fn displays_cents() {
        assert_eq!(Money(1234).to_string(), "12.34");
    }

    #[test]
    fn adds_vat() {
        assert_eq!(Money(1000).with_vat().0, 1200);
    }
}
