use zeroize::Zeroize;

pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub const fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> std::fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let secret = Secret::new([7u8; 32]);
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(secret.expose()[0], 7);
    }
}
