use std::{fmt, str::FromStr};

/// USD 10^-12 단위. float와 DB typmod 반올림을 입력 경계로 사용하지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount(u128);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmountError;
impl FromStr for Amount {
    type Err = AmountError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (integer, fraction) = value.split_once('.').ok_or(AmountError)?;
        if integer.is_empty()
            || integer.len() > 16
            || (integer.len() > 1 && integer.starts_with('0'))
            || fraction.len() != 12
            || !integer
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return Err(AmountError);
        }
        let mantissa = integer.parse::<u128>().map_err(|_| AmountError)? * Self::SCALE
            + fraction.parse::<u128>().map_err(|_| AmountError)?;
        Ok(Self(mantissa))
    }
}
impl Amount {
    pub const ZERO: Self = Self(0);
    pub const SCALE: u128 = 1_000_000_000_000;
    pub const MAX: u128 = 9_999_999_999_999_999_999_999_999_999;
    pub fn checked_add(self, other: Self) -> Result<Self, AmountError> {
        let n = self.0.checked_add(other.0).ok_or(AmountError)?;
        if n > Self::MAX {
            return Err(AmountError);
        }
        Ok(Self(n))
    }
    /// 전체 항목의 곱을 checked 256bit 정수로 합산한 뒤 딱 한 번 반올림한다.
    pub fn price(
        items: impl IntoIterator<Item = (u64, Self)>,
        reservation: bool,
    ) -> Result<Self, AmountError> {
        let mut sum = Wide([0; 4]);
        for (tokens, rate) in items {
            sum.add(Wide::product(rate.0, tokens))?;
        }
        sum.add(Wide([if reservation { 999_999 } else { 500_000 }, 0, 0, 0]))?;
        let result = sum.divide(1_000_000);
        if result.0[2] != 0 || result.0[3] != 0 {
            return Err(AmountError);
        }
        let n = u128::from(result.0[0]) | (u128::from(result.0[1]) << 64);
        if n > Self::MAX {
            return Err(AmountError);
        }
        Ok(Self(n))
    }
}
// 외부 입력 크기와 무관한 고정 네 limb. 임의정밀도 할당/float 변환이 없다.
struct Wide([u64; 4]);
impl Wide {
    fn product(n: u128, factor: u64) -> Self {
        let low = u128::from(n as u64) * u128::from(factor);
        let high = (n >> 64) * u128::from(factor) + (low >> 64);
        Self([low as u64, high as u64, (high >> 64) as u64, 0])
    }
    fn add(&mut self, other: Self) -> Result<(), AmountError> {
        let mut carry = false;
        for (target, source) in self.0.iter_mut().zip(other.0) {
            let (n, a) = target.overflowing_add(source);
            let (n, b) = n.overflowing_add(u64::from(carry));
            *target = n;
            carry = a || b;
        }
        if carry { Err(AmountError) } else { Ok(()) }
    }
    fn divide(mut self, divisor: u64) -> Self {
        let mut remainder = 0u128;
        for word in self.0.iter_mut().rev() {
            let n = (remainder << 64) | u128::from(*word);
            *word = (n / u128::from(divisor)) as u64;
            remainder = n % u128::from(divisor);
        }
        self
    }
}
impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{:012}",
            self.0 / 1_000_000_000_000,
            self.0 % 1_000_000_000_000
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn amount_sums_before_half_up_or_reservation_ceil() {
        let tiny: Amount = "0.000000000001".parse().unwrap();
        assert_eq!(
            Amount::price([(250_000, tiny), (250_000, tiny)], false).unwrap(),
            tiny
        );
        assert_eq!(Amount::price([(1, tiny)], false).unwrap(), Amount::ZERO);
        assert_eq!(Amount::price([(1, tiny)], true).unwrap(), tiny);
        let max: Amount = "9999999999999999.999999999999".parse().unwrap();
        assert_eq!(Amount::price([(1_000_000, max)], false).unwrap(), max);
        assert!(Amount::price([(i64::MAX as u64, max)], false).is_err());
        assert!(max.checked_add(tiny).is_err());
        assert!(Wide([u64::MAX; 4]).add(Wide([1, 0, 0, 0])).is_err());
        // 192bit 곱의 큰 몫이 작은 값으로 잘려 수용되는 일이 없어야 한다.
        assert!(Amount::price([(u64::MAX, max); 64], false).is_err());
    }
    #[test]
    fn amount_rejects_noncanonical_or_rounded_inputs() {
        for bad in [
            "0",
            "1.0",
            "01.000000000000",
            "0.0000000000001",
            "-0.000000000001",
            "+1.000000000000",
            "1e0",
            "NaN",
            "Infinity",
            "10000000000000000.000000000000",
            " 1.000000000000",
        ] {
            assert!(
                bad.parse::<Amount>().is_err(),
                "noncanonical amount accepted: {bad}"
            );
        }
        for good in [
            "0.000000000000",
            "0.000000000001",
            "9999999999999999.999999999999",
        ] {
            assert_eq!(good.parse::<Amount>().unwrap().to_string(), good);
        }
    }
}
