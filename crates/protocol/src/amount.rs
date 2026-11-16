use core::{cmp::Ordering, fmt};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

const MAX: &str = "115792089237316195423570985008687907853269984665640564039457584007913129639935";

/// A uint256 represented by its canonical decimal string on the product wire.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Amount(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmountError {
    InvalidDecimal,
    OutOfRange,
    Overflow,
}

impl Amount {
    pub fn from_decimal_str(value: &str) -> Result<Self, AmountError> {
        if value.len() > MAX.len() {
            return Err(AmountError::OutOfRange);
        }
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(AmountError::InvalidDecimal);
        }
        if value.len() == MAX.len() && value > MAX {
            return Err(AmountError::OutOfRange);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn checked_add(&self, other: &Self) -> Result<Self, AmountError> {
        let mut left = self.0.bytes().rev();
        let mut right = other.0.bytes().rev();
        let mut carry = 0;
        let mut digits = Vec::with_capacity(MAX.len() + 1);
        loop {
            let a = left.next();
            let b = right.next();
            if a.is_none() && b.is_none() && carry == 0 {
                break;
            }
            let sum = a.map_or(0, |byte| byte - b'0') + b.map_or(0, |byte| byte - b'0') + carry;
            digits.push(b'0' + sum % 10);
            carry = sum / 10;
        }
        let total: String = digits.into_iter().rev().map(char::from).collect();
        if total.len() > MAX.len() || (total.len() == MAX.len() && total.as_str() > MAX) {
            return Err(AmountError::Overflow);
        }
        Ok(Self(total))
    }

    pub fn checked_sum_decimal<'a>(
        values: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, AmountError> {
        values.into_iter().try_fold(Self("0".into()), |sum, value| {
            sum.checked_add(&Self::from_decimal_str(value)?)
        })
    }
}

impl Ord for Amount {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for Amount {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for AmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidDecimal => "amount must be a canonical decimal string",
            Self::OutOfRange => "amount exceeds uint256 range",
            Self::Overflow => "amount sum exceeds uint256 range",
        })
    }
}

impl std::error::Error for AmountError {}

impl Serialize for Amount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AmountVisitor;
        impl de::Visitor<'_> for AmountVisitor {
            type Value = Amount;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical decimal uint256 string")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Amount, E> {
                Amount::from_decimal_str(value).map_err(E::custom)
            }
        }
        deserializer
            .deserialize_str(AmountVisitor)
            .map_err(|_| de::Error::custom("expected a canonical decimal uint256 string"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_decimal_parser_preserves_strings_and_rejects_invalid_input() {
        for value in [
            "0",
            "1",
            "10",
            "115792089237316195423570985008687907853269984665640564039457584007913129639935",
        ] {
            assert_eq!(Amount::from_decimal_str(value).unwrap().as_str(), value);
        }
        for value in [
            "", "00", "01", "+1", "-1", " 1", "1 ", "1.0", "1e6", "0x10", "１", "١",
        ] {
            assert_eq!(
                Amount::from_decimal_str(value),
                Err(AmountError::InvalidDecimal)
            );
        }
        for value in [
            "115792089237316195423570985008687907853269984665640564039457584007913129639936"
                .to_owned(),
            "1".repeat(79),
            "9".repeat(10_000),
            "0".repeat(10_000),
        ] {
            assert_eq!(
                Amount::from_decimal_str(&value),
                Err(AmountError::OutOfRange)
            );
        }
    }

    #[test]
    fn numeric_order_handles_different_lengths_and_uint256_values() {
        let values = [
            "0",
            "1",
            "2",
            "9",
            "10",
            "99",
            "100",
            "100000000000000000000000000000000000000000000000000000000000000000000000000000",
            "110000000000000000000000000000000000000000000000000000000000000000000000000000",
            "115792089237316195423570985008687907853269984665640564039457584007913129639934",
            "115792089237316195423570985008687907853269984665640564039457584007913129639935",
        ];
        for (index, value) in values.iter().enumerate() {
            let amount = Amount::from_decimal_str(value).unwrap();
            assert_eq!(amount.cmp(&amount), Ordering::Equal);
            for larger in &values[index + 1..] {
                let larger = Amount::from_decimal_str(larger).unwrap();
                assert_eq!(amount.cmp(&larger), Ordering::Less);
                assert_eq!(larger.partial_cmp(&amount), Some(Ordering::Greater));
            }
        }
    }

    #[test]
    fn checked_arithmetic_carries_and_rejects_overflow() {
        for (values, expected) in [
            (vec![], "0"),
            (vec!["0", "0"], "0"),
            (vec!["9", "1"], "10"),
            (vec!["99", "1"], "100"),
            (
                vec!["340282366920938463463374607431768211455", "1"],
                "340282366920938463463374607431768211456",
            ),
            (
                vec![
                    "115792089237316195423570985008687907853269984665640564039457584007913129639935",
                    "0",
                ],
                "115792089237316195423570985008687907853269984665640564039457584007913129639935",
            ),
        ] {
            assert_eq!(
                Amount::checked_sum_decimal(values).unwrap().as_str(),
                expected
            );
        }
        assert_eq!(
            Amount::checked_sum_decimal([MAX, "1"]),
            Err(AmountError::Overflow)
        );
        assert_eq!(
            Amount::checked_sum_decimal([MAX, "1", "00"]),
            Err(AmountError::Overflow)
        );
        assert_eq!(
            Amount::checked_sum_decimal(["1", "00"]),
            Err(AmountError::InvalidDecimal)
        );
        assert_eq!(
            Amount::checked_sum_decimal([
                "1",
                "115792089237316195423570985008687907853269984665640564039457584007913129639936"
            ]),
            Err(AmountError::OutOfRange)
        );
    }

    #[test]
    fn serde_uses_decimal_strings_and_errors_do_not_expose_input() {
        assert_eq!(
            serde_json::to_string(&Amount::from_decimal_str("10").unwrap()).unwrap(),
            "\"10\""
        );
        assert_eq!(
            serde_json::from_str::<Amount>("\"10\"").unwrap().as_str(),
            "10"
        );
        for value in [
            "1",
            "null",
            "[]",
            "{}",
            "\"0x1\"",
            "\"00\"",
            "\"01\"",
            "\"\"",
            "\"115792089237316195423570985008687907853269984665640564039457584007913129639936\"",
        ] {
            assert!(serde_json::from_str::<Amount>(value).is_err());
        }
        for (wire, original) in [
            ("\"private-file-name\"", "private-file-name"),
            ("987654321", "987654321"),
        ] {
            let error = serde_json::from_str::<Amount>(wire)
                .unwrap_err()
                .to_string();
            assert!(error.contains("canonical decimal"));
            assert!(!error.contains(original));
        }
    }
}
